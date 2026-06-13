use std::path::Path;

use remux_core::framing::{read_message, write_message};
use remux_core::{RemuxError, Request, Response};
use tokio::net::UnixStream;

/// IPC client for communicating with the remuxd daemon.
pub struct RemuxClient {
    stream: UnixStream,
    line_buf: Vec<u8>,
}

impl RemuxClient {
    /// Connect to the daemon's Unix domain socket.
    pub async fn connect(socket_path: &Path) -> Result<Self, RemuxError> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            RemuxError::ConnectionFailed(format!("failed to connect to daemon: {e}"))
        })?;
        Ok(Self {
            stream,
            line_buf: Vec::new(),
        })
    }

    /// Send a request and receive a single response.
    pub async fn send_request(&mut self, request: Request) -> Result<Response, RemuxError> {
        write_message(&mut self.stream, &request).await?;
        let response: Option<Response> = read_message(&mut self.stream, &mut self.line_buf).await?;
        match response {
            Some(r) => Ok(r),
            None => Err(RemuxError::ConnectionFailed(
                "daemon closed connection".to_string(),
            )),
        }
    }

    /// Send a request without waiting for a response. Used for fire-and-forget
    /// requests the daemon does not reply to (e.g. `SendInput`).
    pub async fn send_oneway(&mut self, request: Request) -> Result<(), RemuxError> {
        write_message(&mut self.stream, &request).await
    }

    /// Split the internal stream into read and write halves (for attach mode).
    pub fn split(
        self,
    ) -> (
        tokio::io::ReadHalf<UnixStream>,
        tokio::io::WriteHalf<UnixStream>,
    ) {
        tokio::io::split(self.stream)
    }
}
