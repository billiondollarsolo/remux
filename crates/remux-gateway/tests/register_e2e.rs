//! AW6 gateway auto-registration end-to-end: a real `remuxd` daemon + a real
//! `remux-gateway` (served in-process over TLS) that **registers itself** with
//! an in-process control plane via `remux_gateway::register::spawn`.
//!
//! Proves: outbound registration reaches the control plane (the CP's
//! `GET /cp/v1/hosts` shows the gateway as healthy within a bounded wait), the
//! heartbeat keeps it healthy, and the CP can reach the gateway's `/v1` API
//! (`GET /cp/v1/sessions` fans out to it and returns the gateway's sessions).
//!
//! The control plane is wired in-process using `remux-control-plane` as a
//! dev-dependency, mirroring how `crates/remux-control-plane/tests/common/mod.rs`
//! starts it.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use remux_control_plane::app::AppState as CpState;
use remux_control_plane::auth::AuthConfig as CpAuth;
use remux_control_plane::tls::TlsMaterial as CpTls;
use remux_gateway::app::AppState as GwState;
use remux_gateway::auth::AuthConfig as GwAuth;
use remux_gateway::register::RegisterConfig;
use remux_gateway::tls::TlsMaterial as GwTls;

use common::{ensure_crypto_provider, start_harness, TEST_TOKEN};

const ADMIN_TOKEN: &str = "cp-admin-token-abc123";
const REGISTER_TOKEN: &str = "cp-register-token-xyz789";

/// A running in-process control plane.
struct ControlPlaneHandle {
    base_url: String,
}

/// Start the control plane in-process over TLS on `127.0.0.1:0`.
async fn start_control_plane() -> ControlPlaneHandle {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("cp rustls config");
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind control-plane port");
    let auth = CpAuth::new(ADMIN_TOKEN.to_string(), REGISTER_TOKEN.to_string());
    let state = CpState::new(auth)
        .with_gateway_tls_insecure(true)
        .with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    ControlPlaneHandle {
        base_url: format!("https://{addr}"),
    }
}

/// Start a real gateway in-process over TLS and return its base URL. Unlike the
/// shared `start_gateway` helper, this returns just the URL (the test only needs
/// to point the CP at it).
async fn start_gateway(socket_path: PathBuf) -> String {
    ensure_crypto_provider();
    let tls = GwTls::generate_self_signed().expect("gw self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("gw rustls config");
    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind gateway port");
    let state = GwState::new(socket_path, GwAuth::new(TEST_TOKEN.to_string()));
    tokio::spawn(async move {
        let _ = remux_gateway::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    format!("https://{addr}")
}

/// A reqwest client that accepts the self-signed CP/gateway certs.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build reqwest client")
}

#[tokio::test]
async fn gateway_auto_registers_and_is_reachable() {
    // --- A real daemon + a real gateway with a known session ---
    let harness = start_harness().await;
    let gw_url = start_gateway(harness.socket_path().to_path_buf()).await;

    let http = client();
    // Create a session directly on the gateway so the CP fan-out can see it.
    let resp = http
        .post(format!("{gw_url}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "name": "reg-session", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create session");
    assert_eq!(resp.status(), 201, "gateway create should be 201");

    // --- Control plane ---
    let cp = start_control_plane().await;

    // --- Drive the gateway's OWN auto-registration against the CP (short TTL so
    // the heartbeat fires quickly within the test window). ---
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "dev".to_string());
    let cfg = RegisterConfig {
        cp_url: cp.base_url.clone(),
        register_token: REGISTER_TOKEN.to_string(),
        advertise_url: gw_url.clone(),
        name: "reg-host".to_string(),
        labels,
        gateway_token: TEST_TOKEN.to_string(),
        ttl_secs: 2,
        verification: remux_gateway::peer_tls::PeerVerification::Insecure,
    };
    remux_gateway::register::spawn(cfg, shutdown_rx);

    // --- Within a bounded wait, GET /cp/v1/hosts shows the gateway as healthy ---
    let mut healthy = false;
    for _ in 0..50 {
        let resp = http
            .get(format!("{}/cp/v1/hosts", cp.base_url))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .expect("hosts");
        if resp.status() == 200 {
            let hosts = resp.json::<serde_json::Value>().await.unwrap();
            if let Some(arr) = hosts.as_array() {
                if let Some(h) = arr.iter().find(|h| h["name"] == "reg-host") {
                    if h["healthy"] == true {
                        assert_eq!(h["url"], gw_url, "advertised url recorded");
                        assert_eq!(h["labels"]["env"], "dev", "labels registered");
                        healthy = true;
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "gateway should auto-register and show healthy");

    // --- GET /cp/v1/sessions reaches the gateway and returns its session ---
    let resp = http
        .get(format!("{}/cp/v1/sessions", cp.base_url))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("federated sessions");
    assert_eq!(resp.status(), 200);
    let fanout = resp.json::<serde_json::Value>().await.unwrap();
    let rows = fanout.as_array().expect("fanout array");
    let host_row = rows
        .iter()
        .find(|r| r["host"] == "reg-host")
        .expect("reg-host present in fan-out");
    assert_eq!(host_row["ok"], true, "registered gateway is reachable");
    assert!(
        host_row["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "reg-session"),
        "fan-out returns the gateway's session"
    );

    // --- The heartbeat keeps it healthy past the TTL window ---
    tokio::time::sleep(Duration::from_secs(3)).await;
    let resp = http
        .get(format!("{}/cp/v1/hosts", cp.base_url))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("hosts after ttl");
    let hosts = resp.json::<serde_json::Value>().await.unwrap();
    let h = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "reg-host")
        .expect("reg-host still present");
    assert_eq!(
        h["healthy"], true,
        "heartbeat keeps the host healthy past its TTL"
    );
}
