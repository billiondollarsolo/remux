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
        let daemon_bin = Self::find_daemon_binary()?;
        Self::start_with_binary(&daemon_bin).await
    }

    /// Start a daemon using an explicit binary path.
    pub async fn start_with_binary(daemon_bin: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        Self::start_inner(daemon_bin, temp_dir, None, false).await
    }

    /// Start a daemon with persistence enabled and an explicit, reusable data
    /// directory, so a subsequent daemon started on the same `data_dir` recovers
    /// the sessions and scrollback the first one persisted.
    ///
    /// The returned harness owns a fresh temp dir for its socket and config, but
    /// the daemon's `data.dir` is pointed at the caller-provided `data_dir`. Pass
    /// the same `data_dir` to two successive `start_with_data_dir` calls (with a
    /// `stop()` in between) to exercise restart recovery.
    pub async fn start_with_data_dir(
        data_dir: &Path,
        persist_scrollback: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let daemon_bin = Self::find_daemon_binary()?;
        Self::start_with_binary_and_data_dir(&daemon_bin, data_dir, persist_scrollback).await
    }

    /// Like [`Self::start_with_data_dir`] but with an explicit binary path (for
    /// tests that locate `remuxd` themselves rather than relying on the
    /// cwd-relative search).
    pub async fn start_with_binary_and_data_dir(
        daemon_bin: &Path,
        data_dir: &Path,
        persist_scrollback: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempfile::tempdir()?;
        Self::start_inner(
            daemon_bin,
            temp_dir,
            Some(data_dir.to_path_buf()),
            persist_scrollback,
        )
        .await
    }

    /// Internal: spawn the daemon with an isolated data dir written into a
    /// generated config file (passed via `-c`). Always isolates `data.dir` so
    /// test runs never touch the real `~/.local/share/remux` and so startup
    /// session recovery cannot pull in unrelated state.
    async fn start_inner(
        daemon_bin: &Path,
        temp_dir: tempfile::TempDir,
        data_dir: Option<PathBuf>,
        persist_scrollback: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let socket_path = temp_dir.path().join("remux.sock");
        // Default the data dir to this harness's own temp dir so each daemon is
        // fully isolated unless the caller asked to share one (restart tests).
        let data_dir = data_dir.unwrap_or_else(|| temp_dir.path().join("data"));
        std::fs::create_dir_all(&data_dir)?;

        // Write a minimal config pinning the data dir (and persistence). Written
        // as a literal TOML string to avoid pulling a toml serializer into the
        // testkit. Paths are emitted via debug formatting, which quotes/escapes.
        let config_path = temp_dir.path().join("config.toml");
        let config_toml = format!(
            "[data]\ndir = {data:?}\n\n[daemon]\npersist_scrollback = {persist}\n",
            data = data_dir,
            persist = persist_scrollback,
        );
        std::fs::write(&config_path, config_toml)?;

        let mut cmd = tokio::process::Command::new(daemon_bin);
        cmd.env(
            "REMUX_SOCKET_PATH",
            socket_path.to_string_lossy().to_string(),
        );
        // Pass the socket path explicitly via the daemon's `--socket` flag so the
        // daemon binds where the harness expects (the env var alone is advisory).
        cmd.arg("--socket").arg(&socket_path);
        cmd.arg("--config").arg(&config_path);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| RemuxError::Internal(format!("failed to spawn remuxd: {e}")))?;

        let mut harness = Self {
            daemon_process: Some(child),
            socket_path,
            temp_dir,
        };

        // Wait for socket to appear
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
    async fn wait_for_socket(
        &mut self,
        timeout: Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self.socket_path.exists() {
                return Ok(());
            }
            // Check if the daemon is still alive
            if let Some(ref mut child) = self.daemon_process {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return Err(
                            format!("remuxd exited prematurely with status: {status}").into()
                        );
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
            cargo_home()
                .map(|h| h.join("bin/remuxd"))
                .unwrap_or_default(),
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
