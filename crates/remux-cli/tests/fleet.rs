//! Integration test for the AW6 v1 client-side fleet fan-out.
//!
//! `remux fleet ls` connects to each configured host over the SSH transport
//! (`ssh <host> remux bridge`) and aggregates their sessions. The only
//! SSH-specific bit is the literal `ssh <host>` prefix; the rest — spawn a child
//! with piped stdio, speak the framed protocol, have the far end pipe to a local
//! `remuxd` — is what this test exercises directly, using the SAME injectable
//! connector seam (`gather_sessions(hosts, connect)`) the production code uses.
//!
//! Setup:
//!   * start TWO real `remuxd` daemons via `DaemonHarness`,
//!   * create a differently-named session in each,
//!   * register three "hosts": one per real daemon (connector spawns
//!     `remux bridge --socket <harnessN.sock>`), plus one pointed at a bogus
//!     socket,
//!   * run `gather_sessions` with that injected connector and assert:
//!       - both real hosts' sessions are aggregated and tagged with the right
//!         host,
//!       - the bogus host is reported as unreachable WITHOUT failing the others.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use remux_cli::cmd::fleet::{build_rows, gather_sessions};
use remux_core::{FleetHost, Request, Response};
use remux_testkit::DaemonHarness;
use tokio::process::Command;

/// Locate the freshly built `remux` CLI binary.
fn locate_remux() -> PathBuf {
    let exe = "remux";
    if let Some(bin) = std::env::var_os("CARGO_BIN_EXE_remux") {
        let p = PathBuf::from(bin);
        if p.exists() {
            return p;
        }
    }
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
    panic!("could not find the `{exe}` binary; run `cargo build -p remux-cli` first");
}

/// Locate the `remuxd` daemon binary (for the harness).
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
    panic!("could not find the `{exe}` binary; run `cargo build -p remux-daemon` first");
}

/// Build the test connector: a host whose `ssh` field is actually a socket path
/// gets `remux bridge --socket <that path>`. This is the local stand-in for
/// `ssh <host> remux bridge`, exercising the identical transport.
fn make_connector(remux: PathBuf) -> impl Fn(&FleetHost) -> Command {
    move |host: &FleetHost| {
        let mut cmd = Command::new(&remux);
        cmd.arg("--socket").arg(&host.ssh).arg("bridge");
        cmd
    }
}

fn host(name: &str, socket: &str) -> FleetHost {
    FleetHost {
        name: name.to_string(),
        ssh: socket.to_string(),
        labels: BTreeMap::new(),
    }
}

/// Create a session named `name` on the daemon behind `socket`, then wait until
/// it is visible via `ls` (registration is async).
async fn create_session(remux: &PathBuf, socket: &PathBuf, name: &str) {
    let mut cmd = Command::new(remux);
    cmd.arg("--socket").arg(socket).arg("bridge");
    let mut client = remux_cli::client::RemuxClient::connect_via_command(cmd)
        .await
        .expect("connect to create session");

    let create = Request::CreateSession(remux_core::CreateSessionRequest {
        name: Some(name.to_string()),
        command: vec!["sleep".to_string(), "60".to_string()],
        cwd: None,
        env: vec![("TERM".to_string(), "xterm-256color".to_string())],
        size: remux_core::TermSize { cols: 80, rows: 24 },
    });
    match client.send_request(create).await.expect("create failed") {
        Response::Created(d) => assert_eq!(d.name, name),
        other => panic!("unexpected create response: {other:?}"),
    }

    for _ in 0..40 {
        if let Ok(Response::SessionList(sessions)) =
            client.send_request(Request::ListSessions).await
        {
            if sessions.iter().any(|s| s.name == name) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("session {name} never became visible");
}

#[tokio::test]
async fn fleet_gather_aggregates_and_isolates_unreachable() {
    let remuxd = locate_remuxd();
    let remux = locate_remux();

    let h1 = DaemonHarness::start_with_binary(&remuxd)
        .await
        .expect("start daemon 1");
    let h2 = DaemonHarness::start_with_binary(&remuxd)
        .await
        .expect("start daemon 2");

    let sock1 = h1.socket_path().to_path_buf();
    let sock2 = h2.socket_path().to_path_buf();

    create_session(&remux, &sock1, "alpha").await;
    create_session(&remux, &sock2, "beta").await;

    // Three registry hosts: two real, one pointed at a bogus socket path.
    let hosts = vec![
        host("box1", sock1.to_str().unwrap()),
        host("box2", sock2.to_str().unwrap()),
        host("dead", "/tmp/remux-fleet-test-nonexistent.sock"),
    ];

    let connector = make_connector(remux.clone());
    let results = gather_sessions(&hosts, connector).await;

    assert_eq!(results.len(), 3, "one result per host, in order");

    // box1 -> alpha
    assert_eq!(results[0].host, "box1");
    let box1 = results[0]
        .result
        .as_ref()
        .expect("box1 should be reachable");
    assert!(box1.iter().any(|s| s.name == "alpha"), "box1 has alpha");

    // box2 -> beta
    assert_eq!(results[1].host, "box2");
    let box2 = results[1]
        .result
        .as_ref()
        .expect("box2 should be reachable");
    assert!(box2.iter().any(|s| s.name == "beta"), "box2 has beta");

    // dead -> error, but the others still succeeded.
    assert_eq!(results[2].host, "dead");
    assert!(
        results[2].result.is_err(),
        "bogus host must be reported as unreachable, got {:?}",
        results[2].result
    );

    // The aggregated rows tag each session with its host and surface the failure.
    let rows = build_rows(&results);
    let alpha_row = rows
        .iter()
        .find(|r| r.name == "alpha")
        .expect("alpha row present");
    assert_eq!(alpha_row.host, "box1");
    let beta_row = rows
        .iter()
        .find(|r| r.name == "beta")
        .expect("beta row present");
    assert_eq!(beta_row.host, "box2");
    let dead_row = rows
        .iter()
        .find(|r| r.host == "dead")
        .expect("dead row present");
    assert_eq!(dead_row.status, "unreachable");
}
