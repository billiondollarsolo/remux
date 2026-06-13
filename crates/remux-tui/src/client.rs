use std::path::Path;

use remux_core::framing::{read_message, write_message};
use remux_core::{RemuxError, Request, Response};
use tokio::net::UnixStream;

/// IPC client for communicating with the remux daemon.
///
/// Uses the shared framing module from remux-core for consistent
/// message serialization across all clients.
pub struct RemuxClient {
    stream: UnixStream,
    line_buf: Vec<u8>,
}

impl RemuxClient {
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

    /// Send a request and wait for a response.
    pub async fn send_request(&mut self, request: &Request) -> Result<Response, RemuxError> {
        write_message(&mut self.stream, request).await?;
        let response = read_message(&mut self.stream, &mut self.line_buf)
            .await?
            .ok_or_else(|| RemuxError::ConnectionFailed("daemon closed connection".to_string()))?;
        Ok(response)
    }
}
