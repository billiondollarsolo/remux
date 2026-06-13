use std::io::{Read as StdRead, Write as StdWrite};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    status_line: bool,
) -> Result<(), RemuxError> {
    let session = parse_selector(&name);
    let physical = get_terminal_size();
    let client_id = ClientId::new();
    let prefix_byte = parse_detach_byte(detach_key);
    let prefix_label = human_prefix_name(detach_key);
    let detach_hint = format!("{prefix_label}-d: detach ");

    // The status line reserves the bottom physical row by shrinking the app's
    // view to `rows - 1`. It is only active when requested AND the terminal is
    // tall enough to spare a row. `status_active` tracks the *current* state
    // (it can flip on SIGWINCH if the terminal shrinks below 2 rows).
    let mut status_active = status_line && physical.rows >= 2;

    // The size the session/PTY/VT is told about: one row shorter when the
    // status line is active so the app never addresses the reserved row.
    let size: TermSize = content_size(physical, status_active);

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

    // Guard to ensure we always exit raw mode AND tear down the DECSTBM scroll
    // region on every exit path (normal, detach, error, panic). A stuck scroll
    // region after exit would trap the user's shell prompt in the top region,
    // so the reset (`\x1b[r`) must be guaranteed. `region_set` is shared with
    // the loop: it is true whenever a scroll region is currently installed.
    struct RawModeGuard {
        region_set: Arc<AtomicBool>,
    }
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            if self.region_set.load(Ordering::SeqCst) {
                // Reset the scroll region, then move to the bottom row and clear
                // it so the status bar doesn't linger above the shell prompt.
                let rows = get_terminal_size().rows.max(1);
                let mut teardown = Vec::new();
                teardown.extend_from_slice(b"\x1b[r");
                let _ = write!(
                    &mut teardown as &mut Vec<u8>,
                    "\x1b[{rows};1H\x1b[0m\x1b[2K"
                );
                let _ = write_to_stdout(&teardown);
            }
            let _ = disable_raw_mode();
        }
    }
    let region_set = Arc::new(AtomicBool::new(false));
    let _guard = RawModeGuard {
        region_set: region_set.clone(),
    };

    // When the status line is active, reserve the bottom row by setting a
    // DECSTBM scroll region of `1..=rows-1` on the LOCAL terminal so streamed
    // scrolling stays in the top region and leaves the bottom row alone.
    if status_active {
        let _ = write_to_stdout(&set_scroll_region(physical.rows));
        region_set.store(true, Ordering::SeqCst);
    } else {
        // One-line detach hint so the user knows how to get out (when there is
        // no persistent bar to show it).
        eprint!("[detached with {prefix_label}-d]\r\n");
    }

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

    // Local line buffer for scroll (copy) mode. Seed it from the bootstrap
    // scrollback so history is immediately scrollable, then keep it current by
    // appending every `Event::Output` chunk (even while scrolling).
    let mut line_buffer = LineBuffer::new(MAX_SCROLL_LINES);
    line_buffer.seed(&bootstrap.scrollback);

    // Repaint the visible screen from the parsed VT snapshot so TUIs (vim,
    // htop, less) and colors come back faithfully on reattach.
    if let Some(snapshot) = bootstrap.vt_snapshot {
        let painted = crate::render_snapshot::paint_snapshot(&snapshot);
        write_to_stdout(&painted)?;
        last_snapshot = Some(snapshot);
    }

    // Helper closure-like macro for drawing the status bar at the current size.
    // We recompute the physical size each draw so SIGWINCH races can't paint a
    // stale row position.
    if status_active {
        let phys = get_terminal_size();
        let bar = status_bar_bytes(&session_name, false, &detach_hint, phys.cols, phys.rows);
        let _ = write_to_stdout(&bar);
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

    // Scroll (copy) mode state. `None` means LIVE mode; `Some` means we are in
    // scroll mode with the given key decoder and scroll offset (0 = newest).
    let mut scroll: Option<(ScrollKeys, usize)> = None;

    // Line buffer for reading daemon events in JSON mode.
    let mut event_line = Vec::new();

    loop {
        tokio::select! {
            // Handle raw stdin bytes. While in scroll (copy) mode bytes drive
            // navigation and are never forwarded to the PTY; otherwise they go
            // through the prefix machine and on to the session.
            chunk = stdin_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        if let Some((keys, offset)) = scroll.as_mut() {
                            // SCROLL MODE: decode navigation, never forward.
                            let mut quit = false;
                            for &b in &bytes {
                                if let Some(nav) = keys.feed(b) {
                                    apply_scroll_nav(nav, offset, &mut quit);
                                }
                            }
                            if quit {
                                exit_scroll_mode(&last_snapshot);
                                scroll = None;
                                // Back in LIVE mode: the snapshot repaint covers
                                // the content rows; restore the status bar.
                                if status_active {
                                    let phys = get_terminal_size();
                                    let bar = status_bar_bytes(
                                        &session_name, false, &detach_hint, phys.cols, phys.rows,
                                    );
                                    let _ = write_to_stdout(&bar);
                                }
                            } else {
                                let (rows, cols) = scroll_dimensions();
                                let view = line_buffer.view_lines();
                                let max_off = max_scroll_offset(view.len(), rows);
                                let off = (*offset).min(max_off);
                                *offset = off;
                                let painted = render_scroll_view(&view, off, rows, cols);
                                let _ = write_to_stdout(&painted);
                            }
                            continue;
                        }

                        // LIVE MODE: run bytes through the prefix machine.
                        let mut forward: Vec<u8> = Vec::new();
                        let mut do_detach = false;
                        let mut do_redraw = false;
                        let mut do_scroll = false;
                        for &b in &bytes {
                            match prefix.feed(b) {
                                PrefixAction::Forward(out) => forward.extend_from_slice(&out),
                                PrefixAction::Pending => {}
                                PrefixAction::Detach => do_detach = true,
                                PrefixAction::Redraw => do_redraw = true,
                                PrefixAction::EnterScroll => do_scroll = true,
                            }
                        }

                        // In read-only mode we never forward input to the PTY;
                        // the prefix machine still runs so detach/redraw/scroll work.
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
                            // The full repaint above covers the content rows;
                            // restore the status bar on the reserved row.
                            if status_active {
                                let phys = get_terminal_size();
                                let bar = status_bar_bytes(
                                    &session_name, false, &detach_hint, phys.cols, phys.rows,
                                );
                                let _ = write_to_stdout(&bar);
                            }
                        }

                        if do_scroll {
                            let (rows, cols) = scroll_dimensions();
                            // Enter the alternate screen so the live view is
                            // preserved, then render the buffer at offset 0.
                            let _ = write_to_stdout(b"\x1b[?1049h");
                            let view = line_buffer.view_lines();
                            let painted = render_scroll_view(&view, 0, rows, cols);
                            let _ = write_to_stdout(&painted);
                            scroll = Some((ScrollKeys::new(rows.saturating_sub(1).max(1)), 0));
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
                let phys = get_terminal_size();
                // Recompute whether the status line can be active at the new
                // size: it may flip off if the terminal shrank below 2 rows, or
                // back on if it grew. Re-set / tear down the DECSTBM region to
                // match before resizing the session.
                let want_active = status_line && phys.rows >= 2;
                if want_active && !status_active {
                    // Status line turning on: install the region.
                    let _ = write_to_stdout(&set_scroll_region(phys.rows));
                    region_set.store(true, Ordering::SeqCst);
                } else if want_active {
                    // Still on, possibly new height: re-set the region.
                    let _ = write_to_stdout(&set_scroll_region(phys.rows));
                } else if status_active {
                    // Status line turning off (terminal too short): reset the
                    // region so the whole screen is usable again.
                    let _ = write_to_stdout(b"\x1b[r");
                    region_set.store(false, Ordering::SeqCst);
                }
                status_active = want_active;

                // Resize the session to the content size (one row shorter while
                // the status line is active so the app never paints the bar row).
                let resize_req = Request::ResizeSession {
                    session: session_for_resize.clone(),
                    size: content_size(phys, status_active),
                    client_id: client_id.clone(),
                };
                let _ = write_message(&mut daemon_writer, &resize_req).await;

                // If scrolling, re-render the view at the new terminal size.
                if let Some((_keys, offset)) = scroll.as_mut() {
                    let (rows, cols) = scroll_dimensions();
                    let view = line_buffer.view_lines();
                    let max_off = max_scroll_offset(view.len(), rows);
                    let off = (*offset).min(max_off);
                    *offset = off;
                    let painted = render_scroll_view(&view, off, rows, cols);
                    let _ = write_to_stdout(&painted);
                } else if status_active {
                    // LIVE mode: repaint the bar at the new size/position.
                    let bar = status_bar_bytes(
                        &session_name, false, &detach_hint, phys.cols, phys.rows,
                    );
                    let _ = write_to_stdout(&bar);
                }
            }
            // Handle daemon events (output from the session)
            event_result = read_message::<Event>(&mut daemon_reader, &mut event_line) => {
                match event_result {
                    Ok(Some(event)) => {
                        match event {
                            Event::Output { data, .. } => {
                                // Always keep the local scroll buffer current,
                                // even while scrolling, so it stays live.
                                line_buffer.append_bytes(&data);
                                // In LIVE mode, write straight to stdout as
                                // before. In scroll mode, withhold output (the
                                // alternate screen is showing history) — the
                                // buffer captured it and it'll be visible on
                                // exit / when scrolling back to the bottom.
                                if scroll.is_none() {
                                    let _ = write_to_stdout(&data);
                                    // Repaint the status bar after every live
                                    // output chunk: an app `\x1b[2J` or scroll
                                    // can clobber the reserved row, and the
                                    // DECSTBM region only protects against
                                    // streamed scrolling, not erase-display.
                                    if status_active {
                                        let phys = get_terminal_size();
                                        let bar = status_bar_bytes(
                                            &session_name,
                                            false,
                                            &detach_hint,
                                            phys.cols,
                                            phys.rows,
                                        );
                                        let _ = write_to_stdout(&bar);
                                    }
                                }
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
                                if status_active && scroll.is_none() {
                                    let phys = get_terminal_size();
                                    let bar = status_bar_bytes(
                                        &session_name, false, &detach_hint, phys.cols, phys.rows,
                                    );
                                    let _ = write_to_stdout(&bar);
                                }
                            }
                            Event::StateSnapshot { snapshot, .. } => {
                                let painted = crate::render_snapshot::paint_snapshot(&snapshot);
                                let _ = write_to_stdout(&painted);
                                last_snapshot = Some(snapshot);
                                if status_active && scroll.is_none() {
                                    let phys = get_terminal_size();
                                    let bar = status_bar_bytes(
                                        &session_name, false, &detach_hint, phys.cols, phys.rows,
                                    );
                                    let _ = write_to_stdout(&bar);
                                }
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

/// The size the session/PTY/VT should be told about. When the status line is
/// active we shrink the app's view by one row (the reserved bottom physical
/// row) so the app never addresses it; columns are unchanged. When inactive (or
/// the terminal is too short) the full physical size is used unchanged.
fn content_size(physical: TermSize, status_active: bool) -> TermSize {
    if status_active && physical.rows >= 2 {
        TermSize {
            cols: physical.cols,
            rows: physical.rows - 1,
        }
    } else {
        physical
    }
}

/// Bytes that install a DECSTBM scroll region of `1..=rows-1` on the LOCAL
/// terminal, reserving the bottom physical row for the status bar so streamed
/// scrolling stays in the top region. Caller guarantees `rows >= 2`.
fn set_scroll_region(rows: u16) -> Vec<u8> {
    let bottom = rows.saturating_sub(1).max(1);
    format!("\x1b[1;{bottom}r").into_bytes()
}

/// Lay out the status bar TEXT for a terminal `cols` wide. Left segment is the
/// session name (and a `[SCROLL]` indicator when scrolling); right segment is
/// the detach hint. The result is exactly `cols` columns wide: the left segment
/// is padded with spaces up to where the right segment begins, and the whole
/// thing is truncated to `cols` if it would overflow. Pure and unit-tested.
fn format_status_bar(session_name: &str, scrolling: bool, detach_hint: &str, cols: u16) -> String {
    let cols = cols as usize;
    if cols == 0 {
        return String::new();
    }
    let mut left = format!(" remux: {session_name}");
    if scrolling {
        left.push_str("   [SCROLL]");
    }
    let right = detach_hint;

    // If both segments fit with at least one space between them, right-align the
    // hint. Otherwise fall back to the left segment alone, truncated to width.
    if left.chars().count() + right.chars().count() < cols {
        let gap = cols - left.chars().count() - right.chars().count();
        let mut out = String::with_capacity(cols);
        out.push_str(&left);
        for _ in 0..gap {
            out.push(' ');
        }
        out.push_str(right);
        out
    } else {
        // Truncate the left segment to fit and pad to full width.
        let mut out: String = left.chars().take(cols).collect();
        let pad = cols - out.chars().count();
        for _ in 0..pad {
            out.push(' ');
        }
        out
    }
}

/// Full byte sequence that paints the status bar on the reserved bottom row
/// without disturbing the app's cursor: save cursor (`\x1b7`), move to
/// `\x1b[{rows};1H`, write the reverse-video bar (`\x1b[7m … \x1b[0m`) laid out
/// by [`format_status_bar`] to exactly `cols`, then restore cursor (`\x1b8`).
fn status_bar_bytes(
    session_name: &str,
    scrolling: bool,
    detach_hint: &str,
    cols: u16,
    rows: u16,
) -> Vec<u8> {
    let text = format_status_bar(session_name, scrolling, detach_hint, cols);
    let mut out = Vec::new();
    // Save cursor, move to the bottom row, reverse video, bar text, reset,
    // restore cursor. The save/restore keeps the app's cursor intact.
    let _ = write!(
        &mut out as &mut Vec<u8>,
        "\x1b7\x1b[{rows};1H\x1b[7m{text}\x1b[0m\x1b8"
    );
    out
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

/// Maximum number of history lines the client keeps locally for scroll mode.
const MAX_SCROLL_LINES: usize = 10_000;

/// A bounded buffer of session output lines used by scroll (copy) mode.
///
/// Lines are stored without their trailing newline (and without a trailing
/// `\r`), mirroring the daemon's `ScrollbackBuffer`. A `partial` accumulator
/// holds the in-progress last line until a newline arrives. The buffer is
/// capped to `max_lines`, evicting the oldest lines first.
struct LineBuffer {
    lines: std::collections::VecDeque<Vec<u8>>,
    partial: Vec<u8>,
    max_lines: usize,
}

impl LineBuffer {
    fn new(max_lines: usize) -> Self {
        Self {
            lines: std::collections::VecDeque::new(),
            partial: Vec::new(),
            max_lines,
        }
    }

    /// Seed the buffer from a raw scrollback blob (history bytes). Splits on
    /// `\n`, stripping a trailing `\r` from each line. A trailing partial line
    /// (no terminating newline) is retained in `partial` so subsequent appends
    /// compose correctly.
    fn seed(&mut self, data: &[u8]) {
        self.append_bytes(data);
    }

    fn push(&mut self, line: Vec<u8>) {
        if self.max_lines > 0 && self.lines.len() >= self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Append raw output bytes, splitting on newline boundaries. Partial lines
    /// are accumulated until a newline is seen. Mirrors
    /// `remux-daemon::scrollback::ScrollbackBuffer::append_bytes`.
    fn append_bytes(&mut self, data: &[u8]) {
        self.partial.extend_from_slice(data);
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let mut line = self.partial.split_off(pos + 1);
            std::mem::swap(&mut line, &mut self.partial);
            // `line` now holds everything up to and including the newline.
            line.pop(); // drop '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // drop trailing '\r' (CRLF)
            }
            self.push(line);
        }
    }

    /// All complete lines plus the current partial line (if non-empty) as a
    /// single contiguous slice view, materialized into a `Vec` of refs. This is
    /// the set of lines scroll mode renders over.
    fn view_lines(&self) -> Vec<&[u8]> {
        let mut out: Vec<&[u8]> = self.lines.iter().map(|l| l.as_slice()).collect();
        if !self.partial.is_empty() {
            out.push(self.partial.as_slice());
        }
        out
    }
}

/// Compute the `[start, end)` line indices visible in scroll mode.
///
/// `total` is the number of lines available, `rows` is the terminal height
/// (the caller reserves one row for the status line, so it passes the content
/// height here), and `offset` is how many lines we've scrolled up from the
/// bottom (0 = newest at the bottom). The offset is clamped to
/// `[0, max(0, total - rows)]` so we never scroll past the oldest line.
fn visible_window(total: usize, rows: usize, offset: usize) -> (usize, usize) {
    if rows == 0 || total == 0 {
        return (0, 0);
    }
    let max_offset = total.saturating_sub(rows);
    let offset = offset.min(max_offset);
    // `end` is the exclusive index of the bottom-most visible line.
    let end = total - offset;
    let start = end.saturating_sub(rows);
    (start, end)
}

/// The maximum scroll offset for `total` lines and a window of `rows` rows
/// (the caller passes the full terminal height; one row is reserved for the
/// status line). Offset is in `[0, max]` where 0 = newest at the bottom.
fn max_scroll_offset(total: usize, rows: usize) -> usize {
    let content = rows.saturating_sub(1).max(1);
    total.saturating_sub(content)
}

/// Current terminal `(rows, cols)` as `usize`, for scroll rendering.
fn scroll_dimensions() -> (usize, usize) {
    let size = get_terminal_size();
    (size.rows as usize, size.cols as usize)
}

/// Apply a navigation intent to the scroll `offset` in place. Sets `quit` when
/// the user asked to leave scroll mode. Offsets grow toward older lines; they
/// are clamped to `>= 0` here and to the upper bound by the caller (which knows
/// the line count and terminal height).
fn apply_scroll_nav(nav: ScrollNav, offset: &mut usize, quit: &mut bool) {
    match nav {
        ScrollNav::Older(n) => *offset = offset.saturating_add(n),
        ScrollNav::Newer(n) => *offset = offset.saturating_sub(n),
        ScrollNav::Top => *offset = usize::MAX, // clamped to oldest by caller
        ScrollNav::Bottom => *offset = 0,
        ScrollNav::Quit => *quit = true,
    }
}

/// Leave scroll mode: drop the alternate screen and repaint the live view from
/// the last snapshot (if any).
fn exit_scroll_mode(last_snapshot: &Option<TerminalSnapshot>) {
    let _ = write_to_stdout(b"\x1b[?1049l");
    if let Some(snap) = last_snapshot {
        let painted = crate::render_snapshot::paint_snapshot(snap);
        let _ = write_to_stdout(&painted);
    }
}

/// A navigation intent decoded from input bytes while in scroll mode.
#[derive(Debug, PartialEq, Eq)]
enum ScrollNav {
    /// Scroll toward older lines by `n` (PageUp/Ctrl-b/k/Up).
    Older(usize),
    /// Scroll toward newer lines by `n` (PageDown/Ctrl-f/j/Down).
    Newer(usize),
    /// Jump to the oldest line (Home).
    Top,
    /// Jump to the newest line (End/G).
    Bottom,
    /// Leave scroll mode (q/Esc).
    Quit,
}

/// Stateful decoder for scroll-mode navigation. Handles single bytes (`k`,
/// `j`, `q`, `G`) and CSI escape sequences (arrows, PageUp/PageDown, Home/End)
/// that may be split across reads. `page` is the page size for PageUp/PageDown
/// (typically the visible content height).
struct ScrollKeys {
    /// Pending escape-sequence bytes (after an ESC), if any.
    pending: Vec<u8>,
    page: usize,
}

impl ScrollKeys {
    fn new(page: usize) -> Self {
        Self {
            pending: Vec::new(),
            page: page.max(1),
        }
    }

    /// Feed one byte; returns a navigation intent if one is complete.
    fn feed(&mut self, byte: u8) -> Option<ScrollNav> {
        if self.pending.is_empty() {
            match byte {
                0x1b => {
                    // Start of a possible escape sequence. A lone ESC is
                    // resolved on the next byte: if it isn't a CSI intro we
                    // treat the ESC as "quit".
                    self.pending.push(byte);
                    None
                }
                b'k' => Some(ScrollNav::Older(1)),
                b'j' => Some(ScrollNav::Newer(1)),
                0x02 => Some(ScrollNav::Older(self.page)), // Ctrl-b
                0x06 => Some(ScrollNav::Newer(self.page)), // Ctrl-f
                b'G' => Some(ScrollNav::Bottom),
                b'q' => Some(ScrollNav::Quit),
                _ => None,
            }
        } else {
            self.pending.push(byte);
            self.try_resolve_pending()
        }
    }

    fn try_resolve_pending(&mut self) -> Option<ScrollNav> {
        // pending[0] is always ESC.
        match self.pending.as_slice() {
            // Lone ESC followed by a non-`[`/`O` byte: treat ESC as quit and
            // drop the trailing byte (it is not a navigation key we model).
            [0x1b, b] if *b != b'[' && *b != b'O' => {
                self.pending.clear();
                Some(ScrollNav::Quit)
            }
            // Incomplete CSI/SS3 intro: keep waiting.
            [0x1b, b'['] | [0x1b, b'O'] => None,
            // SS3 arrows (ESC O A/B) — some terminals in application mode.
            [0x1b, b'O', c] => {
                let nav = arrow_or_none(*c);
                self.pending.clear();
                nav
            }
            // CSI arrows: ESC [ A/B/C/D.
            [0x1b, b'[', c @ (b'A' | b'B' | b'C' | b'D')] => {
                let nav = arrow_or_none(*c);
                self.pending.clear();
                nav
            }
            // CSI Home/End without parameters: ESC [ H / ESC [ F.
            [0x1b, b'[', b'H'] => {
                self.pending.clear();
                Some(ScrollNav::Top)
            }
            [0x1b, b'[', b'F'] => {
                self.pending.clear();
                Some(ScrollNav::Bottom)
            }
            // Parameterized CSI: ESC [ <digits> ~  (PageUp=5, PageDown=6,
            // Home=1/7, End=4/8).
            [0x1b, b'[', rest @ ..] => {
                if let Some((&last, params)) = rest.split_last() {
                    if last == b'~' {
                        let nav = match params {
                            b"5" => Some(ScrollNav::Older(self.page)),
                            b"6" => Some(ScrollNav::Newer(self.page)),
                            b"1" | b"7" => Some(ScrollNav::Top),
                            b"4" | b"8" => Some(ScrollNav::Bottom),
                            _ => None,
                        };
                        self.pending.clear();
                        return nav;
                    }
                    // Still accumulating digits; bail if it grows unreasonable.
                    if rest.len() > 8 {
                        self.pending.clear();
                    }
                    None
                } else {
                    None
                }
            }
            _ => {
                self.pending.clear();
                None
            }
        }
    }
}

fn arrow_or_none(c: u8) -> Option<ScrollNav> {
    match c {
        b'A' => Some(ScrollNav::Older(1)), // Up
        b'B' => Some(ScrollNav::Newer(1)), // Down
        _ => None,                         // Left/Right: no-op in scroll mode
    }
}

/// Render the scroll-mode view of `lines` at `offset` for a terminal of
/// `total_rows` rows by `cols` columns. Returns the bytes to write to stdout.
/// The bottom row is an inverse-video status line; the remaining rows show a
/// window of the buffer. Each content line is prefixed with an SGR reset so
/// color state from history lines does not bleed.
fn render_scroll_view(lines: &[&[u8]], offset: usize, total_rows: usize, cols: usize) -> Vec<u8> {
    let content_rows = total_rows.saturating_sub(1).max(1);
    let total = lines.len();
    let (start, end) = visible_window(total, content_rows, offset);

    let mut out: Vec<u8> = Vec::new();
    // Clear the screen and home the cursor (we are on the alternate screen).
    out.extend_from_slice(b"\x1b[2J\x1b[H");

    for line in &lines[start..end] {
        // Reset SGR so color state from a prior history line doesn't bleed,
        // then the raw line bytes (which may themselves carry SGR), then CRLF.
        out.extend_from_slice(b"\x1b[0m");
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }

    // Status / indicator line on the last row, in reverse video.
    let top = if total == 0 { 0 } else { start + 1 };
    let bottom = end;
    let _ = write!(
        &mut out as &mut Vec<u8>,
        "\x1b[{};1H\x1b[0m\x1b[7m",
        total_rows
    );
    let status =
        format!("-- SCROLL  line {top}-{bottom}/{total}  (PageUp/PageDown/k/j, q to quit) --");
    // Truncate/pad the status to the terminal width so the reverse-video bar
    // spans the row without wrapping.
    let mut bar = status.into_bytes();
    if cols > 0 {
        bar.truncate(cols);
        while bar.len() < cols {
            bar.push(b' ');
        }
    }
    out.extend_from_slice(&bar);
    out.extend_from_slice(b"\x1b[0m");

    out
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
    /// Enter scrollback (copy) mode.
    EnterScroll,
}

/// Stateful GNU-screen-style prefix machine.
///
/// All bytes are forwarded verbatim except the prefix byte, which is held until
/// the following byte disambiguates the command:
/// - `d` / Ctrl-d        -> detach
/// - `a` / prefix byte   -> send a single literal prefix byte
/// - `l` / Ctrl-l        -> redraw from the last snapshot
/// - `[`                 -> enter scrollback (copy) mode
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
                // enter scrollback (copy) mode
                0x5b => PrefixAction::EnterScroll, // '['
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
        let (fwd, detach, redraw, _scroll) = drive_full(m, bytes);
        (fwd, detach, redraw)
    }

    /// Like `drive` but also reports whether scroll mode was requested.
    fn drive_full(m: &mut PrefixMachine, bytes: &[u8]) -> (Vec<u8>, bool, bool, bool) {
        let mut forward = Vec::new();
        let mut detach = false;
        let mut redraw = false;
        let mut scroll = false;
        for &b in bytes {
            match m.feed(b) {
                PrefixAction::Forward(out) => forward.extend_from_slice(&out),
                PrefixAction::Pending => {}
                PrefixAction::Detach => detach = true,
                PrefixAction::Redraw => redraw = true,
                PrefixAction::EnterScroll => scroll = true,
            }
        }
        (forward, detach, redraw, scroll)
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

    #[test]
    fn prefix_then_bracket_enters_scroll() {
        let mut m = PrefixMachine::new(0x01);
        let (fwd, detach, redraw, scroll) = drive_full(&mut m, &[0x01, b'[']);
        assert!(fwd.is_empty());
        assert!(!detach);
        assert!(!redraw);
        assert!(scroll);
    }

    #[test]
    fn prefix_then_bracket_scroll_does_not_fire_alone() {
        // A lone '[' (no prefix) is forwarded, not a scroll request.
        let mut m = PrefixMachine::new(0x01);
        let (fwd, _d, _r, scroll) = drive_full(&mut m, b"[");
        assert_eq!(fwd, b"[");
        assert!(!scroll);
    }

    // ---- visible_window ----

    #[test]
    fn window_fewer_lines_than_rows() {
        // 3 lines, 10 content rows, offset 0 -> whole buffer.
        assert_eq!(visible_window(3, 10, 0), (0, 3));
    }

    #[test]
    fn window_exact_fit_offset_zero() {
        // 10 lines, 10 rows, offset 0 -> all 10.
        assert_eq!(visible_window(10, 10, 0), (0, 10));
    }

    #[test]
    fn window_scrolled_up() {
        // 100 lines, 10 rows, offset 5 -> lines [85, 95).
        assert_eq!(visible_window(100, 10, 5), (85, 95));
    }

    #[test]
    fn window_offset_clamped_to_oldest() {
        // 20 lines, 10 rows: max offset is 10; a larger offset clamps so the
        // window pins to the oldest lines [0, 10).
        assert_eq!(visible_window(20, 10, 999), (0, 10));
    }

    #[test]
    fn window_zero_rows_or_empty() {
        assert_eq!(visible_window(5, 0, 0), (0, 0));
        assert_eq!(visible_window(0, 10, 0), (0, 0));
    }

    // ---- max_scroll_offset ----

    #[test]
    fn max_offset_reserves_status_row() {
        // 21 total lines, terminal height 11 -> 10 content rows -> max 11.
        assert_eq!(max_scroll_offset(21, 11), 11);
        // Fewer lines than rows -> cannot scroll.
        assert_eq!(max_scroll_offset(3, 11), 0);
    }

    // ---- LineBuffer line splitting ----

    #[test]
    fn line_buffer_seed_splits_and_strips_cr() {
        let mut b = LineBuffer::new(100);
        b.seed(b"alpha\r\nbeta\ngamma\r\n");
        let v = b.view_lines();
        assert_eq!(v, vec![&b"alpha"[..], &b"beta"[..], &b"gamma"[..]]);
    }

    #[test]
    fn line_buffer_partial_line_carried() {
        let mut b = LineBuffer::new(100);
        b.append_bytes(b"hel");
        // Partial line not yet a complete line, but visible in view.
        assert_eq!(b.view_lines(), vec![&b"hel"[..]]);
        b.append_bytes(b"lo\nworld");
        assert_eq!(b.view_lines(), vec![&b"hello"[..], &b"world"[..]]);
    }

    #[test]
    fn line_buffer_caps_at_max_lines() {
        let mut b = LineBuffer::new(2);
        b.append_bytes(b"one\ntwo\nthree\n");
        // Oldest evicted; only the last two complete lines remain.
        assert_eq!(b.view_lines(), vec![&b"two"[..], &b"three"[..]]);
    }

    // ---- ScrollKeys navigation decoding ----

    #[test]
    fn scroll_keys_single_byte_nav() {
        let mut k = ScrollKeys::new(10);
        assert_eq!(k.feed(b'k'), Some(ScrollNav::Older(1)));
        assert_eq!(k.feed(b'j'), Some(ScrollNav::Newer(1)));
        assert_eq!(k.feed(b'G'), Some(ScrollNav::Bottom));
        assert_eq!(k.feed(b'q'), Some(ScrollNav::Quit));
        assert_eq!(k.feed(0x02), Some(ScrollNav::Older(10))); // Ctrl-b
        assert_eq!(k.feed(0x06), Some(ScrollNav::Newer(10))); // Ctrl-f
    }

    #[test]
    fn scroll_keys_arrows_and_pages() {
        let mut k = ScrollKeys::new(10);
        // Up arrow: ESC [ A
        assert_eq!(k.feed(0x1b), None);
        assert_eq!(k.feed(b'['), None);
        assert_eq!(k.feed(b'A'), Some(ScrollNav::Older(1)));
        // PageUp: ESC [ 5 ~
        assert_eq!(k.feed(0x1b), None);
        assert_eq!(k.feed(b'['), None);
        assert_eq!(k.feed(b'5'), None);
        assert_eq!(k.feed(b'~'), Some(ScrollNav::Older(10)));
        // PageDown: ESC [ 6 ~
        for b in [0x1b, b'[', b'6'] {
            assert_eq!(k.feed(b), None);
        }
        assert_eq!(k.feed(b'~'), Some(ScrollNav::Newer(10)));
        // Home: ESC [ H
        assert_eq!(k.feed(0x1b), None);
        assert_eq!(k.feed(b'['), None);
        assert_eq!(k.feed(b'H'), Some(ScrollNav::Top));
    }

    #[test]
    fn scroll_keys_lone_esc_quits() {
        let mut k = ScrollKeys::new(10);
        assert_eq!(k.feed(0x1b), None);
        // ESC followed by a non-CSI byte: treat ESC as quit.
        assert_eq!(k.feed(b'x'), Some(ScrollNav::Quit));
    }

    // ---- apply_scroll_nav clamping ----

    #[test]
    fn apply_nav_moves_and_clamps_low() {
        let mut off = 3;
        let mut quit = false;
        apply_scroll_nav(ScrollNav::Older(2), &mut off, &mut quit);
        assert_eq!(off, 5);
        apply_scroll_nav(ScrollNav::Newer(10), &mut off, &mut quit);
        assert_eq!(off, 0); // saturates at 0, doesn't underflow
        apply_scroll_nav(ScrollNav::Bottom, &mut off, &mut quit);
        assert_eq!(off, 0);
        apply_scroll_nav(ScrollNav::Top, &mut off, &mut quit);
        assert_eq!(off, usize::MAX); // caller clamps to oldest
        assert!(!quit);
        apply_scroll_nav(ScrollNav::Quit, &mut off, &mut quit);
        assert!(quit);
    }

    // ---- render_scroll_view ----

    #[test]
    fn render_scroll_view_has_status_and_lines() {
        let lines: Vec<&[u8]> = vec![b"one", b"two", b"three"];
        let out = render_scroll_view(&lines, 0, 4, 40);
        let s = String::from_utf8_lossy(&out);
        // Clears and homes.
        assert!(s.starts_with("\x1b[2J\x1b[H"));
        // Content lines reset SGR.
        assert!(s.contains("\x1b[0mone\r\n"));
        // Reverse-video status bar mentions SCROLL and the line range.
        assert!(s.contains("\x1b[7m"));
        assert!(s.contains("-- SCROLL"));
        assert!(s.contains("1-3/3"));
    }

    // ---- content_size / set_scroll_region ----

    #[test]
    fn content_size_reserves_a_row_when_active() {
        let phys = TermSize { cols: 80, rows: 24 };
        assert_eq!(content_size(phys, true), TermSize { cols: 80, rows: 23 });
    }

    #[test]
    fn content_size_full_when_inactive() {
        let phys = TermSize { cols: 80, rows: 24 };
        assert_eq!(content_size(phys, false), phys);
    }

    #[test]
    fn content_size_full_when_too_short() {
        // rows < 2: never reserve, even if asked to.
        let phys = TermSize { cols: 80, rows: 1 };
        assert_eq!(content_size(phys, true), phys);
    }

    #[test]
    fn set_scroll_region_bytes() {
        // 24 rows -> region 1..=23.
        assert_eq!(set_scroll_region(24), b"\x1b[1;23r".to_vec());
        // Degenerate small height clamps the bottom to >= 1.
        assert_eq!(set_scroll_region(2), b"\x1b[1;1r".to_vec());
    }

    // ---- format_status_bar (pure text layout) ----

    #[test]
    fn status_bar_right_aligns_hint_and_fills_width() {
        let s = format_status_bar("work", false, "Ctrl-a-d: detach ", 40);
        assert_eq!(s.chars().count(), 40);
        assert!(s.starts_with(" remux: work"));
        assert!(s.ends_with("Ctrl-a-d: detach "));
    }

    #[test]
    fn status_bar_includes_scroll_indicator() {
        let s = format_status_bar("work", true, "x", 40);
        assert!(s.contains("[SCROLL]"));
        assert_eq!(s.chars().count(), 40);
    }

    #[test]
    fn status_bar_truncates_when_too_narrow() {
        // Width too small to fit both segments: left segment truncated/padded
        // to exactly `cols`, hint dropped.
        let s = format_status_bar("a-very-long-session-name", false, "Ctrl-a-d: detach ", 10);
        assert_eq!(s.chars().count(), 10);
    }

    #[test]
    fn status_bar_zero_cols_is_empty() {
        assert_eq!(format_status_bar("x", false, "y", 0), "");
    }

    #[test]
    fn status_bar_bytes_wraps_with_cursor_save_restore() {
        let b = status_bar_bytes("work", false, "h", 20, 24);
        let s = String::from_utf8_lossy(&b);
        // Save cursor, goto row 24 col 1, reverse video, reset, restore cursor.
        assert!(s.starts_with("\x1b7\x1b[24;1H\x1b[7m"));
        assert!(s.ends_with("\x1b[0m\x1b8"));
    }

    #[test]
    fn render_scroll_view_empty_buffer() {
        let lines: Vec<&[u8]> = vec![];
        let out = render_scroll_view(&lines, 0, 4, 40);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("0-0/0"));
    }
}
