//! End-to-end integration test for AW0: prove the `/v1` DTO decoupling holds
//! against a **real** daemon.
//!
//! Starts a real `remuxd` via `DaemonHarness`, connects a `DaemonConn` over the
//! Unix socket (exercising the real `Hello` handshake), then runs a
//! create -> list -> capture-screen -> kill roundtrip, mapping every protocol
//! result through the public `/v1` DTO layer and asserting on the DTO fields.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use remux_gateway::dto::{ScreenView, SessionView};
use remux_gateway::DaemonConn;
use remux_testkit::DaemonHarness;

use remux_core::{CreateSessionRequest, SessionSelector, TermSize};

/// Locate the freshly built `remuxd` binary (same strategy as the testkit's own
/// integration tests: `cargo test` cwd is the package dir, not the workspace
/// root, so we resolve from `CARGO_MANIFEST_DIR` and honor `CARGO_TARGET_DIR`).
fn locate_remuxd() -> PathBuf {
    let exe = "remuxd";

    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let mut p = PathBuf::from(target_dir);
        p.push("debug");
        p.push(exe);
        if p.exists() {
            return p;
        }
    }

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

async fn start_harness() -> DaemonHarness {
    DaemonHarness::start_with_binary(&locate_remuxd())
        .await
        .expect("failed to start remuxd harness")
}

fn create_req(name: &str, command: &[&str]) -> CreateSessionRequest {
    CreateSessionRequest {
        name: Some(name.to_string()),
        command: command.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: vec![("TERM".to_string(), "xterm-256color".to_string())],
        size: TermSize { cols: 80, rows: 24 },
    }
}

#[tokio::test]
async fn create_list_capture_kill_through_v1_dtos() {
    let harness = start_harness().await;

    // Connect through the gateway's DaemonConn (real Unix socket + handshake).
    let mut conn = DaemonConn::connect(harness.socket_path())
        .await
        .expect("DaemonConn::connect (handshake) failed");

    let name = "gw-roundtrip";

    // --- CREATE: protocol SessionDetails -> public SessionView.
    let details = conn
        .create_session(create_req(name, &["sleep", "30"]))
        .await
        .expect("create_session failed");
    let created_view: SessionView = details.into();
    assert_eq!(created_view.name, name);
    assert_eq!(
        created_view.command,
        vec!["sleep".to_string(), "30".to_string()]
    );
    // The public status is a lowercase string, never the internal enum.
    assert_ne!(created_view.status, "exited");
    // id is a uuid string; created_at is an RFC3339 string.
    assert!(uuid::Uuid::parse_str(&created_view.id).is_ok());
    assert!(chrono::DateTime::parse_from_rfc3339(&created_view.created_at).is_ok());

    // --- LIST: each SessionSummary -> SessionView; assert ours is present and
    // correctly mapped.
    let summaries = conn.list_sessions().await.expect("list_sessions failed");
    let views: Vec<SessionView> = summaries.into_iter().map(SessionView::from).collect();
    let listed = views
        .iter()
        .find(|v| v.name == name)
        .expect("created session not present in list (through DTO layer)");
    assert_eq!(listed.id, created_view.id);
    assert_eq!(listed.command, created_view.command);
    assert!(uuid::Uuid::parse_str(&listed.id).is_ok());

    // --- CAPTURE SCREEN: TerminalSnapshot -> public ScreenView.
    let snapshot = conn
        .capture_screen(SessionSelector::Name(name.to_string()))
        .await
        .expect("capture_screen failed");
    let screen: ScreenView = snapshot.into();
    // The structured-cells contract: dimensions match what we created (80x24)
    // and the cell grid is fully populated (cols*rows).
    assert_eq!(screen.0.cols, 80);
    assert_eq!(screen.0.rows, 24);
    assert_eq!(screen.0.cells.len(), 80 * 24);
    // The DTO serializes transparently to the snapshot JSON shape.
    let json = serde_json::to_value(&screen).expect("serialize ScreenView");
    assert_eq!(json["cols"], serde_json::json!(80));
    assert!(json["cells"].is_array());

    // --- KILL: protocol Ok -> unit. Then confirm via the DTO layer it's gone
    // or Exited.
    conn.kill_session(SessionSelector::Name(name.to_string()), None)
        .await
        .expect("kill_session failed");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut gone_or_exited = false;
    while Instant::now() < deadline {
        let summaries = conn.list_sessions().await.expect("list_sessions failed");
        let views: Vec<SessionView> = summaries.into_iter().map(SessionView::from).collect();
        match views.iter().find(|v| v.name == name) {
            None => {
                gone_or_exited = true;
                break;
            }
            Some(v) if v.status == "exited" => {
                gone_or_exited = true;
                break;
            }
            Some(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        gone_or_exited,
        "session {name:?} was neither removed nor marked exited after kill (via DTO layer)"
    );
}

#[tokio::test]
async fn wait_for_regex_through_observer_stream() {
    // Prove the composed `wait()` verb works end-to-end: it is NOT a single
    // request but an observer-stream predicate (mirroring cmd/wait.rs).
    let harness = start_harness().await;

    let mut conn = DaemonConn::connect(harness.socket_path())
        .await
        .expect("DaemonConn::connect failed");

    let name = "gw-wait";
    let marker = "REMUX_GATEWAY_WAIT_MARKER";
    conn.create_session(create_req(name, &["/bin/sh"]))
        .await
        .expect("create_session failed");

    // Give the shell time to reach its read loop.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Start the observer-based wait FIRST (on its own connection — `wait`
    // consumes its conn), so it is attached before the output is produced. This
    // mirrors how a real caller waits: subscribe, then trigger.
    let wait_conn = DaemonConn::connect(harness.socket_path())
        .await
        .expect("DaemonConn::connect (wait) failed");
    let wait_task = tokio::spawn(wait_conn.wait(
        SessionSelector::Name(name.to_string()),
        remux_gateway::WaitPredicate::Regex(marker.to_string()),
        Some(Duration::from_secs(10)),
    ));

    // Give the observer time to attach, then drive the shell to print the marker.
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.send_input(
        SessionSelector::Name(name.to_string()),
        format!("echo {marker}\n").into_bytes(),
    )
    .await
    .expect("send_input failed");

    let outcome = wait_task
        .await
        .expect("wait task join failed")
        .expect("wait failed");

    assert_eq!(
        outcome.result_str(),
        "matched",
        "expected the marker regex to match; got {outcome:?}"
    );

    // Cleanup.
    let _ = conn
        .kill_session(SessionSelector::Name(name.to_string()), None)
        .await;
}

#[tokio::test]
async fn wait_timeout_yields_timeout_outcome() {
    // A short timeout against a quiet session yields the `timeout` outcome,
    // proving the deadline path of the composed wait loop.
    let harness = start_harness().await;

    let mut conn = DaemonConn::connect(harness.socket_path())
        .await
        .expect("DaemonConn::connect failed");

    let name = "gw-wait-timeout";
    conn.create_session(create_req(name, &["sleep", "30"]))
        .await
        .expect("create_session failed");

    let wait_conn = DaemonConn::connect(harness.socket_path())
        .await
        .expect("DaemonConn::connect (wait) failed");

    let outcome = wait_conn
        .wait(
            SessionSelector::Name(name.to_string()),
            remux_gateway::WaitPredicate::Regex("THIS_WILL_NEVER_APPEAR".to_string()),
            Some(Duration::from_millis(300)),
        )
        .await
        .expect("wait failed");

    assert_eq!(outcome.result_str(), "timeout", "got {outcome:?}");

    let _ = conn
        .kill_session(SessionSelector::Name(name.to_string()), None)
        .await;
}
