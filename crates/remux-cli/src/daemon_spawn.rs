use std::path::{Path, PathBuf};
use std::time::Duration;

use remux_core::RemuxError;

/// Ensure the daemon is running, spawning it if necessary.
pub fn ensure_daemon_running(socket_path: &Path) -> Result<(), RemuxError> {
    // Try connecting to see if daemon is already running.
    if try_connect(socket_path) {
        return Ok(());
    }

    // Socket file might be stale if the daemon crashed. Remove it.
    if socket_path.exists() {
        std::fs::remove_file(socket_path).map_err(|e| {
            RemuxError::IoError(format!("failed to remove stale socket: {e}"))
        })?;
    }

    // Ensure the parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            RemuxError::IoError(format!("failed to create socket directory: {e}"))
        })?;
    }

    // Find the remuxd binary.
    let daemon_bin = find_daemon_binary().ok_or(RemuxError::DaemonNotRunning)?;

    // Spawn the daemon as a background process.
    spawn_daemon(&daemon_bin)?;

    // Wait for the socket to become available.
    wait_for_socket(socket_path, Duration::from_secs(5))
}

/// Try a blocking connection to the socket to check if the daemon is alive.
fn try_connect(socket_path: &Path) -> bool {
    // Use a non-blocking check: if the socket file doesn't exist, daemon is not running.
    if !socket_path.exists() {
        return false;
    }
    // Try a quick std::net connection (UnixStream::connect is blocking but fast on local).
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

/// Find the remuxd binary. Look in the same directory as the current executable, then PATH.
fn find_daemon_binary() -> Option<PathBuf> {
    // First: same directory as current executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("remuxd");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    // Second: search PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir_str in path_var.split(':') {
            let candidate = PathBuf::from(dir_str).join("remuxd");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

/// Spawn the daemon as a detached background process.
fn spawn_daemon(bin: &Path) -> Result<(), RemuxError> {
    // Double-fork to fully detach from the controlling terminal.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(RemuxError::IoError("failed to fork daemon process".to_string()));
    }
    if pid > 0 {
        // Parent: wait briefly for the first fork to complete, then return.
        // Reap the intermediate child.
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        return Ok(());
    }

    // First child: fork again and exit.
    let pid2 = unsafe { libc::fork() };
    if pid2 < 0 {
        unsafe { libc::_exit(1); }
    }
    if pid2 > 0 {
        unsafe { libc::_exit(0); }
    }

    // Grandchild: this is the daemon process.
    // Create a new session.
    unsafe { libc::setsid(); }

    // Redirect stdin/stdout/stderr to /dev/null.
    let devnull_path = std::ffi::CString::new("/dev/null").unwrap_or_default();
    let devnull = unsafe { libc::open(devnull_path.as_ptr(), libc::O_RDWR) };
    if devnull >= 0 {
        unsafe {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }

    // Execute the daemon binary.
    let bin_cstr = std::ffi::CString::new(bin.to_string_lossy().into_owned()).unwrap_or_default();
    unsafe {
        libc::execv(bin_cstr.as_ptr(), [bin_cstr.as_ptr(), std::ptr::null()].as_ptr());
        // If execv fails:
        libc::_exit(1);
    }
}

/// Poll the socket path until it becomes connectable or we time out.
fn wait_for_socket(socket_path: &Path, timeout: Duration) -> Result<(), RemuxError> {
    let start = std::time::Instant::now();
    loop {
        if try_connect(socket_path) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(RemuxError::DaemonNotRunning);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
