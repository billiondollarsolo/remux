//! Per-request audit-logging end-to-end (AW4 hardening).
//!
//! This is the deliberately-light audit test: it installs a process-global
//! capturing `tracing` subscriber, drives a couple of `/v1` requests against a
//! real gateway, and asserts that:
//! - an audit line is emitted (target `remux_gateway::audit`),
//! - it carries the non-reversible `token_id` hash,
//! - it never contains the raw bearer token.
//!
//! It lives in its own test binary so it can own the process-global subscriber.

mod common;

use std::sync::{Arc, Mutex};

use common::{start_gateway, start_harness, TEST_TOKEN};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// A `MakeWriter` that appends everything written into a shared buffer so the
/// test can inspect emitted log lines.
#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufferWriter {
    type Writer = BufferWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn audit_line_logs_identity_without_raw_token() {
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = BufferWriter(buffer.clone());

    // Capture INFO+ on the audit target into our buffer (process-global; this is
    // the only test in this binary).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("remux_gateway::audit=info"))
        .with_writer(writer)
        .without_time()
        .with_ansi(false)
        .init();

    let harness = start_harness().await;
    let gw = start_gateway(harness.socket_path().to_path_buf()).await;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // An authed request (so an AuthContext + token_id is logged).
    let resp = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .expect("list");
    assert_eq!(resp.status(), 200);

    // A public request (health) — anonymous identity, still audited.
    let resp = http
        .get(format!("{}/v1/health", gw.base_url))
        .send()
        .await
        .expect("health");
    assert_eq!(resp.status(), 200);

    // Give the spawned server task a beat to flush its log line.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let logs = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();

    // An audit line for the request was emitted with the expected fields.
    assert!(
        logs.contains("token_id"),
        "audit log missing token_id field:\n{logs}"
    );
    assert!(
        logs.contains("latency_ms"),
        "audit log missing latency_ms field:\n{logs}"
    );
    assert!(
        logs.contains("path") && logs.contains("/v1/sessions"),
        "audit log missing path:\n{logs}"
    );
    // The raw token must NEVER appear in the logs.
    assert!(
        !logs.contains(TEST_TOKEN),
        "audit log leaked the raw bearer token!\n{logs}"
    );
}
