//! Phase C PART 1 — control-plane → gateway TLS verification (secure-by-default
//! pinning / CA trust), end-to-end with a real `remuxd` + a real `remux-gateway`
//! in-process over TLS.
//!
//! Proves:
//! - the CP pinning the gateway's ACTUAL self-signed leaf SHA-256 → federation
//!   works (the gateway's sessions fan out, `ok:true`);
//! - a WRONG pin → the gateway is reported unreachable (`ok:false`, a TLS error),
//!   never a panic / never failing the whole request;
//! - trusting the gateway's own self-signed cert AS a CA bundle (`--gateway-ca`)
//!   also works.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{ensure_crypto_provider, start_harness, ADMIN_TOKEN, GW_TOKEN, REGISTER_TOKEN};
use remux_control_plane::app::AppState as CpState;
use remux_control_plane::auth::AuthConfig as CpAuth;
use remux_control_plane::tls::TlsMaterial as CpTls;
use remux_gateway::app::AppState as GwState;
use remux_gateway::auth::AuthConfig as GwAuth;
use remux_gateway::peer_tls::{sha256_fingerprint_of_pem, PeerVerification};
use remux_gateway::tls::TlsMaterial as GwTls;
use serde_json::json;

/// A running in-process gateway, plus its self-signed cert PEM so the CP can pin
/// or CA-trust it.
struct GatewayHandle {
    base_url: String,
    cert_pem: Vec<u8>,
}

/// Start a real gateway in-process over TLS, returning its URL + cert PEM.
async fn start_gateway(socket_path: PathBuf) -> GatewayHandle {
    ensure_crypto_provider();
    let tls = GwTls::generate_self_signed().expect("gw self-signed cert");
    let cert_pem = tls.cert_pem.clone();
    let rustls_config = tls.into_rustls_config().await.expect("gw rustls config");
    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind gateway port");
    let state = GwState::new(socket_path, GwAuth::new(GW_TOKEN.to_string()));
    tokio::spawn(async move {
        let _ = remux_gateway::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    GatewayHandle {
        base_url: format!("https://{addr}"),
        cert_pem,
    }
}

/// Start the control plane in-process with a specific gateway-verification
/// posture. Returns its base URL.
async fn start_cp(verification: PeerVerification) -> String {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("cp rustls config");
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind control-plane port");
    let auth = CpAuth::new(ADMIN_TOKEN.to_string(), REGISTER_TOKEN.to_string());
    let state = CpState::new(auth)
        .with_gateway_verification(verification)
        .with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    format!("https://{addr}")
}

/// A reqwest client that accepts self-signed certs (to talk to the CP itself).
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

async fn register(http: &reqwest::Client, cp: &str, name: &str, url: &str) {
    let resp = http
        .post(format!("{cp}/cp/v1/register"))
        .bearer_auth(REGISTER_TOKEN)
        .json(&json!({ "name": name, "url": url, "labels": {}, "token": GW_TOKEN }))
        .send()
        .await
        .expect("register");
    assert!(resp.status().is_success(), "register {name}");
}

/// Fan out `GET /cp/v1/sessions` and return the row for `host`.
async fn fanout_row(http: &reqwest::Client, cp: &str, host: &str) -> serde_json::Value {
    let resp = http
        .get(format!("{cp}/cp/v1/sessions"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("federated sessions");
    assert_eq!(resp.status(), 200);
    let rows = resp.json::<serde_json::Value>().await.unwrap();
    rows.as_array()
        .unwrap()
        .iter()
        .find(|r| r["host"] == host)
        .cloned()
        .expect("host row present")
}

#[tokio::test]
async fn cp_pins_gateway_self_signed_leaf_and_federation_works() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;

    // Create a session directly on the gateway so the CP fan-out can see it.
    let http = client();
    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(GW_TOKEN)
        .json(&json!({ "name": "pin-session", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create session");
    assert_eq!(resp.status(), 201);

    // Extract the gateway's ACTUAL leaf SHA-256 and pin it.
    let fp = sha256_fingerprint_of_pem(&gw.cert_pem).expect("fingerprint");
    let cp = start_cp(PeerVerification::Pins(vec![fp])).await;
    register(&http, &cp, "pinned-host", &gw.base_url).await;

    let row = fanout_row(&http, &cp, "pinned-host").await;
    assert_eq!(row["ok"], true, "correct pin → federation works: {row}");
    assert!(
        row["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "pin-session"),
        "pinned gateway's session is federated: {row}"
    );
}

#[tokio::test]
async fn cp_wrong_pin_reports_gateway_unreachable_not_panic() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();

    // A syntactically-valid but WRONG pin (never matches the real leaf).
    let wrong = "ab".repeat(32);
    let cp = start_cp(PeerVerification::Pins(vec![wrong])).await;
    register(&http, &cp, "wrong-pin-host", &gw.base_url).await;

    let row = fanout_row(&http, &cp, "wrong-pin-host").await;
    assert_eq!(row["ok"], false, "wrong pin → unreachable: {row}");
    assert!(
        row["error"].is_string(),
        "wrong pin carries a TLS error string (no panic): {row}"
    );
}

#[tokio::test]
async fn cp_trusts_gateway_cert_as_ca_bundle() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();
    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(GW_TOKEN)
        .json(&json!({ "name": "ca-session", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create session");
    assert_eq!(resp.status(), 201);

    // Trust the gateway's own self-signed cert AS the CA bundle.
    let cp = start_cp(PeerVerification::CaBundle(gw.cert_pem.clone())).await;
    register(&http, &cp, "ca-host", &gw.base_url).await;

    let row = fanout_row(&http, &cp, "ca-host").await;
    assert_eq!(
        row["ok"], true,
        "gateway-cert-as-CA → federation works: {row}"
    );
}
