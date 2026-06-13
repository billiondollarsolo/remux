use std::path::Path;

use remux_core::framing::{read_message, write_message};
use remux_core::{
    AttachMode, ClientId, CreateSessionRequest, RemuxError, Request, Response, ScrollbackChunk,
    SessionDetails, SessionSelector, SessionSummary, TermSize, TerminalSnapshot,
};
use tokio::net::UnixStream;

/// Test client for communicating with a remuxd daemon over IPC.
///
/// Uses the shared framing module from remux-core for consistent
/// message serialization with convenience methods for integration tests.
pub struct TestClient {
    stream: UnixStream,
    line_buf: Vec<u8>,
}

impl TestClient {
    /// Connect to the daemon's Unix socket at the given path.
    pub async fn connect(socket_path: &Path) -> Result<Self, RemuxError> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| RemuxError::ConnectionFailed(format!("{}: {e}", socket_path.display())))?;
        let mut client = Self {
            stream,
            line_buf: Vec::new(),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Perform the lenient protocol handshake: send `Hello` with our protocol
    /// version and read (and discard) the daemon's reply. A version mismatch is
    /// reported by the daemon as `Response::Error`.
    async fn handshake(&mut self) -> Result<(), RemuxError> {
        write_message(
            &mut self.stream,
            &Request::Hello {
                version: remux_core::PROTOCOL_VERSION,
            },
        )
        .await?;
        match read_message::<Response>(&mut self.stream, &mut self.line_buf).await? {
            Some(Response::Error(e)) => Err(e),
            _ => Ok(()),
        }
    }

    /// Send an arbitrary request and receive a response.
    pub async fn send_request(&mut self, request: &Request) -> Result<Response, RemuxError> {
        write_message(&mut self.stream, request).await?;
        let response = read_message(&mut self.stream, &mut self.line_buf)
            .await?
            .ok_or_else(|| RemuxError::ConnectionFailed("daemon closed connection".to_string()))?;
        Ok(response)
    }

    /// Ping the daemon to check connectivity.
    pub async fn ping(&mut self) -> Result<(), RemuxError> {
        match self.send_request(&Request::Ping).await? {
            Response::Pong => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to Ping: {other:?}"
            ))),
        }
    }

    /// Create a new session with the given name and default settings.
    pub async fn create_session(&mut self, name: &str) -> Result<SessionDetails, RemuxError> {
        let request = Request::CreateSession(CreateSessionRequest {
            name: Some(name.to_string()),
            command: vec!["bash".to_string()],
            cwd: None,
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            size: TermSize { cols: 80, rows: 24 },
        });

        match self.send_request(&request).await? {
            Response::Created(details) => Ok(details),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to CreateSession: {other:?}"
            ))),
        }
    }

    /// Create a new session with an explicit command and default settings.
    ///
    /// Unlike [`create_session`](Self::create_session) (which always runs
    /// `bash`), this lets a test pick the program to run, e.g. `["/bin/sh"]`
    /// for an interactive shell or `["sleep", "30"]` for a long-lived
    /// no-input process.
    pub async fn create_session_with_command(
        &mut self,
        name: &str,
        command: &[&str],
    ) -> Result<SessionDetails, RemuxError> {
        let request = Request::CreateSession(CreateSessionRequest {
            name: Some(name.to_string()),
            command: command.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: vec![("TERM".to_string(), "xterm-256color".to_string())],
            size: TermSize { cols: 80, rows: 24 },
        });

        match self.send_request(&request).await? {
            Response::Created(details) => Ok(details),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to CreateSession: {other:?}"
            ))),
        }
    }

    /// List all sessions.
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionSummary>, RemuxError> {
        match self.send_request(&Request::ListSessions).await? {
            Response::SessionList(sessions) => Ok(sessions),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to ListSessions: {other:?}"
            ))),
        }
    }

    /// Kill a session by name.
    pub async fn kill_session(&mut self, name: &str) -> Result<(), RemuxError> {
        let request = Request::KillSession {
            session: remux_core::SessionSelector::Name(name.to_string()),
            signal: None,
        };

        match self.send_request(&request).await? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to KillSession: {other:?}"
            ))),
        }
    }

    /// Get full details for a session by name.
    pub async fn inspect_session(&mut self, name: &str) -> Result<SessionDetails, RemuxError> {
        let request = Request::InspectSession {
            session: remux_core::SessionSelector::Name(name.to_string()),
        };

        match self.send_request(&request).await? {
            Response::SessionDetails(details) => Ok(details),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to InspectSession: {other:?}"
            ))),
        }
    }

    /// Attach to a session. Returns the attach bootstrap data.
    pub async fn attach_session(
        &mut self,
        name: &str,
        client_id: ClientId,
    ) -> Result<remux_core::AttachBootstrap, RemuxError> {
        let request = Request::AttachSession {
            session: SessionSelector::Name(name.to_string()),
            size: TermSize { cols: 80, rows: 24 },
            mode: AttachMode::Control,
            client_id,
        };

        match self.send_request(&request).await? {
            Response::Attached(bootstrap) => Ok(bootstrap),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to AttachSession: {other:?}"
            ))),
        }
    }

    /// Detach a client from a session by name.
    pub async fn detach_session(
        &mut self,
        name: &str,
        client_id: ClientId,
    ) -> Result<(), RemuxError> {
        let request = Request::DetachSession {
            session: SessionSelector::Name(name.to_string()),
            client_id,
        };

        match self.send_request(&request).await? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to DetachSession: {other:?}"
            ))),
        }
    }

    /// Resize a session's PTY.
    pub async fn resize_session(
        &mut self,
        name: &str,
        client_id: ClientId,
        size: TermSize,
    ) -> Result<(), RemuxError> {
        let request = Request::ResizeSession {
            session: SessionSelector::Name(name.to_string()),
            size,
            client_id,
        };

        match self.send_request(&request).await? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to ResizeSession: {other:?}"
            ))),
        }
    }

    /// Read scrollback from a session.
    pub async fn read_scrollback(
        &mut self,
        name: &str,
        lines: usize,
    ) -> Result<ScrollbackChunk, RemuxError> {
        let request = Request::ReadScrollback {
            session: SessionSelector::Name(name.to_string()),
            lines,
        };

        match self.send_request(&request).await? {
            Response::Scrollback(chunk) => Ok(chunk),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to ReadScrollback: {other:?}"
            ))),
        }
    }

    /// Rename a session.
    pub async fn rename_session(
        &mut self,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), RemuxError> {
        let request = Request::RenameSession {
            session: SessionSelector::Name(old_name.to_string()),
            new_name: new_name.to_string(),
        };

        match self.send_request(&request).await? {
            Response::Ok => Ok(()),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to RenameSession: {other:?}"
            ))),
        }
    }

    /// Send raw input bytes to a session's PTY (by name).
    ///
    /// `SendInput` is fire-and-forget: on success the daemon sends no reply,
    /// so this method only writes the request and returns without waiting for a
    /// response. Only the controlling client (or a non-attached client) may
    /// send input; an attached observer is denied — but since the daemon does
    /// not reply on success, a permission error surfaces on a *subsequent*
    /// request rather than here.
    pub async fn send_input(&mut self, name: &str, data: &[u8]) -> Result<(), RemuxError> {
        let request = Request::SendInput {
            session: SessionSelector::Name(name.to_string()),
            data: data.to_vec(),
        };
        write_message(&mut self.stream, &request).await
    }

    /// Capture the current rendered screen of a session as a snapshot (by name).
    pub async fn capture_screen(&mut self, name: &str) -> Result<TerminalSnapshot, RemuxError> {
        let request = Request::CaptureScreen {
            session: SessionSelector::Name(name.to_string()),
        };

        match self.send_request(&request).await? {
            Response::Screen(snapshot) => Ok(snapshot),
            Response::Error(e) => Err(e),
            other => Err(RemuxError::ProtocolError(format!(
                "unexpected response to CaptureScreen: {other:?}"
            ))),
        }
    }
}
