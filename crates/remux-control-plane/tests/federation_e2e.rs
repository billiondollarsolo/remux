//! AW6 federation end-to-end: TWO real `remuxd` daemons + TWO real
//! `remux-gateway` instances in-process over TLS + the control plane in-process
//! over TLS, exercised with a self-signed-accepting reqwest client.
//!
//! Proves: outbound registration, host listing/health, concurrent federated
//! `GET /cp/v1/sessions` (tagged by host, per-host error isolation), label
//! filtering, intent `resolve` (create + idempotent reuse), and auth.

mod common;

use common::{
    client, create_session_on_gateway, start_control_plane, start_gateway, start_harness,
    ADMIN_TOKEN, GW_TOKEN, REGISTER_TOKEN,
};
use serde_json::json;

/// Register a gateway with the control plane (with labels).
async fn register_host(
    http: &reqwest::Client,
    cp_base: &str,
    name: &str,
    url: &str,
    labels: serde_json::Value,
) {
    let resp = http
        .post(format!("{cp_base}/cp/v1/register"))
        .bearer_auth(REGISTER_TOKEN)
        .json(&json!({ "name": name, "url": url, "labels": labels, "token": GW_TOKEN }))
        .send()
        .await
        .expect("register");
    assert!(
        resp.status().is_success(),
        "register {name} should succeed, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn federation_full_flow() {
    // --- Two daemons, two gateways, two distinctly-named sessions ---
    let harness_a = start_harness().await;
    let harness_b = start_harness().await;
    let gw_a = start_gateway(harness_a.socket_path().to_path_buf()).await;
    let gw_b = start_gateway(harness_b.socket_path().to_path_buf()).await;

    let http = client();
    let _id_a = create_session_on_gateway(&http, &gw_a.base_url, "alpha-session").await;
    let _id_b = create_session_on_gateway(&http, &gw_b.base_url, "beta-session").await;

    // --- Control plane ---
    let cp = start_control_plane().await;
    let cp_base = &cp.base_url;

    // health is public.
    let resp = http
        .get(format!("{cp_base}/cp/v1/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["status"],
        "ok"
    );

    // --- Register both gateways with labels (A: env=dev, B: env=prod) ---
    register_host(
        &http,
        cp_base,
        "host-a",
        &gw_a.base_url,
        json!({"env":"dev"}),
    )
    .await;
    register_host(
        &http,
        cp_base,
        "host-b",
        &gw_b.base_url,
        json!({"env":"prod"}),
    )
    .await;

    // --- GET /cp/v1/hosts shows both healthy ---
    let resp = http
        .get(format!("{cp_base}/cp/v1/hosts"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("hosts");
    assert_eq!(resp.status(), 200);
    let hosts = resp.json::<serde_json::Value>().await.unwrap();
    let arr = hosts.as_array().expect("hosts array");
    assert_eq!(arr.len(), 2, "expected two hosts");
    for h in arr {
        assert_eq!(h["healthy"], true, "host {} should be healthy", h["name"]);
    }
    // Tokens are never exposed in the host view.
    assert!(
        !hosts.to_string().contains(GW_TOKEN),
        "gateway token must never appear in /cp/v1/hosts"
    );

    // --- GET /cp/v1/sessions aggregates BOTH gateways, tagged by host ---
    let resp = http
        .get(format!("{cp_base}/cp/v1/sessions"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("federated sessions");
    assert_eq!(resp.status(), 200);
    let fanout = resp.json::<serde_json::Value>().await.unwrap();
    let rows = fanout.as_array().expect("fanout array");
    assert_eq!(rows.len(), 2, "fan-out should report both hosts");
    let alpha_host = rows
        .iter()
        .find(|r| {
            r["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "alpha-session")
        })
        .expect("alpha-session present");
    assert_eq!(alpha_host["host"], "host-a");
    assert_eq!(alpha_host["ok"], true);
    let beta_host = rows
        .iter()
        .find(|r| {
            r["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "beta-session")
        })
        .expect("beta-session present");
    assert_eq!(beta_host["host"], "host-b");

    // --- Register a THIRD host with a bogus url; it's reported ok:false ---
    register_host(
        &http,
        cp_base,
        "host-bogus",
        "https://127.0.0.1:1", // nothing listening here
        json!({"env":"dev"}),
    )
    .await;
    let resp = http
        .get(format!("{cp_base}/cp/v1/sessions"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("federated sessions with bogus host");
    assert_eq!(resp.status(), 200);
    let fanout = resp.json::<serde_json::Value>().await.unwrap();
    let rows = fanout.as_array().expect("fanout array");
    assert_eq!(rows.len(), 3, "all three hosts reported");
    let bogus = rows
        .iter()
        .find(|r| r["host"] == "host-bogus")
        .expect("bogus host row");
    assert_eq!(bogus["ok"], false, "bogus host must be ok:false");
    assert!(bogus["error"].is_string(), "bogus host carries an error");
    // The good hosts still succeeded (per-host isolation).
    assert!(
        rows.iter()
            .filter(|r| r["host"] != "host-bogus")
            .all(|r| r["ok"] == true),
        "good hosts unaffected by the bogus one"
    );

    // --- Label filter: env=dev returns only host-a (and the bogus dev host) ---
    let resp = http
        .get(format!("{cp_base}/cp/v1/sessions?label=env=dev"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .expect("labeled sessions");
    assert_eq!(resp.status(), 200);
    let fanout = resp.json::<serde_json::Value>().await.unwrap();
    let rows = fanout.as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["host"].as_str().unwrap()).collect();
    assert!(names.contains(&"host-a"), "env=dev includes host-a");
    assert!(!names.contains(&"host-b"), "env=dev excludes host-b (prod)");

    // Deregister the bogus host so the rest of the test sees only the good dev host.
    let resp = http
        .delete(format!("{cp_base}/cp/v1/hosts/host-bogus"))
        .bearer_auth(REGISTER_TOKEN)
        .send()
        .await
        .expect("deregister bogus");
    assert_eq!(resp.status(), 204);

    // --- resolve {env=dev, command:[/bin/sh]} -> host-a + a session id ---
    let resp = http
        .post(format!("{cp_base}/cp/v1/resolve"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({
            "labels": {"env":"dev"},
            "command": ["/bin/sh"],
            "reuse_name": "resolved-one"
        }))
        .send()
        .await
        .expect("resolve create");
    assert!(
        resp.status().is_success(),
        "resolve should succeed, got {}",
        resp.status()
    );
    let resolved = resp.json::<serde_json::Value>().await.unwrap();
    assert_eq!(resolved["host"], "host-a");
    assert_eq!(resolved["created"], true, "first resolve creates");
    assert_eq!(resolved["name"], "resolved-one");
    let resolved_id = resolved["session_id"].as_str().unwrap().to_string();
    assert!(!resolved_id.is_empty());

    // Verify (via the gateway) the session now exists.
    let resp = http
        .get(format!("{}/v1/sessions", gw_a.base_url))
        .bearer_auth(GW_TOKEN)
        .send()
        .await
        .expect("gateway list after resolve");
    let sessions = resp.json::<serde_json::Value>().await.unwrap();
    assert!(
        sessions
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "resolved-one"),
        "resolved session must exist on host-a's daemon"
    );

    // --- resolve again with the same reuse_name -> SAME id, created:false ---
    let resp = http
        .post(format!("{cp_base}/cp/v1/resolve"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({
            "labels": {"env":"dev"},
            "command": ["/bin/sh"],
            "reuse_name": "resolved-one"
        }))
        .send()
        .await
        .expect("resolve reuse");
    assert_eq!(resp.status(), 200, "reuse returns 200, not 201");
    let reused = resp.json::<serde_json::Value>().await.unwrap();
    assert_eq!(reused["created"], false, "second resolve reuses");
    assert_eq!(
        reused["session_id"].as_str().unwrap(),
        resolved_id,
        "reuse must return the same session id (no duplicate)"
    );
}

#[tokio::test]
async fn auth_enforced() {
    let cp = start_control_plane().await;
    let http = client();
    let cp_base = &cp.base_url;

    // Missing admin token -> 401 (unknown/missing credential).
    let resp = http
        .get(format!("{cp_base}/cp/v1/hosts"))
        .send()
        .await
        .expect("hosts no token");
    assert_eq!(resp.status(), 401, "missing token -> 401");

    // Wrong/unknown token -> 401.
    let resp = http
        .get(format!("{cp_base}/cp/v1/hosts"))
        .bearer_auth("wrong")
        .send()
        .await
        .expect("hosts wrong token");
    assert_eq!(resp.status(), 401, "unknown token -> 401");

    // The register token resolves to the `registrar` principal, which LACKS
    // fleet.hosts.read -> known-but-unauthorized -> 403 (distinct from 401).
    let resp = http
        .get(format!("{cp_base}/cp/v1/hosts"))
        .bearer_auth(REGISTER_TOKEN)
        .send()
        .await
        .expect("hosts register token");
    assert_eq!(
        resp.status(),
        403,
        "registrar principal must be forbidden (403) on the fleet API"
    );
    let body: serde_json::Value = resp.json().await.expect("403 json");
    assert_eq!(body["kind"], "forbidden", "403 body: {body}");

    // Register without any token -> 401.
    let resp = http
        .post(format!("{cp_base}/cp/v1/register"))
        .json(&json!({ "name": "x", "url": "https://x:8443", "labels": {}, "token": "t" }))
        .send()
        .await
        .expect("register no token");
    assert_eq!(resp.status(), 401, "register without token -> 401");

    // The admin token resolves to `fleet-admin`, which holds ALL CP permissions
    // including host.register, so it may register (back-compat: a superuser).
    let resp = http
        .post(format!("{cp_base}/cp/v1/register"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "name": "x", "url": "https://x:8443", "labels": {}, "token": "t" }))
        .send()
        .await
        .expect("register admin token");
    assert!(
        resp.status().is_success(),
        "fleet-admin (superuser) may register, got {}",
        resp.status()
    );
    // Clean up the host the admin just registered.
    let _ = http
        .delete(format!("{cp_base}/cp/v1/hosts/x"))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await;

    // Resolve with no matching host -> 404 (admin is authorized; just no host).
    let resp = http
        .post(format!("{cp_base}/cp/v1/resolve"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "labels": {"env":"nope"}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("resolve no host");
    assert_eq!(resp.status(), 404, "resolve with no matching host -> 404");
}

#[tokio::test]
async fn fleet_viewer_can_list_but_not_resolve() {
    use common::start_control_plane_with_auth_config;

    let (cp, viewer_token) = start_control_plane_with_auth_config().await;
    let http = client();
    let cp_base = &cp.base_url;

    // The fleet-viewer principal holds fleet.hosts.read -> GET /cp/v1/hosts ok.
    let resp = http
        .get(format!("{cp_base}/cp/v1/hosts"))
        .bearer_auth(&viewer_token)
        .send()
        .await
        .expect("viewer hosts");
    assert_eq!(resp.status(), 200, "fleet-viewer may list hosts (200)");

    // It also holds fleet.sessions.read -> GET /cp/v1/sessions ok.
    let resp = http
        .get(format!("{cp_base}/cp/v1/sessions"))
        .bearer_auth(&viewer_token)
        .send()
        .await
        .expect("viewer sessions");
    assert_eq!(
        resp.status(),
        200,
        "fleet-viewer may read federated sessions"
    );

    // But it LACKS fleet.resolve -> POST /cp/v1/resolve -> 403.
    let resp = http
        .post(format!("{cp_base}/cp/v1/resolve"))
        .bearer_auth(&viewer_token)
        .json(&json!({ "labels": {"env":"dev"}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("viewer resolve");
    assert_eq!(
        resp.status(),
        403,
        "fleet-viewer must be forbidden on resolve"
    );
    let body: serde_json::Value = resp.json().await.expect("403 json");
    assert_eq!(body["kind"], "forbidden", "403 body: {body}");
}
