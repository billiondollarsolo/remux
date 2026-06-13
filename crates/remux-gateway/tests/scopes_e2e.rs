//! Scope-enforcement end-to-end (AW4 hardening): a real `remuxd` + a real
//! `remux-gateway` over TLS, configured with both a read-write and a read-only
//! token.
//!
//! Asserts the enforcement matrix:
//! - read-only token: `GET /v1/sessions` → 200; `POST /v1/sessions` and
//!   `POST .../input` → 403.
//! - read-write token: both work.
//! - bogus token → 401.
//! - read-only token on `/stream` WS → rejected; on `/events` WS → accepted.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    ensure_crypto_provider, start_gateway_with_scopes, start_harness, TEST_READ_TOKEN, TEST_TOKEN,
};
use serde_json::json;
use tokio_tungstenite::Connector;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client")
}

/// A rustls client config that accepts the gateway's self-signed cert.
fn insecure_client_config() -> rustls::ClientConfig {
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _e: &rustls::pki_types::CertificateDer<'_>,
            _i: &[rustls::pki_types::CertificateDer<'_>],
            _s: &rustls::pki_types::ServerName<'_>,
            _o: &[u8],
            _n: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _m: &[u8],
            _c: &rustls::pki_types::CertificateDer<'_>,
            _d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            use rustls::SignatureScheme::*;
            vec![
                RSA_PKCS1_SHA256,
                RSA_PKCS1_SHA384,
                RSA_PKCS1_SHA512,
                ECDSA_NISTP256_SHA256,
                ECDSA_NISTP384_SHA384,
                RSA_PSS_SHA256,
                RSA_PSS_SHA384,
                RSA_PSS_SHA512,
                ED25519,
            ]
        }
    }
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth()
}

#[tokio::test]
async fn read_only_token_can_read_but_not_write() {
    let harness = start_harness().await;
    let gw = start_gateway_with_scopes(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    // --- bogus token -> 401 ---
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth("bogus-token")
        .send()
        .await
        .expect("bogus list");
    assert_eq!(resp.status(), 401, "bogus token must be 401");

    // --- read-only: GET /v1/sessions -> 200 ---
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_READ_TOKEN)
        .send()
        .await
        .expect("ro list");
    assert_eq!(resp.status(), 200, "read token must read sessions (200)");

    // --- read-only: POST /v1/sessions -> 403 (with a forbidden JSON kind) ---
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_READ_TOKEN)
        .json(&json!({ "name": "nope", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("ro create");
    assert_eq!(resp.status(), 403, "read token creating must be 403");
    let body: serde_json::Value = resp.json().await.expect("403 json");
    assert_eq!(body["kind"], "forbidden", "403 body: {body}");

    // --- read-write: POST /v1/sessions -> 201 ---
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "scope-e2e", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("rw create");
    assert_eq!(resp.status(), 201, "read-write token must create (201)");
    let id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // --- read-only: POST .../input -> 403 ---
    let resp = http
        .post(format!("{base}/v1/sessions/{id}/input"))
        .bearer_auth(TEST_READ_TOKEN)
        .json(&json!({ "text": "echo nope\n" }))
        .send()
        .await
        .expect("ro input");
    assert_eq!(resp.status(), 403, "read token sending input must be 403");

    // --- read-write: POST .../input -> 202 ---
    let resp = http
        .post(format!("{base}/v1/sessions/{id}/input"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "text": "echo ok\n" }))
        .send()
        .await
        .expect("rw input");
    assert_eq!(resp.status(), 202, "read-write input must be 202");

    // --- read-only can GET the screen (read scope) -> 200 ---
    let resp = http
        .get(format!("{base}/v1/sessions/{id}/screen"))
        .bearer_auth(TEST_READ_TOKEN)
        .send()
        .await
        .expect("ro screen");
    assert_eq!(resp.status(), 200, "read token reading screen must be 200");

    // Cleanup with the read-write token.
    let _ = http
        .delete(format!("{base}/v1/sessions/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await;
}

#[tokio::test]
async fn read_only_token_rejected_on_stream_accepted_on_events() {
    ensure_crypto_provider();
    let harness = start_harness().await;
    let gw = start_gateway_with_scopes(harness.socket_path().to_path_buf()).await;
    let http = client();

    // Create a session with the read-write token.
    let created: serde_json::Value = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "scope-ws", "command": ["sleep", "30"] }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("created json");
    let id = created["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // --- read-only token on /stream (a write route) is rejected (403 before
    //     the upgrade -> the WS connect fails) ---
    let stream_url = format!(
        "{}/v1/sessions/{id}/stream?token={TEST_READ_TOKEN}",
        gw.ws_base
    );
    let connector = Connector::Rustls(Arc::new(insecure_client_config()));
    let stream_result =
        tokio_tungstenite::connect_async_tls_with_config(&stream_url, None, false, Some(connector))
            .await;
    assert!(
        stream_result.is_err(),
        "read-only token on /stream must be rejected before upgrade"
    );

    // --- read-only token on /events (a read route) is accepted ---
    let events_url = format!(
        "{}/v1/sessions/{id}/events?token={TEST_READ_TOKEN}",
        gw.ws_base
    );
    let connector = Connector::Rustls(Arc::new(insecure_client_config()));
    let events_result =
        tokio_tungstenite::connect_async_tls_with_config(&events_url, None, false, Some(connector))
            .await;
    assert!(
        events_result.is_ok(),
        "read-only token on /events must be accepted"
    );
    // The events stream is dropped here (closing the connection) — accepting the
    // upgrade is the assertion that matters.
    drop(events_result);

    // Cleanup.
    let _ = http
        .delete(format!("{}/v1/sessions/{id}", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await;
}
