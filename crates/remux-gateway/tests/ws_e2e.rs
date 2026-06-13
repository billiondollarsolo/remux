//! AW3 WebSocket end-to-end: connect to `/v1/sessions/{id}/stream` over `wss://`
//! with the token in the query string, send input as a binary frame, and assert
//! the echoed output appears. Bounded by timeouts; tears down cleanly.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ensure_crypto_provider, start_gateway, start_harness, TEST_TOKEN};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

/// A rustls client config that accepts any server cert (the gateway's
/// self-signed cert), the WS analogue of reqwest's `danger_accept_invalid_certs`.
fn insecure_client_config() -> rustls::ClientConfig {
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
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
async fn ws_stream_echoes_input_over_wss() {
    ensure_crypto_provider();

    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // Create a shell session via REST.
    let created: serde_json::Value = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "ws-e2e", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("created json");
    let id = created["id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Connect to /stream over wss with the token in the QUERY string (the
    // browser-friendly path that can't use the Authorization header).
    let url = format!("{}/v1/sessions/{id}/stream?token={TEST_TOKEN}", gw.ws_base);
    let connector = Connector::Rustls(Arc::new(insecure_client_config()));
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector))
            .await
            .expect("wss connect to /stream");

    // Give the control attach a beat to register, then send input as a BINARY frame.
    tokio::time::sleep(Duration::from_millis(200)).await;
    ws.send(Message::Binary(b"echo WSOK\n".to_vec()))
        .await
        .expect("send binary input");

    // Read output frames until WSOK appears (bounded ~4s).
    let mut acc: Vec<u8> = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Binary(bytes)) => {
                    acc.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&acc).contains("WSOK") {
                        return true;
                    }
                }
                Ok(Message::Close(_)) => return false,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    assert!(
        found,
        "WSOK never appeared in the accumulated /stream output: {:?}",
        String::from_utf8_lossy(&acc)
    );

    // Clean teardown.
    let _ = ws.close(None).await;
    let _ = http
        .delete(format!("{}/v1/sessions/{id}", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await;
}

#[tokio::test]
async fn ws_stream_requires_token() {
    ensure_crypto_provider();

    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;

    // No token in the query and no Authorization header -> the upgrade must fail
    // (the auth middleware returns 401 before the WS upgrade completes).
    let url = format!("{}/v1/sessions/whatever/stream", gw.ws_base);
    let connector = Connector::Rustls(Arc::new(insecure_client_config()));
    let result =
        tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector)).await;
    assert!(
        result.is_err(),
        "WS connect without a token must be rejected"
    );
}

#[tokio::test]
async fn ws_events_emits_structured_json() {
    ensure_crypto_provider();

    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // A short-lived command so we get a SessionExited -> {"type":"exited"} event.
    let created: serde_json::Value = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "ws-events-e2e", "command": ["sleep", "30"] }))
        .send()
        .await
        .expect("create")
        .json()
        .await
        .expect("created json");
    let id = created["id"].as_str().unwrap().to_string();

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Connect to /events (token via query).
    let url = format!("{}/v1/sessions/{id}/events?token={TEST_TOKEN}", gw.ws_base);
    let connector = Connector::Rustls(Arc::new(insecure_client_config()));
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector))
            .await
            .expect("wss connect to /events");

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Kill the session to provoke an `exited` event on the structured channel.
    let _ = http
        .delete(format!("{}/v1/sessions/{id}", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await;

    let got_exit = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        // Any structured event proves the JSON channel works; an
                        // `exited` event is the one we provoked.
                        if v["type"] == "exited" {
                            return true;
                        }
                    }
                }
                Ok(Message::Close(_)) | Err(_) => return false,
                Ok(_) => {}
            }
        }
        false
    })
    .await
    .unwrap_or(false);

    assert!(
        got_exit,
        "did not receive a structured `exited` event on /events"
    );

    let _ = ws.close(None).await;
}
