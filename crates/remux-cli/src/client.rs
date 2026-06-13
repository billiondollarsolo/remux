use std::path::Path;

use remux_core::framing::{read_message, write_message};
use remux_core::{RemuxError, Request, Response};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Boxed read half of a client transport. Both the local Unix-socket path and
/// the remote SSH-piped path produce one of these, so the streaming commands
/// (`attach`, `wait`) work identically over either.
pub type ClientReadHalf = Box<dyn AsyncRead + Unpin + Send>;
/// Boxed write half of a client transport (see [`ClientReadHalf`]).
pub type ClientWriteHalf = Box<dyn AsyncWrite + Unpin + Send>;

/// The underlying duplex byte stream a [`RemuxClient`] speaks the framed
/// Request/Response/Event protocol over.
///
/// `Unix` is the local daemon socket. `Piped` is the remote path: stdin/stdout
/// of an `ssh <target> remux bridge` child, which the bridge end forwards
/// to the *remote* host's local `remuxd.sock`. The daemon is identical in both
/// cases — only the transport differs.
enum Transport {
    Unix(UnixStream),
    Piped {
        stdin: ChildStdin,
        stdout: ChildStdout,
        /// The SSH (or test bridge) child. Kept alive for the client's lifetime
        /// so the tunnel stays open; dropped (and killed) with the client.
        child: Child,
    },
}

/// IPC client for communicating with a remuxd daemon, either locally over a Unix
/// socket or remotely over an SSH-tunneled pipe.
pub struct RemuxClient {
    transport: Transport,
    line_buf: Vec<u8>,
}

impl RemuxClient {
    /// Connect to the local daemon's Unix domain socket.
    pub async fn connect(socket_path: &Path) -> Result<Self, RemuxError> {
        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            RemuxError::ConnectionFailed(format!("failed to connect to daemon: {e}"))
        })?;
        let mut client = Self {
            transport: Transport::Unix(stream),
            line_buf: Vec::new(),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Connect to a remote daemon by tunneling the protocol over SSH.
    ///
    /// Spawns `ssh <ssh_target> remux bridge` with piped stdin/stdout and an
    /// inherited stderr (so SSH prompts/errors reach the user), then speaks the
    /// same framed protocol over that pipe. The remote `remux bridge` connects
    /// to the remote host's local `remuxd.sock` and copies bytes both ways.
    pub async fn connect_remote(ssh_target: &str) -> Result<Self, RemuxError> {
        let mut cmd = Command::new("ssh");
        cmd.arg(ssh_target).arg("remux").arg("bridge");
        Self::connect_via_command(cmd).await
    }

    /// Build a client over a child process's stdin/stdout. The `cmd` must, when
    /// spawned, run something that speaks the framed remux protocol on its
    /// stdin/stdout (e.g. `ssh <target> remux bridge`, or `remux bridge`
    /// directly for tests). stderr is inherited so prompts/errors are visible.
    ///
    /// Factored out from [`connect_remote`] so the literal `ssh` invocation is
    /// injectable: tests spawn `remux bridge` directly to exercise the exact
    /// same transport code path without needing a real SSH server.
    pub async fn connect_via_command(mut cmd: Command) -> Result<Self, RemuxError> {
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::inherit());
        // Don't leave a zombie or a lingering tunnel if the client is dropped.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| RemuxError::ConnectionFailed(format!("failed to spawn transport: {e}")))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            RemuxError::ConnectionFailed("transport child has no stdin".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RemuxError::ConnectionFailed("transport child has no stdout".to_string())
        })?;

        let mut client = Self {
            transport: Transport::Piped {
                stdin,
                stdout,
                child,
            },
            line_buf: Vec::new(),
        };
        client.handshake().await?;
        Ok(client)
    }

    /// Perform the lenient protocol handshake: send `Hello` with our protocol
    /// version and read (and discard) the daemon's reply. A version mismatch is
    /// reported by the daemon as `Response::Error`.
    async fn handshake(&mut self) -> Result<(), RemuxError> {
        let request = Request::Hello {
            version: remux_core::PROTOCOL_VERSION,
        };
        self.write_request(&request).await?;
        match self.read_response().await? {
            Some(Response::Error(e)) => Err(e),
            _ => Ok(()),
        }
    }

    /// Send a request and receive a single response.
    pub async fn send_request(&mut self, request: Request) -> Result<Response, RemuxError> {
        self.write_request(&request).await?;
        match self.read_response().await? {
            Some(r) => Ok(r),
            None => Err(RemuxError::ConnectionFailed(
                "daemon closed connection".to_string(),
            )),
        }
    }

    /// Send a request without waiting for a response. Used for fire-and-forget
    /// requests the daemon does not reply to (e.g. `SendInput`).
    pub async fn send_oneway(&mut self, request: Request) -> Result<(), RemuxError> {
        self.write_request(&request).await
    }

    /// Write a request frame over whichever transport is active.
    async fn write_request(&mut self, request: &Request) -> Result<(), RemuxError> {
        match &mut self.transport {
            Transport::Unix(stream) => write_message(stream, request).await,
            Transport::Piped { stdin, .. } => write_message(stdin, request).await,
        }
    }

    /// Read a response frame over whichever transport is active.
    async fn read_response(&mut self) -> Result<Option<Response>, RemuxError> {
        match &mut self.transport {
            Transport::Unix(stream) => read_message(stream, &mut self.line_buf).await,
            Transport::Piped { stdout, .. } => read_message(stdout, &mut self.line_buf).await,
        }
    }

    /// Split the transport into boxed read and write halves (for attach/wait
    /// streaming). Works for both the local Unix socket and the remote pipe.
    ///
    /// For the piped transport the child handle is moved into the read half so
    /// the tunnel stays alive for as long as the streaming loop holds either
    /// half; it is killed on drop.
    pub fn split(self) -> (ClientReadHalf, ClientWriteHalf) {
        match self.transport {
            Transport::Unix(stream) => {
                let (r, w) = tokio::io::split(stream);
                (Box::new(r), Box::new(w))
            }
            Transport::Piped {
                stdin,
                stdout,
                child,
            } => {
                // Keep the child alive alongside the read half.
                let reader = PipedReadHalf {
                    stdout,
                    _child: child,
                };
                (Box::new(reader), Box::new(stdin))
            }
        }
    }
}

/// Read half of a piped transport that owns the child process so the SSH tunnel
/// stays open while streaming. Reads delegate to the child's stdout.
struct PipedReadHalf {
    stdout: ChildStdout,
    _child: Child,
}

impl AsyncRead for PipedReadHalf {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawning a non-existent transport command surfaces a connection error
    /// (rather than panicking), proving the piped-transport spawn path reports
    /// failures cleanly. This is the only failure mode reachable without a real
    /// daemon on the far end.
    #[tokio::test]
    async fn connect_via_command_reports_spawn_failure() {
        let cmd = Command::new("definitely-not-a-real-binary-remux-xyz");
        let result = RemuxClient::connect_via_command(cmd).await;
        assert!(
            matches!(result, Err(RemuxError::ConnectionFailed(_))),
            "expected ConnectionFailed on spawn failure"
        );
    }
}
