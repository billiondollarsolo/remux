use std::io::{Read as StdRead, Write as StdWrite};

use remux_core::framing::{read_message, write_message};
use remux_core::terminal::TerminalSnapshot;
use remux_core::{
    AttachMode, ClientId, Event, RemuxError, Request, Response, SessionSelector, TermSize,
};
use tokio::io::BufReader;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::task;

use crate::client::RemuxClient;
use crate::raw_mode::get_terminal_size;

/// Handle the `attach` command.
///
/// `read_only` attaches as an [`AttachMode::Observer`]: output is still
/// rendered and the detach prefix + resize handling stay active, but stdin is
/// never forwarded to the PTY (the daemon enforces this for observers anyway).
pub async fn run(
    mut client: RemuxClient,
    name: String,
    detach_key: &str,
    read_only: bool,
) -> Result<(), RemuxError> {
    let session = parse_selector(&name);
    let size: TermSize = get_terminal_size();
    let client_id = ClientId::new();
    let prefix_byte = parse_detach_byte(detach_key);
    let prefix_label = human_prefix_name(detach_key);

    let mode = if read_only {
        AttachMode::Observer
    } else {
        AttachMode::Control
    };

    // Send attach request.
    let response = client
        .send_request(Request::AttachSession {
            session: session.clone(),
            size,
            mode,
            client_id: client_id.clone(),
        })
        .await?;

    let bootstrap = match response {
        Response::Attached(bootstrap) => bootstrap,
        Response::Error(e) => return Err(e),
        other => {
            return Err(RemuxError::ProtocolError(format!(
                "unexpected response: {other:?}"
            )));
        }
    };

    let session_name = bootstrap.session.name.clone();

    // Track the most recently rendered snapshot so the redraw command
    // (prefix + l) can repaint without a round-trip to the daemon.
    let mut last_snapshot: Option<TerminalSnapshot> = None;

    // Enter raw terminal mode.
    enable_raw_mode().map_err(|e| RemuxError::IoError(format!("failed to enter raw mode: {e}")))?;

    // Guard to ensure we always exit raw mode.
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    let _guard = RawModeGuard;

    // One-line detach hint so the user knows how to get out.
    eprint!("[detached with {prefix_label}-d]\r\n");

    // Write any scrollback data to stdout (history first), then repaint the
    // current screen from the VT snapshot on top of it.
    if !bootstrap.scrollback.is_empty() {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle
            .write_all(&bootstrap.scrollback)
            .map_err(|e| RemuxError::IoError(format!("scrollback write error: {e}")))?;
        handle
            .flush()
            .map_err(|e| RemuxError::IoError(format!("flush error: {e}")))?;
    }

    // Repaint the visible screen from the parsed VT snapshot so TUIs (vim,
    // htop, less) and colors come back faithfully on reattach.
    if let Some(snapshot) = bootstrap.vt_snapshot {
        let painted = crate::render_snapshot::paint_snapshot(&snapshot);
        write_to_stdout(&painted)?;
        last_snapshot = Some(snapshot);
    }

    // Split the UnixStream for concurrent reading and writing.
    let (read_half, write_half) = client.split();
    let mut daemon_reader = BufReader::new(read_half);
    let mut daemon_writer = write_half;

    // Spawn a blocking stdin reader. It forwards raw byte chunks over an mpsc
    // channel so the async loop can run them through the prefix state machine
    // and relay the rest verbatim to the PTY.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        // Receiver dropped; the attach loop has exited.
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // SIGWINCH handler: resize is handled out-of-band, not via the byte stream.
    let mut sigwinch = signal(SignalKind::window_change())
        .map_err(|e| RemuxError::IoError(format!("failed to install SIGWINCH handler: {e}")))?;

    let session_for_input = session.clone();
    let session_for_detach = session.clone();
    let session_for_resize = session.clone();
    let mut detached = false;

    // Stateful prefix machine, carried across stdin chunks so a prefix landing
    // at the end of one read still composes with the command byte in the next.
    let mut prefix = PrefixMachine::new(prefix_byte);

    // Line buffer for reading daemon events in JSON mode.
    let mut event_line = Vec::new();

    loop {
        tokio::select! {
            // Handle raw stdin bytes: feed each through the prefix machine.
            chunk = stdin_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        let mut forward: Vec<u8> = Vec::new();
                        let mut do_detach = false;
                        let mut do_redraw = false;
                        for &b in &bytes {
                            match prefix.feed(b) {
                                PrefixAction::Forward(out) => forward.extend_from_slice(&out),
                                PrefixAction::Pending => {}
                                PrefixAction::Detach => do_detach = true,
                                PrefixAction::Redraw => do_redraw = true,
                            }
                        }

                        // In read-only mode we never forward input to the PTY;
                        // the prefix machine still runs so detach/redraw work.
                        if !read_only && !forward.is_empty() {
                            let send_req = Request::SendInput {
                                session: session_for_input.clone(),
                                data: forward,
                            };
                            write_message(&mut daemon_writer, &send_req).await?;
                        }

                        if do_redraw {
                            if let Some(ref snap) = last_snapshot {
                                let painted = crate::render_snapshot::paint_snapshot(snap);
                                let _ = write_to_stdout(&painted);
                            }
                        }

                        if do_detach {
                            let detach_req = Request::DetachSession {
                                session: session_for_detach.clone(),
                                client_id: client_id.clone(),
                            };
                            let _ = write_message(&mut daemon_writer, &detach_req).await;
                            detached = true;
                            break;
                        }
                    }
                    None => {
                        // stdin closed (EOF). Nothing more to forward.
                        break;
                    }
                }
            }
            // Handle terminal resize out-of-band via SIGWINCH.
            _ = sigwinch.recv() => {
                let new_size = get_terminal_size();
                let resize_req = Request::ResizeSession {
                    session: session_for_resize.clone(),
                    size: new_size,
                    client_id: client_id.clone(),
                };
                let _ = write_message(&mut daemon_writer, &resize_req).await;
            }
            // Handle daemon events (output from the session)
            event_result = read_message::<Event>(&mut daemon_reader, &mut event_line) => {
                match event_result {
                    Ok(Some(event)) => {
                        match event {
                            Event::Output { data, .. } => {
                                let _ = write_to_stdout(&data);
                            }
                            Event::SessionExited { exit_code, .. } => {
                                let msg = match exit_code {
                                    Some(c) => format!("\r\n[session exited with code: {c}]\r\n"),
                                    None => "\r\n[session exited]\r\n".to_string(),
                                };
                                let _ = write_to_stdout(msg.as_bytes());
                                break;
                            }
                            Event::Error(e) => {
                                let _ = write_to_stdout(format!("\r\n[error: {e}]\r\n").as_bytes());
                            }
                            Event::ControlLost { session: _ } => {
                                let _ = write_to_stdout(b"\r\n[session control taken by another client]\r\n");
                            }
                            Event::StateSnapshot { snapshot, .. } => {
                                let painted = crate::render_snapshot::paint_snapshot(&snapshot);
                                let _ = write_to_stdout(&painted);
                                last_snapshot = Some(snapshot);
                            }
                            Event::SessionUpdated(_) => {}
                            Event::SessionTerminating { session: _ } => {}
                        }
                    }
                    Ok(None) => {
                        let _ = write_to_stdout(b"\r\n[detached: daemon disconnected]\r\n");
                        break;
                    }
                    Err(e) => {
                        let _ = write_to_stdout(format!("\r\n[read error: {e}]\r\n").as_bytes());
                        break;
                    }
                }
            }
        }
    }

    // Exit raw mode (guard will handle it via Drop).
    drop(_guard);

    if detached {
        eprintln!("[detached from session \"{}\"]", session_name);
    }

    Ok(())
}

fn write_to_stdout(data: &[u8]) -> Result<(), RemuxError> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle
        .write_all(data)
        .map_err(|e| RemuxError::IoError(format!("stdout write error: {e}")))?;
    handle
        .flush()
        .map_err(|e| RemuxError::IoError(format!("stdout flush error: {e}")))?;
    Ok(())
}

/// Parse a session name or ID into a SessionSelector.
fn parse_selector(name: &str) -> SessionSelector {
    if let Ok(uuid) = uuid::Uuid::parse_str(name) {
        SessionSelector::Id(remux_core::SessionId(uuid))
    } else {
        SessionSelector::Name(name.to_string())
    }
}

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// Parse a detach/prefix key string (e.g. "ctrl-a", "ctrl-q") into the raw
/// control byte the terminal emits for that chord. Unrecognized input falls
/// back to Ctrl-A (0x01), the default prefix.
fn parse_detach_byte(s: &str) -> u8 {
    const DEFAULT: u8 = 0x01; // Ctrl-A
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("ctrl-") {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_lowercase() {
                return (c as u8) - b'a' + 1;
            }
        }
    }
    DEFAULT
}

/// Produce a human-readable name for the prefix chord, e.g. "Ctrl-a". Used in
/// the on-attach detach hint. Falls back to "Ctrl-a" for unrecognized input.
fn human_prefix_name(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("ctrl-") {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_lowercase() {
                return format!("Ctrl-{c}");
            }
        }
    }
    "Ctrl-a".to_string()
}

/// What the prefix machine wants the caller to do with a fed byte.
#[derive(Debug, PartialEq, Eq)]
enum PrefixAction {
    /// Forward these bytes to the PTY verbatim.
    Forward(Vec<u8>),
    /// The byte was consumed as a (potential) prefix; nothing to do yet.
    Pending,
    /// Detach from the session.
    Detach,
    /// Repaint from the last known snapshot.
    Redraw,
}

/// Stateful GNU-screen-style prefix machine.
///
/// All bytes are forwarded verbatim except the prefix byte, which is held until
/// the following byte disambiguates the command:
/// - `d` / Ctrl-d        -> detach
/// - `a` / prefix byte   -> send a single literal prefix byte
/// - `l` / Ctrl-l        -> redraw from the last snapshot
/// - any other byte X    -> prefix byte then X are both forwarded (transparency)
///
/// The pending state lives in the struct so a prefix at the end of one chunk
/// composes correctly with the command byte arriving in the next chunk.
struct PrefixMachine {
    prefix: u8,
    /// True once the prefix byte has been seen and we await the command byte.
    armed: bool,
}

impl PrefixMachine {
    fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
        }
    }

    fn feed(&mut self, byte: u8) -> PrefixAction {
        if self.armed {
            self.armed = false;
            match byte {
                // detach
                0x64 | 0x04 => PrefixAction::Detach, // 'd' or Ctrl-d
                // literal prefix: 'a' or the prefix byte itself
                0x61 => PrefixAction::Forward(vec![self.prefix]),
                b if b == self.prefix => PrefixAction::Forward(vec![self.prefix]),
                // redraw
                0x6c | 0x0c => PrefixAction::Redraw, // 'l' or Ctrl-l
                // unrecognized: forward the prefix byte then this byte
                other => PrefixAction::Forward(vec![self.prefix, other]),
            }
        } else if byte == self.prefix {
            self.armed = true;
            PrefixAction::Pending
        } else {
            PrefixAction::Forward(vec![byte])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: feed a whole byte slice and collect the resulting actions,
    /// flattening forwarded bytes into a single buffer.
    fn drive(m: &mut PrefixMachine, bytes: &[u8]) -> (Vec<u8>, bool, bool) {
        let mut forward = Vec::new();
        let mut detach = false;
        let mut redraw = false;
        for &b in bytes {
            match m.feed(b) {
                PrefixAction::Forward(out) => forward.extend_from_slice(&out),
                PrefixAction::Pending => {}
                PrefixAction::Detach => detach = true,
                PrefixAction::Redraw => redraw = true,
            }
        }
        (forward, detach, redraw)
    }

    #[test]
    fn parse_detach_byte_ctrl_q() {
        assert_eq!(parse_detach_byte("ctrl-q"), 0x11);
    }

    #[test]
    fn parse_detach_byte_ctrl_a() {
        assert_eq!(parse_detach_byte("ctrl-a"), 0x01);
    }

    #[test]
    fn parse_detach_byte_garbage_defaults_to_ctrl_a() {
        assert_eq!(parse_detach_byte("garbage"), 0x01);
        assert_eq!(parse_detach_byte(""), 0x01);
        assert_eq!(parse_detach_byte("ctrl-"), 0x01);
        assert_eq!(parse_detach_byte("ctrl-ab"), 0x01);
        assert_eq!(parse_detach_byte("alt-q"), 0x01);
    }

    #[test]
    fn parse_detach_byte_case_insensitive_and_trimmed() {
        assert_eq!(parse_detach_byte("  Ctrl-Q  "), 0x11);
    }

    #[test]
    fn human_prefix_name_formats() {
        assert_eq!(human_prefix_name("ctrl-a"), "Ctrl-a");
        assert_eq!(human_prefix_name("CTRL-Q"), "Ctrl-q");
        assert_eq!(human_prefix_name("garbage"), "Ctrl-a");
    }

    #[test]
    fn lone_bytes_are_forwarded() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, b"hello world");
        assert_eq!(fwd, b"hello world");
        assert!(!detach);
        assert!(!redraw);
    }

    #[test]
    fn prefix_then_d_detaches() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, &[0x01, b'd']);
        assert!(fwd.is_empty());
        assert!(detach);
        assert!(!redraw);
    }

    #[test]
    fn prefix_then_ctrl_d_detaches() {
        let mut m = PrefixMachine::new(0x01);
        let (_fwd, detach, _redraw) = drive(&mut m, &[0x01, 0x04]);
        assert!(detach);
    }

    #[test]
    fn prefix_then_a_sends_literal_prefix() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, &[0x01, b'a']);
        assert_eq!(fwd, vec![0x01]);
        assert!(!detach);
        assert!(!redraw);
    }

    #[test]
    fn prefix_then_prefix_sends_literal_prefix() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, _detach, _redraw) = drive(&mut m, &[0x01, 0x01]);
        assert_eq!(fwd, vec![0x01]);
    }

    #[test]
    fn prefix_then_l_redraws() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, &[0x01, b'l']);
        assert!(fwd.is_empty());
        assert!(!detach);
        assert!(redraw);
    }

    #[test]
    fn prefix_then_unknown_forwards_both() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, &[0x01, b'x']);
        assert_eq!(fwd, vec![0x01, b'x']);
        assert!(!detach);
        assert!(!redraw);
    }

    #[test]
    fn prefix_split_across_two_feeds() {
        let mut m = PrefixMachine::new(0x01);
        // First chunk ends with the bare prefix byte.
        let (fwd1, detach1, _) = drive(&mut m, &[b'a', b'b', 0x01]);
        assert_eq!(fwd1, b"ab");
        assert!(!detach1);
        // Second chunk delivers the command byte.
        let (fwd2, detach2, _) = drive(&mut m, b"d");
        assert!(fwd2.is_empty());
        assert!(detach2);
    }

    #[test]
    fn bytes_containing_prefix_mid_stream() {
        // A paste containing the prefix byte followed by a normal char forwards
        // the prefix transparently (prefix + char), not eaten.
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw) = drive(&mut m, &[b'h', 0x01, b'i']);
        assert_eq!(fwd, vec![b'h', 0x01, b'i']);
        assert!(!detach);
        assert!(!redraw);
    }

    #[test]
    fn arbitrary_escape_and_utf8_pass_unchanged() {
        let mut m = PrefixMachine::new(0x01);
        let chunk = [0x1b, b'[', b'A', 0xf0, 0x9f, 0x98, 0x80];
        let (fwd, detach, redraw) = drive(&mut m, &chunk);
        assert_eq!(fwd, chunk.to_vec());
        assert!(!detach);
        assert!(!redraw);
    }
}
