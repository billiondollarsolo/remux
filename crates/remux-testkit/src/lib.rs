mod client;

pub use client::TestClient;

use std::path::{Path, PathBuf};
use std::time::Duration;

use remux_core::RemuxError;

/// Test harness that manages a remuxd daemon process for integration testing.
///
/// Creates a temporary directory for the daemon's socket and data, starts the
/// daemon process, and provides a `TestClient` for sending IPC requests.
pub struct DaemonHarness {
    daemon_process: Option<tokio::process::Child>,
    socket_path: PathBuf,
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
}

impl DaemonHarness {
    /// Start a new remuxd daemon in a temporary directory.
    ///
    /// Waits up to 5 seconds for the socket to become available.
    pub async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let socket_path = temp_dir.path().join("remux.sock");

        let daemon_bin = Self::find_daemon_binary()?;

        let mut cmd = tokio::process::Command::new(daemon_bin);
        cmd.env(
            "REMUX_SOCKET_PATH",
            socket_path.to_string_lossy().to_string(),
        );
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            RemuxError::Internal(format!("failed to spawn remuxd: {e}"))
        })?;

        let mut harness = Self {
            daemon_process: Some(child),
            socket_path,
            temp_dir,
        };

        // Wait for socket to appear
        harness.wait_for_socket(Duration::from_secs(5)).await?;

        Ok(harness)
    }

    /// Start a daemon using an explicit binary path.
    pub async fn start_with_binary(daemon_bin: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        let socket_path = temp_dir.path().join("remux.sock");

        let mut cmd = tokio::process::Command::new(daemon_bin);
        cmd.env(
            "REMUX_SOCKET_PATH",
            socket_path.to_string_lossy().to_string(),
        );
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            RemuxError::Internal(format!("failed to spawn remuxd: {e}"))
        })?;

        let mut harness = Self {
            daemon_process: Some(child),
            socket_path,
            temp_dir,
        };

        harness.wait_for_socket(Duration::from_secs(5)).await?;

        Ok(harness)
    }

    /// Get the socket path for client connections.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Create a new `TestClient` connected to this daemon.
    pub async fn connect(&self) -> Result<TestClient, RemuxError> {
        TestClient::connect(&self.socket_path).await
    }

    /// Stop the daemon process gracefully.
    pub async fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(ref mut child) = self.daemon_process {
            child.kill().await?;
        }
        self.daemon_process = None;
        Ok(())
    }

    /// Wait for the Unix socket file to exist.
    async fn wait_for_socket(&mut self, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.socket_path.exists() {
                return Ok(());
            }
            // Check if the daemon is still alive
            if let Some(ref mut child) = self.daemon_process {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return Err(format!("remuxd exited prematurely with status: {status}").into());
                    }
                    Ok(None) => {} // Still running
                    Err(e) => {
                        return Err(format!("failed to check remuxd status: {e}").into());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("timed out waiting for remuxd socket".into())
    }

    /// Try to find the remuxd binary in common locations.
    fn find_daemon_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
        let candidates = [
            PathBuf::from("target/debug/remuxd"),
            PathBuf::from("target/release/remuxd"),
            cargo_home().map(|h| h.join("bin/remuxd")).unwrap_or_default(),
            PathBuf::from("/usr/local/bin/remuxd"),
        ];

        for candidate in &candidates {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }

        // Fall back to $PATH lookup
        which_binary("remuxd").map_err(|e| e.into())
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.daemon_process {
            // Best-effort kill; ignore errors since we're dropping
            let _ = child.start_kill();
        }
    }
}

/// Resolve a binary name on `$PATH`.
fn which_binary(name: &str) -> Result<PathBuf, String> {
    let path_var = std::env::var_os("PATH").ok_or("PATH not set")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("{name} not found on PATH"))
}

/// Get the Cargo home directory.
fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".cargo");
                p.into_os_string()
            })
        })
        .map(PathBuf::from)
}
