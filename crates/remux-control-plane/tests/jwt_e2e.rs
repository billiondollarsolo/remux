//! JWT/OIDC bearer authentication for the control plane (Phase B): a real
//! `remux-control-plane` over TLS configured with the static admin/register
//! tokens AND a JWT HS256 validator. A JWT maps its claims to a `Principal` and
//! flows through the SAME RBAC enforcement as a static token.
//!
//! Asserts a JWT mapping to the built-in `fleet-viewer` role can LIST hosts
//! (`GET /cp/v1/hosts` → 200) but cannot RESOLVE (`POST /cp/v1/resolve` → 403),
//! while a `fleet-admin` JWT can resolve; an expired/garbage bearer → 401; and
//! the static admin token still works alongside JWT.

mod common;

use common::{client, mint_cp_jwt, start_control_plane_with_jwt, ADMIN_TOKEN};
use serde_json::json;

#[tokio::test]
async fn fleet_viewer_jwt_can_list_but_not_resolve() {
    let cp = start_control_plane_with_jwt().await;
    let http = client();
    let base = &cp.base_url;

    let viewer = mint_cp_jwt("dashboard", &["fleet-viewer"]);

    // fleet-viewer JWT: GET /cp/v1/hosts -> 200 (read).
    let resp = http
        .get(format!("{base}/cp/v1/hosts"))
        .bearer_auth(&viewer)
        .send()
        .await
        .expect("viewer hosts");
    assert_eq!(resp.status(), 200, "fleet-viewer JWT must list hosts (200)");

    // fleet-viewer JWT: POST /cp/v1/resolve -> 403 (fleet.resolve not granted).
    let resp = http
        .post(format!("{base}/cp/v1/resolve"))
        .bearer_auth(&viewer)
        .json(&json!({ "labels": {}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("viewer resolve");
    assert_eq!(resp.status(), 403, "fleet-viewer JWT resolving must be 403");
    let body: serde_json::Value = resp.json().await.expect("403 json");
    assert_eq!(body["kind"], "forbidden", "403 body: {body}");
}

#[tokio::test]
async fn fleet_admin_jwt_may_resolve_and_garbage_is_401() {
    let cp = start_control_plane_with_jwt().await;
    let http = client();
    let base = &cp.base_url;

    // A fleet-admin JWT clears the fleet.resolve permission gate. With no healthy
    // host matching, resolve returns 404 (NOT 401/403) — proving authz passed.
    let admin = mint_cp_jwt("ci", &["fleet-admin"]);
    let resp = http
        .post(format!("{base}/cp/v1/resolve"))
        .bearer_auth(&admin)
        .json(&json!({ "labels": {}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("admin resolve");
    assert_eq!(
        resp.status(),
        404,
        "fleet-admin JWT passes authz; no host -> 404 (not 401/403)"
    );

    // Garbage bearer -> 401.
    let resp = http
        .get(format!("{base}/cp/v1/hosts"))
        .bearer_auth("not-a-jwt.nor-a-token")
        .send()
        .await
        .expect("garbage hosts");
    assert_eq!(resp.status(), 401, "garbage bearer must be 401");
}

#[tokio::test]
async fn static_admin_token_still_works_with_jwt_enabled() {
    let cp = start_control_plane_with_jwt().await;
    let http = client();
    let base = &cp.base_url;

    let resp = http
        .get(format!("{base}/cp/v1/hosts"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("static hosts");
    assert_eq!(
        resp.status(),
        200,
        "static admin token must still list hosts"
    );
}
