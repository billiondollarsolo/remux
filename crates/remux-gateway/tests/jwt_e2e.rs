//! JWT/OIDC bearer authentication end-to-end (Phase B): a real `remuxd` + a real
//! `remux-gateway` over TLS, configured with a static admin token AND a JWT HS256
//! validator. A JWT that validates maps its claims to a `Principal` and flows
//! through the EXACT SAME RBAC enforcement as a static token.
//!
//! Asserts:
//! - an `operator`-role JWT (`sub:"alice"`) can create + input (write) and read.
//! - a `viewer`-role JWT gets 403 on write and 200 on read (same 401/403
//!   semantics as static tokens).
//! - an expired JWT and a garbage bearer both → 401.
//! - the static `TEST_TOKEN` still works alongside JWT.
//!
//! The JWKS-URL fetch/cache/refresh path is covered by the `remux-authz`
//! `parse_jwks` unit tests + the `jwt_service` unit tests; the static key path is
//! exercised here end-to-end.

mod common;

use common::{mint_jwt, mint_jwt_with, start_gateway_with_jwt, start_harness, TEST_TOKEN};
use serde_json::json;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client")
}

#[tokio::test]
async fn operator_jwt_can_read_and_write() {
    let harness = start_harness().await;
    let gw = start_gateway_with_jwt(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    let jwt = mint_jwt("alice", &["operator"]);

    // operator JWT: GET /v1/sessions -> 200
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(&jwt)
        .send()
        .await
        .expect("operator list");
    assert_eq!(resp.status(), 200, "operator JWT must read sessions");

    // operator JWT: POST /v1/sessions -> 201 (write allowed)
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "jwt-sess", "command": ["/bin/cat"] }))
        .send()
        .await
        .expect("operator create");
    assert_eq!(resp.status(), 201, "operator JWT must create a session");
    let created: serde_json::Value = resp.json().await.expect("created json");
    let id = created["id"].as_str().expect("session id").to_string();

    // operator JWT: POST .../input -> 202 (write allowed)
    let resp = http
        .post(format!("{base}/v1/sessions/{id}/input"))
        .bearer_auth(&jwt)
        .json(&json!({ "text": "hello\\n" }))
        .send()
        .await
        .expect("operator input");
    assert_eq!(resp.status(), 202, "operator JWT must send input");
}

#[tokio::test]
async fn viewer_jwt_reads_but_cannot_write() {
    let harness = start_harness().await;
    let gw = start_gateway_with_jwt(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    let jwt = mint_jwt("bob", &["viewer"]);

    // viewer JWT: GET /v1/sessions -> 200 (read)
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(&jwt)
        .send()
        .await
        .expect("viewer list");
    assert_eq!(resp.status(), 200, "viewer JWT must read (200)");

    // viewer JWT: POST /v1/sessions -> 403 (write forbidden, same as static)
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&jwt)
        .json(&json!({ "name": "nope", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("viewer create");
    assert_eq!(resp.status(), 403, "viewer JWT writing must be 403");
    let body: serde_json::Value = resp.json().await.expect("403 json");
    assert_eq!(body["kind"], "forbidden", "403 body: {body}");
}

#[tokio::test]
async fn expired_and_garbage_jwt_are_401() {
    let harness = start_harness().await;
    let gw = start_gateway_with_jwt(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    // Expired JWT (exp 60s in the past) -> 401.
    let expired = mint_jwt_with("alice", &["operator"], -60);
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(&expired)
        .send()
        .await
        .expect("expired list");
    assert_eq!(resp.status(), 401, "expired JWT must be 401");

    // Garbage bearer (not a static token, not a valid JWT) -> 401.
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth("not-a-jwt.and-not-a-token")
        .send()
        .await
        .expect("garbage list");
    assert_eq!(resp.status(), 401, "garbage bearer must be 401");
}

#[tokio::test]
async fn static_token_still_works_alongside_jwt() {
    let harness = start_harness().await;
    let gw = start_gateway_with_jwt(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    // The static admin TEST_TOKEN keeps full (admin) access even with JWT enabled.
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("static list");
    assert_eq!(resp.status(), 200, "static admin token must still read");

    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "static-sess", "command": ["/bin/cat"] }))
        .send()
        .await
        .expect("static create");
    assert_eq!(resp.status(), 201, "static admin token must still create");
}
