//! End-to-end test of the SSH remote transport, *without* a real SSH server.
//!
//! The remote path is: client -> `ssh <host> remux bridge` -> remote
//! `remuxd.sock`. The only SSH-specific part is the literal `ssh <host>`
//! prefix; everything else (spawn a child with piped stdin/stdout, speak the
//! framed protocol over it, and have the far end pipe those bytes into a local
//! `remuxd` Unix socket) is identical locally.
//!
//! This test simulates that by:
//!   1. starting a real `remuxd` via [`DaemonHarness`],
//!   2. spawning the `remux` binary's hidden `bridge` subcommand as a child,
//!      pointed at the harness socket with `--socket`,
//!   3. connecting a real [`RemuxClient`] over that child's stdin/stdout using
//!      [`RemuxClient::connect_via_command`] — the *exact* code path
//!      `connect_remote` uses, minus the `ssh` prefix,
//!   4. running a create -> ls -> kill roundtrip through the bridged transport.
//!
//! This proves the generic boxed-halves transport and the `bridge` subcommand
//! work together over a non-Unix-socket duplex.

use std::path::PathBuf;
use std::time::Duration;

use remux_cli::client::RemuxClient;
use remux_core::{Request, Response, SessionSelector};
use remux_testkit::DaemonHarness;
use tokio::process::Command;

/// Locate the freshly built `remux` binary (the CLI), walking up from the
/// package manifest dir to the workspace `target/{debug,release}`.
fn locate_remux() -> PathBuf {
    let exe = "remux";

    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let mut p = PathBuf::from(target_dir);
        p.push("debug");
        p.push(exe);
        if p.exists() {
            return p;
        }
    }

    // Cargo sets this to the directory of the crate currently being tested.
    if let Some(bin) = std::env::var_os("CARGO_BIN_EXE_remux") {
        let p = PathBuf::from(bin);
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

/// Locate the `remuxd` daemon binary the same way (for the harness).
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

#[tokio::test]
async fn bridge_transport_roundtrip() {
    let remuxd = locate_remuxd();
    let harness = DaemonHarness::start_with_binary(&remuxd)
        .await
        .expect("failed to start remuxd harness");

    let remux = locate_remux();

    // Build the command the client will spawn: `remux bridge --socket <sock>`.
    // This is the local stand-in for `ssh <host> remux bridge`. We pass the
    // harness socket explicitly so the bridge connects to *this* daemon.
    let mut cmd = Command::new(&remux);
    cmd.arg("--socket").arg(harness.socket_path()).arg("bridge");

    // Connect the real client over the bridge child's stdin/stdout. This is the
    // same transport `connect_remote` builds, exercised end-to-end.
    let mut client = RemuxClient::connect_via_command(cmd)
        .await
        .expect("failed to connect RemuxClient over the bridge");

    // --- create ---
    let create = Request::CreateSession(remux_core::CreateSessionRequest {
        name: Some("bridged".to_string()),
        command: vec!["sleep".to_string(), "30".to_string()],
        cwd: None,
        env: vec![("TERM".to_string(), "xterm-256color".to_string())],
        size: remux_core::TermSize { cols: 80, rows: 24 },
    });
    let created = client
        .send_request(create)
        .await
        .expect("create request failed over bridge");
    match created {
        Response::Created(details) => assert_eq!(details.name, "bridged"),
        other => panic!("unexpected create response: {other:?}"),
    }

    // --- ls --- (retry briefly to be robust against registration timing)
    let mut found = false;
    for _ in 0..20 {
        let resp = client
            .send_request(Request::ListSessions)
            .await
            .expect("ls request failed over bridge");
        if let Response::SessionList(sessions) = resp {
            if sessions.iter().any(|s| s.name == "bridged") {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(found, "created session not visible via bridged ls");

    // --- kill ---
    let kill = Request::KillSession {
        session: SessionSelector::Name("bridged".to_string()),
        signal: None,
    };
    let killed = client
        .send_request(kill)
        .await
        .expect("kill request failed over bridge");
    assert!(
        matches!(killed, Response::Ok),
        "unexpected kill response: {killed:?}"
    );

    drop(client);
}
