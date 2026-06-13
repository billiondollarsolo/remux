use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;

use remux_core::framing;
use remux_core::{
    ClientId, Config, Event, RemuxError, Request, Response, SessionId, SessionSelector,
};

use crate::persistence;
use crate::session_manager::{SessionManager, SharedSessionManager};

/// Interval between periodic scrollback flushes for crash-resilience.
const SCROLLBACK_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// The main daemon, owning the session registry and socket server.
pub struct Daemon {
    sessions: SharedSessionManager,
}

impl Daemon {
    pub fn new(config: Config) -> Self {
        // Startup recovery (Option A): prune stale persisted sessions, then
        // reconstruct prior sessions from disk as read-only `Exited` handles so
        // their metadata and scrollback survive a daemon restart. The PTYs are
        // gone with the old daemon; we never respawn processes.
        persistence::cleanup_old_sessions(&config);

        let mut manager = SessionManager::new(config);
        manager.load_persisted();

        let sessions = Arc::new(Mutex::new(manager));
        Self { sessions }
    }

    /// Run the daemon: bind to the Unix socket and accept connections.
    pub async fn run(self, socket_path: PathBuf) -> Result<(), RemuxError> {
        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RemuxError::IoError(format!("failed to create socket directory: {e}"))
            })?;
        }

        // Remove stale socket if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .map_err(|e| RemuxError::IoError(format!("failed to remove stale socket: {e}")))?;
        }

        // Bind the Unix listener
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            RemuxError::IoError(format!(
                "failed to bind socket {}: {e}",
                socket_path.display()
            ))
        })?;

        // Set socket permissions to owner-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&socket_path, perms).map_err(|e| {
                RemuxError::IoError(format!("failed to set socket permissions: {e}"))
            })?;
        }

        tracing::info!(socket = %socket_path.display(), "remuxd listening");

        let sessions = self.sessions.clone();

        // Periodic scrollback flush for crash-resilience. Only spawned when
        // `persist_scrollback` is enabled; on a hard daemon crash this bounds
        // scrollback loss to roughly one flush interval. Normal/graceful exits
        // are already flushed synchronously in `mark_exited`.
        let persist_enabled = { sessions.lock().await.persist_scrollback_enabled() };
        if persist_enabled {
            let flush_sessions = sessions.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(SCROLLBACK_FLUSH_INTERVAL);
                // Skip the immediate first tick; nothing to flush at startup.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let mgr = flush_sessions.lock().await;
                    mgr.flush_all_scrollback();
                }
            });
        }

        // Accept connections in a loop
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let sessions = sessions.clone();
                    let stream_peer = stream.peer_addr().ok();
                    tracing::debug!(peer = ?stream_peer, "accepted client connection");

                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, sessions).await {
                            tracing::error!(error = %e, "client handler error");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to accept connection");
                }
            }
        }
    }
}

/// Handle a single client connection using a unified write channel.
///
/// All writes to the client socket go through a single mpsc channel,
/// which ensures serialization whether the source is a response or event.
async fn handle_client(
    stream: tokio::net::UnixStream,
    sessions: SharedSessionManager,
) -> Result<(), RemuxError> {
    let client_id = ClientId::new();
    tracing::info!(client_id = ?client_id, "client connected");

    let (mut read_half, mut write_half) = tokio::io::split(stream);

    // Unified channel for all writes (responses and events) serialized to bytes
    let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    // Spawn a task that drains the write channel and writes to the socket
    let writer_handle = tokio::spawn(async move {
        while let Some(data) = write_rx.recv().await {
            if write_half.write_all(&data).await.is_err() {
                break;
            }
            if write_half.flush().await.is_err() {
                break;
            }
        }
    });

    // Persistent line buffer for framed reads
    let mut line_buf: Vec<u8> = Vec::new();

    // Sessions this connection has attached to. Used to make disconnect cleanup
    // O(attached) instead of iterating every session in the registry.
    let attached_sessions: Arc<tokio::sync::Mutex<HashSet<SessionId>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));

    // Whether we are still expecting a possible opening `Hello` handshake.
    let mut is_first_request = true;

    // Main request loop
    loop {
        let request = match framing::read_message::<Request>(&mut read_half, &mut line_buf).await {
            Ok(Some(req)) => req,
            Ok(None) => {
                tracing::info!(client_id = ?client_id, "client disconnected");
                break;
            }
            Err(e) => {
                tracing::error!(client_id = ?client_id, error = %e, "error reading request");
                break;
            }
        };

        // Intercept an opening `Hello` handshake. It is lenient: only the FIRST
        // message is treated as a handshake, and clients are not required to
        // send one. A version mismatch is a hard error and closes the
        // connection.
        if is_first_request {
            is_first_request = false;
            if let Request::Hello { version } = request {
                let resp = if version == remux_core::PROTOCOL_VERSION {
                    Response::Hello {
                        version: remux_core::PROTOCOL_VERSION,
                    }
                } else {
                    Response::Error(RemuxError::ProtocolError(format!(
                        "protocol version mismatch: client {version}, daemon {}",
                        remux_core::PROTOCOL_VERSION
                    )))
                };
                let mismatch = !matches!(resp, Response::Hello { .. });
                if let Ok(bytes) = framing::serialize_to_bytes(&resp) {
                    let _ = write_tx.send(bytes).await;
                }
                if mismatch {
                    tracing::warn!(client_id = ?client_id, "rejecting client: protocol mismatch");
                    break;
                }
                continue;
            }
        }

        let (response, event_rx, attached) =
            process_request_with_events(&client_id, request, &sessions).await;

        // Record the session this connection attached to, so disconnect cleanup
        // is O(attached) rather than O(total sessions).
        if let Some(sid) = attached {
            attached_sessions.lock().await.insert(sid);
        }

        // Send the response to the client (if any)
        if let Some(resp) = response {
            if let Ok(bytes) = framing::serialize_to_bytes(&resp) {
                if write_tx.send(bytes).await.is_err() {
                    break;
                }
            }
        }

        // If we got an event receiver (from attach), spawn a forwarder
        if let Some(mut rx) = event_rx {
            let write_tx_clone = write_tx.clone();
            let fwd_client_id = client_id.clone();
            let fwd_sessions = sessions.clone();
            let fwd_attached = attached_sessions.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if let Ok(bytes) = framing::serialize_to_bytes(&event) {
                        if write_tx_clone.send(bytes).await.is_err() {
                            break;
                        }
                    }
                }
                detach_tracked_sessions(&fwd_client_id, &fwd_sessions, &fwd_attached).await;
            });
        }
    }

    // Clean up
    drop(write_tx);
    let _ = writer_handle.await;
    detach_tracked_sessions(&client_id, &sessions, &attached_sessions).await;

    Ok(())
}

/// Detach a client from only the sessions it actually attached to.
///
/// O(attached) rather than O(total sessions): we iterate the connection-local
/// set of attached session ids instead of the whole registry. Ids are removed
/// from the set as they are detached so a later cleanup pass is a no-op.
async fn detach_tracked_sessions(
    client_id: &ClientId,
    sessions: &SharedSessionManager,
    attached_sessions: &Arc<tokio::sync::Mutex<HashSet<SessionId>>>,
) {
    let to_detach: Vec<SessionId> = {
        let mut set = attached_sessions.lock().await;
        set.drain().collect()
    };

    if to_detach.is_empty() {
        return;
    }

    let mut mgr = sessions.lock().await;
    for sid in to_detach {
        let selector = SessionSelector::Id(sid);
        let _ = mgr.detach_session(&selector, client_id);
    }
}

/// Process a single request, returning an optional response and optionally an event receiver
/// (when the request results in an attach).
///
/// Returns `None` for the response when no reply should be sent (e.g. SendInput).
async fn process_request_with_events(
    client_id: &ClientId,
    request: Request,
    sessions: &SharedSessionManager,
) -> (
    Option<Response>,
    Option<tokio::sync::mpsc::Receiver<Event>>,
    Option<SessionId>,
) {
    match request {
        // `Hello` is handled by the handshake path in `handle_client`; if it
        // arrives here (not as the first message) treat it as a no-op ack.
        Request::Hello { version: _ } => (
            Some(Response::Hello {
                version: remux_core::PROTOCOL_VERSION,
            }),
            None,
            None,
        ),

        Request::Ping => (Some(Response::Pong), None, None),

        Request::ListSessions => {
            let mgr = sessions.lock().await;
            (Some(Response::SessionList(mgr.list_sessions())), None, None)
        }

        Request::CreateSession(req) => {
            let mut mgr = sessions.lock().await;
            match mgr.create_session(req) {
                Ok((session_id, details)) => {
                    spawn_pty_pump(session_id, sessions.clone());
                    (Some(Response::Created(details)), None, None)
                }
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::InspectSession { session } => {
            let mgr = sessions.lock().await;
            match mgr.inspect_session(&session) {
                Ok(details) => (Some(Response::SessionDetails(details)), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::AttachSession {
            session,
            size,
            mode,
            client_id: _,
        } => {
            let mut mgr = sessions.lock().await;
            match mgr.attach_session(&session, size, mode, client_id.clone()) {
                Ok((bootstrap, rx)) => {
                    // Record the resolved id so disconnect cleanup is
                    // O(attached) rather than O(total sessions).
                    let sid = mgr.resolve_selector(&session).ok();
                    (Some(Response::Attached(bootstrap)), Some(rx), sid)
                }
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::DetachSession {
            session,
            client_id: _,
        } => {
            let mut mgr = sessions.lock().await;
            match mgr.detach_session(&session, client_id) {
                Ok(()) => (Some(Response::Ok), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::ResizeSession {
            session,
            size,
            client_id: _,
        } => {
            let mut mgr = sessions.lock().await;
            match mgr.resize_session(&session, size, client_id) {
                Ok(()) => (Some(Response::Ok), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::SendInput { session, data } => {
            let mut mgr = sessions.lock().await;
            // Only the controlling client may send input
            match mgr.send_input_for_client(&session, data, client_id) {
                Ok(()) => (None, None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::ReadScrollback { session, lines } => {
            let mgr = sessions.lock().await;
            match mgr.read_scrollback(&session, lines) {
                Ok(chunk) => (Some(Response::Scrollback(chunk)), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::RenameSession { session, new_name } => {
            let mut mgr = sessions.lock().await;
            match mgr.rename_session(&session, new_name) {
                Ok(()) => (Some(Response::Ok), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::CaptureScreen { session } => {
            let mgr = sessions.lock().await;
            match mgr.capture_screen(&session) {
                Ok(snapshot) => (Some(Response::Screen(snapshot)), None, None),
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }

        Request::KillSession { session, signal } => {
            let sid = {
                let mgr = sessions.lock().await;
                mgr.resolve_selector(&session).ok()
            };

            let mut mgr = sessions.lock().await;
            match mgr.kill_session(&session, signal) {
                Ok(()) => {
                    // Do NOT broadcast a fake `SessionExited` here: the PTY pump
                    // emits the authoritative one (with the real exit code) when
                    // the process actually dies. Send only an informational
                    // `SessionTerminating` so clients get instant feedback.
                    if let Some(ref id) = sid {
                        mgr.broadcast_event(
                            id,
                            Event::SessionTerminating {
                                session: id.clone(),
                            },
                        );
                    }
                    (Some(Response::Ok), None, None)
                }
                Err(e) => (Some(Response::Error(e)), None, None),
            }
        }
    }
}

/// Spawn the PTY output pump task for a session.
///
/// This task:
/// 1. Reads from the PTY master fd in a blocking context
/// 2. Appends output to the scrollback buffer
/// 3. Fans out Output events to all subscriber channels
/// 4. Detects process exit and emits SessionExited event
fn spawn_pty_pump(session_id: SessionId, sessions: SharedSessionManager) {
    tokio::spawn(async move {
        tracing::debug!(session_id = %session_id.0, "starting PTY output pump");

        // Extract the PTY reader from the session handle
        let reader: Box<dyn std::io::Read + Send> = {
            let mut mgr = sessions.lock().await;
            match mgr.take_pty_reader(&session_id) {
                Some(r) => r,
                None => {
                    tracing::error!(session_id = %session_id.0, "no PTY reader for session");
                    return;
                }
            }
        };

        // Channel to move PTY output from blocking read task to async context
        let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Spawn a blocking task that reads from the PTY
        let pump_session_id = session_id.clone();
        let read_handle = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        tracing::debug!(
                            session_id = %pump_session_id.0,
                            "PTY reader got EOF"
                        );
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if output_tx.blocking_send(data).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            session_id = %pump_session_id.0,
                            error = %e,
                            "PTY read error"
                        );
                        break;
                    }
                }
            }
        });

        // Process output in the async context: update scrollback and fan out events
        while let Some(data) = output_rx.recv().await {
            let mut mgr = sessions.lock().await;
            mgr.append_to_scrollback(&session_id, &data);
            mgr.broadcast_event(
                &session_id,
                Event::Output {
                    session: session_id.clone(),
                    data,
                },
            );
        }

        // Wait for the blocking read task to finish
        let _ = read_handle.await;

        // Check exit code and mark session as exited
        {
            let mut mgr = sessions.lock().await;
            let exit_code = mgr.check_exit_code(&session_id);
            mgr.mark_exited(&session_id, exit_code);
            mgr.broadcast_event(
                &session_id,
                Event::SessionExited {
                    session: session_id.clone(),
                    exit_code,
                },
            );
        }

        tracing::info!(session_id = %session_id.0, "PTY output pump finished");
    });
}
