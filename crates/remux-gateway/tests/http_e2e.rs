//! AW2 REST end-to-end: a real `remuxd` + a real `remux-gateway` over TLS.
//!
//! Exercises the secured `/v1` surface with a `reqwest` client that accepts the
//! self-signed cert: auth enforcement, create/list/input/screen/delete, and the
//! public-health endpoint.

mod common;

use std::time::Duration;

use common::{start_gateway, start_harness, TEST_TOKEN};
use serde_json::json;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client")
}

#[tokio::test]
async fn rest_full_lifecycle_over_tls() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();

    let base = &gw.base_url;

    // --- health is public (no auth) ---
    let resp = http
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .expect("health request");
    assert_eq!(resp.status(), 200, "health should be 200");
    let body: serde_json::Value = resp.json().await.expect("health json");
    assert_eq!(body["status"], "ok");

    // --- 401 without a token ---
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .expect("unauthed list");
    assert_eq!(resp.status(), 401, "missing token must be 401");

    // --- 401 with a wrong token ---
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("wrong-token list");
    assert_eq!(resp.status(), 401, "wrong token must be 401");

    // --- create a session (201 + SessionView) ---
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({
            "name": "gw-e2e",
            "command": ["/bin/sh"],
            "size": { "cols": 80, "rows": 24 }
        }))
        .send()
        .await
        .expect("create session");
    assert_eq!(resp.status(), 201, "create should be 201");
    let created: serde_json::Value = resp.json().await.expect("created json");
    assert_eq!(created["name"], "gw-e2e");
    assert!(created["id"].is_string());
    assert!(uuid::Uuid::parse_str(created["id"].as_str().unwrap()).is_ok());
    let id = created["id"].as_str().unwrap().to_string();

    // --- list contains it ---
    let resp = http
        .get(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("list sessions");
    assert_eq!(resp.status(), 200);
    let list: serde_json::Value = resp.json().await.expect("list json");
    let arr = list.as_array().expect("list is array");
    assert!(
        arr.iter().any(|v| v["name"] == "gw-e2e"),
        "created session missing from list"
    );

    // --- GET by id ---
    let resp = http
        .get(format!("{base}/v1/sessions/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get by id");
    assert_eq!(resp.status(), 200);
    let got: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(got["id"], id);

    // --- unknown session -> 404 ---
    let resp = http
        .get(format!("{base}/v1/sessions/does-not-exist"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("get missing");
    assert_eq!(resp.status(), 404, "unknown session should be 404");

    // Let the shell reach its read loop.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- send input (202) ---
    let resp = http
        .post(format!("{base}/v1/sessions/{id}/input"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "text": "echo GATEWAY_OK\n" }))
        .send()
        .await
        .expect("send input");
    assert_eq!(resp.status(), 202, "input should be 202");

    // --- poll the screen until GATEWAY_OK appears (bounded ~3s) ---
    let mut found = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let resp = http
            .get(format!("{base}/v1/sessions/{id}/screen"))
            .bearer_auth(TEST_TOKEN)
            .send()
            .await
            .expect("screen");
        assert_eq!(resp.status(), 200);
        let screen: serde_json::Value = resp.json().await.expect("screen json");
        // Reconstruct rows from the flat cell grid and look for our marker.
        if screen_contains(&screen, "GATEWAY_OK") {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(found, "GATEWAY_OK never appeared in the structured screen");

    // --- scrollback works ---
    let resp = http
        .get(format!("{base}/v1/sessions/{id}/scrollback?lines=50"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("scrollback");
    assert_eq!(resp.status(), 200);
    let sb: serde_json::Value = resp.json().await.unwrap();
    assert!(sb["text"].is_string());

    // --- delete (204) ---
    let resp = http
        .delete(format!("{base}/v1/sessions/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("delete");
    assert_eq!(resp.status(), 204, "delete should be 204");
}

#[tokio::test]
async fn rest_wait_endpoint_over_tls() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();
    let base = &gw.base_url;

    // Create a shell session.
    let resp = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "name": "gw-wait-e2e", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status(), 201);
    let id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let marker = "WAIT_E2E_MARKER";

    // Start the wait first (regex), with a bounded timeout.
    let wait_url = format!("{base}/v1/sessions/{id}/wait?timeout_ms=10000");
    let http2 = http.clone();
    let wait_task = tokio::spawn(async move {
        http2
            .post(&wait_url)
            .bearer_auth(TEST_TOKEN)
            .json(&json!({ "kind": "regex", "pattern": "WAIT_E2E_MARKER" }))
            .send()
            .await
            .expect("wait send")
            .json::<serde_json::Value>()
            .await
            .expect("wait json")
    });

    // Give the observer time to attach, then drive the marker.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let resp = http
        .post(format!("{base}/v1/sessions/{id}/input"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({ "text": format!("echo {marker}\n") }))
        .send()
        .await
        .expect("input");
    assert_eq!(resp.status(), 202);

    let result = wait_task.await.expect("wait join");
    assert_eq!(result["result"], "matched", "wait result: {result}");

    // Cleanup.
    let _ = http
        .delete(format!("{base}/v1/sessions/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await;
}

/// Reconstruct screen rows from the flat `cells` grid and check for a substring.
fn screen_contains(screen: &serde_json::Value, needle: &str) -> bool {
    let cols = screen["cols"].as_u64().unwrap_or(0) as usize;
    let cells = match screen["cells"].as_array() {
        Some(c) => c,
        None => return false,
    };
    if cols == 0 {
        return false;
    }
    let mut text = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 && i % cols == 0 {
            text.push('\n');
        }
        if let Some(ch) = cell["ch"].as_str() {
            text.push_str(ch);
        }
    }
    text.contains(needle)
}
