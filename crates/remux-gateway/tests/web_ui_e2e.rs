//! AW5 end-to-end: the gateway serves the built-in xterm.js browser client over
//! TLS, on routes OUTSIDE the `/v1` bearer-auth group, and `--no-web-ui` disables
//! it (404).
//!
//! NOTE: actual in-browser rendering / WS interaction is not automatable here.
//! These tests assert the gateway *serves the client assets correctly* (status,
//! content-type, and known markers in the body); the JS<->WS wiring is exercised
//! by a human or a browser-driver, not by these Rust integration tests.

mod common;

use common::{start_gateway, start_gateway_no_web_ui, start_harness};

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client")
}

#[tokio::test]
async fn index_served_unauthenticated_with_markers() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();

    // `GET /` is public (no bearer token) and returns the HTML shell.
    let resp = http
        .get(format!("{}/", gw.base_url))
        .send()
        .await
        .expect("index request");
    assert_eq!(resp.status(), 200, "GET / should be 200");
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.starts_with("text/html"),
        "GET / content-type should be text/html, got {ctype}"
    );
    let body = resp.text().await.expect("index body");
    assert!(
        body.contains("<title>remux"),
        "index missing <title>remux marker"
    );
    assert!(body.contains("xterm"), "index missing xterm.js marker");
}

#[tokio::test]
async fn app_js_served_with_js_content_type() {
    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = client();

    let resp = http
        .get(format!("{}/app.js", gw.base_url))
        .send()
        .await
        .expect("app.js request");
    assert_eq!(resp.status(), 200, "GET /app.js should be 200");
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.contains("javascript"),
        "app.js content-type should be a JS type, got {ctype}"
    );
    let body = resp.text().await.expect("app.js body");
    assert!(
        body.contains("/v1/sessions"),
        "app.js should reference the /v1/sessions endpoint"
    );

    // style.css is served with a CSS content type too.
    let resp = http
        .get(format!("{}/style.css", gw.base_url))
        .send()
        .await
        .expect("style.css request");
    assert_eq!(resp.status(), 200, "GET /style.css should be 200");
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.contains("text/css"),
        "style.css content-type should be text/css, got {ctype}"
    );
}

#[tokio::test]
async fn no_web_ui_returns_404() {
    let harness = start_harness().await;
    let gw = start_gateway_no_web_ui(harness.socket_path().to_path_buf()).await;
    let http = client();

    for path in ["/", "/app.js", "/style.css"] {
        let resp = http
            .get(format!("{}{path}", gw.base_url))
            .send()
            .await
            .expect("request with web ui disabled");
        assert_eq!(
            resp.status(),
            404,
            "with --no-web-ui, GET {path} should be 404"
        );
    }

    // The /v1 API is unaffected by --no-web-ui.
    let resp = http
        .get(format!("{}/v1/health", gw.base_url))
        .send()
        .await
        .expect("health with web ui disabled");
    assert_eq!(resp.status(), 200, "/v1/health unaffected by --no-web-ui");
}
