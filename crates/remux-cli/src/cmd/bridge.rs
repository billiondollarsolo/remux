use std::path::Path;

use remux_core::RemuxError;
use tokio::net::UnixStream;

/// Handle the hidden `bridge` subcommand.
///
/// This is the remote half of the SSH transport: it runs on the remote host
/// (invoked as `ssh <host> remux bridge`) and pipes bytes bidirectionally
/// between this process's stdin/stdout and the *local* `remuxd` Unix socket.
/// A client on the originating machine therefore speaks the ordinary framed
/// remux protocol straight through the SSH channel to the remote daemon — the
/// daemon itself never sees the network and stays Unix-socket-only.
///
/// Connect-and-pipe only: it does not implicitly start a daemon. The caller
/// (`main`) runs the normal daemon-spawn path before dispatching here, so by
/// the time we connect the socket should exist.
pub async fn run(socket_path: &Path) -> Result<(), RemuxError> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        RemuxError::ConnectionFailed(format!(
            "bridge: failed to connect to daemon socket {}: {e}",
            socket_path.display()
        ))
    })?;

    let (mut sock_read, mut sock_write) = tokio::io::split(stream);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Copy in both directions concurrently. Exit as soon as either direction
    // finishes (client closed stdin, or the daemon closed the socket).
    let client_to_daemon = async {
        let _ = tokio::io::copy(&mut stdin, &mut sock_write).await;
        // Signal EOF to the daemon side so it can tear down cleanly.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock_write).await;
    };
    let daemon_to_client = async {
        let _ = tokio::io::copy(&mut sock_read, &mut stdout).await;
        let _ = tokio::io::AsyncWriteExt::flush(&mut stdout).await;
    };

    tokio::select! {
        _ = client_to_daemon => {}
        _ = daemon_to_client => {}
    }

    Ok(())
}
