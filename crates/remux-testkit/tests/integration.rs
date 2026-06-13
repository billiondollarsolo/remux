//! End-to-end integration tests for remux.
//!
//! These tests spawn a real `remuxd` daemon (via [`DaemonHarness`]) in a
//! temporary directory and drive it over the IPC socket with a [`TestClient`].
//! They exercise real PTYs, so they:
//!   * use generous timeouts,
//!   * poll async PTY output in retry loops instead of assuming instant output,
//!   * rely on the harness killing the daemon on drop for teardown.
//!
//! The daemon binary must exist (`cargo build -p remux-daemon`) before running;
//! `DaemonHarness::start()` fails with a clear message if it cannot be found.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use remux_core::{SessionStatus, TerminalSnapshot};
use remux_testkit::DaemonHarness;

/// Locate the freshly built `remuxd` binary.
///
/// `DaemonHarness::start()` looks for `target/debug/remuxd` *relative to the
/// process cwd*, which under `cargo test` is the package directory
/// (`crates/remux-testkit`), not the workspace root — so the relative lookup
/// misses the workspace-level `target/`. We resolve it deterministically from
/// `CARGO_MANIFEST_DIR` (the package dir) by walking up to the workspace root
/// and honoring `CARGO_TARGET_DIR` if set.
fn locate_remuxd() -> PathBuf {
    let exe = "remuxd";

    // Respect an explicit target dir if the build uses one.
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let mut p = PathBuf::from(target_dir);
        p.push("debug");
        p.push(exe);
        if p.exists() {
            return p;
        }
    }

    // crates/remux-testkit -> crates -> <workspace root>
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest_dir.ancestors() {
        for profile in ["debug", "release"] {
            let candidate = ancestor.join("target").join(profile).join(exe);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    panic!(
        "could not find the `{exe}` binary; run `cargo build -p remux-daemon` first \
         (searched CARGO_TARGET_DIR and target/{{debug,release}} from {})",
        manifest_dir.display()
    );
}

/// Start a daemon harness using the explicitly located `remuxd` binary.
async fn start_harness() -> DaemonHarness {
    let bin = locate_remuxd();
    DaemonHarness::start_with_binary(&bin)
        .await
        .expect("failed to start remuxd harness")
}

/// Join a snapshot's cells into per-row strings so tests can search for text.
fn snapshot_rows(snap: &TerminalSnapshot) -> Vec<String> {
    let cols = snap.cols as usize;
    if cols == 0 {
        return Vec::new();
    }
    snap.cells
        .chunks(cols)
        .map(|row| row.iter().map(|cell| cell.ch).collect::<String>())
        .collect()
}

/// True if any row of the snapshot contains `needle`.
fn snapshot_contains(snap: &TerminalSnapshot, needle: &str) -> bool {
    snapshot_rows(snap).iter().any(|row| row.contains(needle))
}

#[tokio::test]
async fn create_list_kill_roundtrip() {
    let harness = start_harness().await;

    let mut client = harness
        .connect()
        .await
        .expect("failed to connect to daemon");

    client.ping().await.expect("ping failed");

    // Run a long-lived process so the session stays Running until we kill it.
    // `sleep` (unlike an interactive `/bin/sh`, which ignores SIGTERM) exits
    // promptly on the default SIGTERM that `kill_session` sends.
    let name = "roundtrip";
    let details = client
        .create_session_with_command(name, &["sleep", "30"])
        .await
        .expect("create_session failed");
    assert_eq!(details.name, name);

    // It should appear in `list`.
    let sessions = client.list_sessions().await.expect("list_sessions failed");
    assert!(
        sessions.iter().any(|s| s.name == name),
        "created session {name:?} not present in list: {sessions:?}"
    );

    // We can inspect it and it should be alive.
    let inspected = client
        .inspect_session(name)
        .await
        .expect("inspect_session failed");
    assert_eq!(inspected.name, name);
    assert_ne!(
        inspected.status,
        SessionStatus::Exited,
        "freshly created session should not be Exited"
    );

    // Kill it.
    client
        .kill_session(name)
        .await
        .expect("kill_session failed");

    // Poll until it is gone or marked Exited; killing a PTY is async.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut gone_or_exited = false;
    while Instant::now() < deadline {
        let sessions = client.list_sessions().await.expect("list_sessions failed");
        match sessions.iter().find(|s| s.name == name) {
            None => {
                gone_or_exited = true;
                break;
            }
            Some(s) if s.status == SessionStatus::Exited => {
                gone_or_exited = true;
                break;
            }
            Some(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        gone_or_exited,
        "session {name:?} was neither removed nor marked Exited after kill"
    );
}

#[tokio::test]
async fn send_and_capture() {
    let harness = start_harness().await;

    let mut client = harness
        .connect()
        .await
        .expect("failed to connect to daemon");

    // Interactive shell (no `-c`), so it reads our injected input and echoes it.
    let name = "send-capture";
    client
        .create_session_with_command(name, &["/bin/sh"])
        .await
        .expect("create_session failed");

    // Give the shell a moment to start and reach its read loop.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // This client is never attached, so it is allowed to inject input
    // (the daemon only denies attached observers). `send_input` is
    // fire-and-forget: the daemon sends no reply on success.
    client
        .send_input(name, b"echo REMUX_MARKER\n")
        .await
        .expect("send_input failed");

    // Poll CaptureScreen until the marker appears. Account for shell echo +
    // command output timing: retry, don't assume instant.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut found = false;
    let mut last_snapshot: Option<TerminalSnapshot> = None;
    while Instant::now() < deadline {
        match client.capture_screen(name).await {
            Ok(snap) => {
                if snapshot_contains(&snap, "REMUX_MARKER") {
                    found = true;
                    break;
                }
                last_snapshot = Some(snap);
            }
            Err(e) => panic!("capture_screen failed: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }

    if !found {
        if let Some(snap) = last_snapshot {
            let rows: Vec<String> = snapshot_rows(&snap)
                .into_iter()
                .map(|r| r.trim_end().to_string())
                .filter(|r| !r.is_empty())
                .collect();
            panic!("REMUX_MARKER never appeared on screen. Last non-empty rows: {rows:?}");
        }
        panic!("REMUX_MARKER never appeared and no snapshot was captured");
    }

    // Clean up the session explicitly (harness also kills the daemon on drop).
    let _ = client.kill_session(name).await;
}
