use std::io::Write as StdWrite;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use remux_core::framing::{read_message, write_message};
use remux_core::{
    AttachMode, ClientId, Event, RemuxError, Request, Response, SessionSelector, TermSize,
};
use tokio::io::BufReader;
use tokio::task;

use crate::client::RemuxClient;
use crate::raw_mode::get_terminal_size;

/// Handle the `attach` command.
pub async fn run(mut client: RemuxClient, name: String) -> Result<(), RemuxError> {
    let session = parse_selector(&name);
    let size: TermSize = get_terminal_size();
    let client_id = ClientId::new();

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

    // Spawn stdin reader task.
    let input_handle = task::spawn_blocking(move || -> Result<CrosstermEvent, RemuxError> {
        crossterm::event::read().map_err(|e| RemuxError::IoError(format!("stdin read error: {e}")))
    });

    let mut input_task = input_handle;
    let session_for_input = session.clone();
    let session_for_detach = session.clone();
    let mut detached = false;

    // Line buffer for reading daemon events in JSON mode.
    let mut event_line = Vec::new();

    loop {
        tokio::select! {
            // Handle stdin input events
            input_result = &mut input_task => {
                match input_result {
                    Ok(Ok(event)) => {
                        match handle_input_event(event) {
                            InputAction::Forward(bytes) => {
                                if !bytes.is_empty() {
                                    let send_req = Request::SendInput {
                                        session: session_for_input.clone(),
                                        data: bytes,
                                    };
                                    write_message(&mut daemon_writer, &send_req).await?;
                                }
                            }
                            InputAction::Detach => {
                                let detach_req = Request::DetachSession {
                                    session: session_for_detach.clone(),
                                    client_id: client_id.clone(),
                                };
                                let _ = write_message(&mut daemon_writer, &detach_req).await;
                                detached = true;
                                break;
                            }
                            InputAction::Resize(new_size) => {
                                let resize_req = Request::ResizeSession {
                                    session: session_for_input.clone(),
                                    size: new_size,
                                    client_id: client_id.clone(),
                                };
                                let _ = write_message(&mut daemon_writer, &resize_req).await;
                            }
                            InputAction::None => {}
                        }
                        // Spawn next input read.
                        input_task = task::spawn_blocking(|| {
                            crossterm::event::read()
                                .map_err(|e| RemuxError::IoError(format!("stdin read error: {e}")))
                        });
                    }
                    Ok(Err(e)) => {
                        let _ = write_to_stdout(format!("\r\n[input error: {e}]\r\n").as_bytes());
                        break;
                    }
                    Err(_) => {
                        let _ = write_to_stdout(b"\r\n[input task cancelled]\r\n");
                        break;
                    }
                }
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

/// Result of processing an input event.
enum InputAction {
    /// Forward these bytes to the PTY.
    Forward(Vec<u8>),
    /// User pressed detach key (Ctrl-Q).
    Detach,
    /// Terminal was resized.
    Resize(TermSize),
    /// No action needed.
    None,
}

/// Handle a single crossterm input event.
fn handle_input_event(event: CrosstermEvent) -> InputAction {
    match event {
        CrosstermEvent::Key(key_event) => {
            // Check for detach key: Ctrl-Q
            if key_event.modifiers.contains(KeyModifiers::CONTROL)
                && key_event.code == KeyCode::Char('q')
            {
                return InputAction::Detach;
            }

            let bytes = encode_key_event(key_event);
            if bytes.is_empty() {
                InputAction::None
            } else {
                InputAction::Forward(bytes)
            }
        }
        CrosstermEvent::Resize(cols, rows) => InputAction::Resize(TermSize { cols, rows }),
        _ => InputAction::None,
    }
}

/// Encode a crossterm key event into raw bytes for a PTY.
fn encode_key_event(key: crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyCode as KC;

    // Handle ctrl+letter
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if let KC::Char(c) = key.code {
            if c.is_ascii_lowercase() && c != 'q' {
                let byte = (c as u8) - b'a' + 1;
                return vec![byte];
            }
        }
    }

    match key.code {
        KC::Enter => vec![b'\r'],
        KC::Backspace => vec![0x7f],
        KC::Tab => vec![b'\t'],
        KC::Esc => vec![0x1b],
        KC::Up => vec![0x1b, b'[', b'A'],
        KC::Down => vec![0x1b, b'[', b'B'],
        KC::Right => vec![0x1b, b'[', b'C'],
        KC::Left => vec![0x1b, b'[', b'D'],
        KC::Home => vec![0x1b, b'[', b'H'],
        KC::End => vec![0x1b, b'[', b'F'],
        KC::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KC::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KC::Delete => vec![0x1b, b'[', b'3', b'~'],
        KC::Insert => vec![0x1b, b'[', b'2', b'~'],
        KC::F(1) => vec![0x1b, b'O', b'P'],
        KC::F(2) => vec![0x1b, b'O', b'Q'],
        KC::F(3) => vec![0x1b, b'O', b'R'],
        KC::F(4) => vec![0x1b, b'O', b'S'],
        KC::F(n) if (5..=12).contains(&n) => {
            let offset: u8 = if n <= 5 {
                15
            } else if n <= 10 {
                11
            } else {
                13
            };
            let code = offset + n;
            vec![0x1b, b'[', code, b'~']
        }
        KC::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        _ => Vec::new(),
    }
}
