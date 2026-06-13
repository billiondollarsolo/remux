//! Phase C PART 1 — gateway → control-plane register-client TLS verification
//! (secure-by-default pinning), end-to-end with a real `remuxd` + a real
//! `remux-gateway` registering itself with an in-process control plane.
//!
//! Proves:
//! - auto-registration SUCCEEDS when the gateway pins the CP's ACTUAL self-signed
//!   leaf SHA-256 (the CP's `GET /cp/v1/hosts` shows the gateway healthy);
//! - auto-registration FAILS CLOSED with a wrong pin (the host never appears) and
//!   the gateway keeps serving its `/v1` API — no crash;
//! - CA-bundle trust of the CP's own self-signed cert also registers successfully.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use remux_control_plane::app::AppState as CpState;
use remux_control_plane::auth::AuthConfig as CpAuth;
use remux_control_plane::tls::TlsMaterial as CpTls;
use remux_gateway::app::AppState as GwState;
use remux_gateway::auth::AuthConfig as GwAuth;
use remux_gateway::peer_tls::{sha256_fingerprint_of_pem, PeerVerification};
use remux_gateway::register::RegisterConfig;
use remux_gateway::tls::TlsMaterial as GwTls;

use common::{ensure_crypto_provider, start_harness, TEST_TOKEN};

const ADMIN_TOKEN: &str = "cp-admin-token-abc123";
const REGISTER_TOKEN: &str = "cp-register-token-xyz789";

/// A running in-process control plane, with its self-signed cert PEM so the
/// gateway can pin / CA-trust it.
struct CpHandle {
    base_url: String,
    cert_pem: Vec<u8>,
}

async fn start_cp() -> CpHandle {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let cert_pem = tls.cert_pem.clone();
    let rustls_config = tls.into_rustls_config().await.expect("cp rustls config");
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind control-plane port");
    let auth = CpAuth::new(ADMIN_TOKEN.to_string(), REGISTER_TOKEN.to_string());
    // The CP itself trusts the gateway self-signed cert (not under test here).
    let state = CpState::new(auth)
        .with_gateway_tls_insecure(true)
        .with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    CpHandle {
        base_url: format!("https://{addr}"),
        cert_pem,
    }
}

/// Start a real gateway in-process over TLS, returning its URL.
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

fn reg_cfg(
    cp_url: &str,
    gw_url: &str,
    name: &str,
    verification: PeerVerification,
) -> RegisterConfig {
    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "dev".to_string());
    RegisterConfig {
        cp_url: cp_url.to_string(),
        register_token: REGISTER_TOKEN.to_string(),
        advertise_url: gw_url.to_string(),
        name: name.to_string(),
        labels,
        gateway_token: TEST_TOKEN.to_string(),
        ttl_secs: 2,
        verification,
    }
}

/// Within a bounded wait, is `name` listed by the CP?
async fn host_appears(http: &reqwest::Client, cp_url: &str, name: &str, tries: usize) -> bool {
    for _ in 0..tries {
        let resp = http
            .get(format!("{cp_url}/cp/v1/hosts"))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .expect("hosts");
        let hosts = resp.json::<serde_json::Value>().await.unwrap();
        if hosts
            .as_array()
            .map(|a| a.iter().any(|h| h["name"] == name))
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn register_succeeds_with_correct_cp_pin() {
    let harness = start_harness().await;
    let gw_url = start_gateway(harness.socket_path().to_path_buf()).await;
    let cp = start_cp().await;
    let http = client();

    // Pin the CP's actual leaf → registration succeeds.
    let fp = sha256_fingerprint_of_pem(&cp.cert_pem).expect("fingerprint");
    let (_tx, rx) = tokio::sync::watch::channel(false);
    remux_gateway::register::spawn(
        reg_cfg(
            &cp.base_url,
            &gw_url,
            "good-pin",
            PeerVerification::Pins(vec![fp]),
        ),
        rx,
    );

    assert!(
        host_appears(&http, &cp.base_url, "good-pin", 50).await,
        "correct CP pin → gateway auto-registers"
    );
}

#[tokio::test]
async fn register_fails_closed_with_wrong_cp_pin_without_crashing_gateway() {
    let harness = start_harness().await;
    let gw_url = start_gateway(harness.socket_path().to_path_buf()).await;
    let cp = start_cp().await;
    let http = client();

    // A wrong pin → registration can never succeed (TLS error every attempt).
    let wrong = "cd".repeat(32);
    let (_tx, rx) = tokio::sync::watch::channel(false);
    remux_gateway::register::spawn(
        reg_cfg(
            &cp.base_url,
            &gw_url,
            "bad-pin",
            PeerVerification::Pins(vec![wrong]),
        ),
        rx,
    );

    assert!(
        !host_appears(&http, &cp.base_url, "bad-pin", 15).await,
        "wrong CP pin → registration fails closed (host never appears)"
    );

    // The gateway keeps serving its /v1 API regardless (no crash).
    let resp = http
        .get(format!("{gw_url}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("gateway still serving");
    assert_eq!(
        resp.status(),
        200,
        "gateway unaffected by failed registration"
    );
}

#[tokio::test]
async fn register_succeeds_with_cp_cert_as_ca() {
    let harness = start_harness().await;
    let gw_url = start_gateway(harness.socket_path().to_path_buf()).await;
    let cp = start_cp().await;
    let http = client();

    let (_tx, rx) = tokio::sync::watch::channel(false);
    remux_gateway::register::spawn(
        reg_cfg(
            &cp.base_url,
            &gw_url,
            "ca-host",
            PeerVerification::CaBundle(cp.cert_pem.clone()),
        ),
        rx,
    );

    assert!(
        host_appears(&http, &cp.base_url, "ca-host", 50).await,
        "CP-cert-as-CA → gateway auto-registers"
    );
}
