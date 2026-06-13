use std::io::{Read as StdRead, Write as StdWrite};

use remux_core::framing::{read_message, write_message};
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
pub async fn run(
    mut client: RemuxClient,
    name: String,
    detach_key: &str,
) -> Result<(), RemuxError> {
    let session = parse_selector(&name);
    let size: TermSize = get_terminal_size();
    let client_id = ClientId::new();
    let detach_byte = parse_detach_byte(detach_key);

    // Send attach request.
    let response = client
        .send_request(Request::AttachSession {
            session: session.clone(),
            size,
            mode: AttachMode::Control,
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
    if let Some(ref snapshot) = bootstrap.vt_snapshot {
        let painted = crate::render_snapshot::paint_snapshot(snapshot);
        write_to_stdout(&painted)?;
    }

    // Split the UnixStream for concurrent reading and writing.
    let (read_half, write_half) = client.split();
    let mut daemon_reader = BufReader::new(read_half);
    let mut daemon_writer = write_half;

    // Spawn a blocking stdin reader. It forwards raw byte chunks over an mpsc
    // channel so the async loop can scan them for the detach key and relay the
    // rest verbatim to the PTY.
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

    // Line buffer for reading daemon events in JSON mode.
    let mut event_line = Vec::new();

    loop {
        tokio::select! {
            // Handle raw stdin bytes: forward verbatim, intercepting the detach key.
            chunk = stdin_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        let scan = scan_for_detach(&bytes, detach_byte);
                        if !scan.forward.is_empty() {
                            let send_req = Request::SendInput {
                                session: session_for_input.clone(),
                                data: scan.forward,
                            };
                            write_message(&mut daemon_writer, &send_req).await?;
                        }
                        if scan.detached {
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

/// Parse a detach key string (e.g. "ctrl-q", "ctrl-a") into the raw control
/// byte the terminal emits for that chord. Unrecognized input falls back to
/// Ctrl-Q (0x11).
fn parse_detach_byte(s: &str) -> u8 {
    const DEFAULT: u8 = 0x11; // Ctrl-Q
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

/// Result of scanning a raw stdin chunk for the detach byte.
#[derive(Debug, PartialEq, Eq)]
struct DetachScan {
    /// Bytes to forward to the PTY (everything before the detach byte, or the
    /// whole chunk if the detach byte is absent).
    forward: Vec<u8>,
    /// Whether the detach byte was found.
    detached: bool,
}

/// Scan a raw stdin chunk for the detach byte. Bytes preceding the detach byte
/// are forwarded; the detach byte itself (and anything after it in the same
/// chunk) is consumed. If the detach byte is absent the whole chunk is
/// forwarded verbatim.
fn scan_for_detach(chunk: &[u8], detach_byte: u8) -> DetachScan {
    match chunk.iter().position(|&b| b == detach_byte) {
        Some(idx) => DetachScan {
            forward: chunk[..idx].to_vec(),
            detached: true,
        },
        None => DetachScan {
            forward: chunk.to_vec(),
            detached: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_detach_byte_ctrl_q() {
        assert_eq!(parse_detach_byte("ctrl-q"), 0x11);
    }

    #[test]
    fn parse_detach_byte_ctrl_a() {
        assert_eq!(parse_detach_byte("ctrl-a"), 0x01);
    }

    #[test]
    fn parse_detach_byte_garbage_defaults_to_ctrl_q() {
        assert_eq!(parse_detach_byte("garbage"), 0x11);
        assert_eq!(parse_detach_byte(""), 0x11);
        assert_eq!(parse_detach_byte("ctrl-"), 0x11);
        assert_eq!(parse_detach_byte("ctrl-ab"), 0x11);
        assert_eq!(parse_detach_byte("alt-q"), 0x11);
    }

    #[test]
    fn parse_detach_byte_case_insensitive_and_trimmed() {
        assert_eq!(parse_detach_byte("  Ctrl-A  "), 0x01);
    }

    #[test]
    fn scan_for_detach_forwards_everything_when_absent() {
        let chunk = b"hello world";
        let scan = scan_for_detach(chunk, 0x11);
        assert_eq!(
            scan,
            DetachScan {
                forward: chunk.to_vec(),
                detached: false,
            }
        );
    }

    #[test]
    fn scan_for_detach_splits_on_detach_byte() {
        // "ab" then Ctrl-Q (0x11) then trailing bytes.
        let chunk = [b'a', b'b', 0x11, b'c', b'd'];
        let scan = scan_for_detach(&chunk, 0x11);
        assert_eq!(
            scan,
            DetachScan {
                forward: vec![b'a', b'b'],
                detached: true,
            }
        );
    }

    #[test]
    fn scan_for_detach_detach_byte_at_start() {
        let chunk = [0x11, b'x'];
        let scan = scan_for_detach(&chunk, 0x11);
        assert_eq!(
            scan,
            DetachScan {
                forward: Vec::new(),
                detached: true,
            }
        );
    }

    #[test]
    fn scan_for_detach_passes_arbitrary_bytes_unchanged() {
        // Escape sequences and UTF-8 must survive byte-exact (no detach byte here).
        let chunk = [0x1b, b'[', b'A', 0xf0, 0x9f, 0x98, 0x80];
        let scan = scan_for_detach(&chunk, 0x11);
        assert_eq!(scan.forward, chunk.to_vec());
        assert!(!scan.detached);
    }
}
