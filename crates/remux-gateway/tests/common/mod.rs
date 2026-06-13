//! Shared helpers for the gateway end-to-end tests: locate `remuxd`, start the
//! daemon harness, and spin up the real gateway over TLS on an ephemeral
//! loopback port with a known token.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;

use remux_gateway::app::AppState;
use remux_gateway::auth::AuthConfig;
use remux_gateway::tls::TlsMaterial;
use remux_testkit::DaemonHarness;

/// The fixed read-write bearer token the e2e tests authenticate with.
pub const TEST_TOKEN: &str = "test-gateway-token-abc123";

/// The fixed read-only bearer token used by the scope-enforcement tests.
pub const TEST_READ_TOKEN: &str = "test-gateway-read-token-xyz789";

/// Locate the freshly built `remuxd` binary (mirrors `daemon_conn_e2e.rs`).
pub fn locate_remuxd() -> PathBuf {
    let exe = "remuxd";

    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let mut p = PathBuf::from(target_dir);
        p.push("debug");
        p.push(exe);
        if p.exists() {
            return p;
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest_dir.ancestors() {
        for profile in ["debug", "release"] {
            let candidate = ancestor.join("target").join(profile).join(exe);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    panic!(
        "could not find `{exe}`; run `cargo build -p remux-daemon` first (searched \
         CARGO_TARGET_DIR and target/{{debug,release}} from {})",
        manifest_dir.display()
    );
}

/// Start a real `remuxd` under the harness.
pub async fn start_harness() -> DaemonHarness {
    DaemonHarness::start_with_binary(&locate_remuxd())
        .await
        .expect("failed to start remuxd harness")
}

/// A running in-process gateway: its base URL, the cert PEM (so a WS client can
/// trust it), and the bound address. The server task is detached and torn down
/// when the test process exits.
pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub base_url: String,
    pub ws_base: String,
    pub cert_pem: Vec<u8>,
}

/// Ensure the process-level rustls crypto provider is installed exactly once.
pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Start the gateway in-process, bound to `127.0.0.1:0`, with a generated
/// self-signed cert and the fixed read-write [`TEST_TOKEN`] (no read-only token),
/// pointed at `socket_path`.
pub async fn start_gateway(socket_path: PathBuf) -> GatewayHandle {
    start_gateway_with_auth(socket_path, AuthConfig::new(TEST_TOKEN.to_string())).await
}

/// Start the gateway with both the read-write [`TEST_TOKEN`] and the read-only
/// [`TEST_READ_TOKEN`] configured (for scope-enforcement tests).
pub async fn start_gateway_with_scopes(socket_path: PathBuf) -> GatewayHandle {
    let auth = AuthConfig::with_scopes(TEST_TOKEN.to_string(), Some(TEST_READ_TOKEN.to_string()));
    start_gateway_with_auth(socket_path, auth).await
}

/// Start the gateway with an explicit [`AuthConfig`].
pub async fn start_gateway_with_auth(socket_path: PathBuf, auth: AuthConfig) -> GatewayHandle {
    ensure_crypto_provider();

    let tls = TlsMaterial::generate_self_signed().expect("generate self-signed cert");
    let cert_pem = tls.cert_pem.clone();
    let rustls_config = tls.into_rustls_config().await.expect("build rustls config");

    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind ephemeral loopback port");

    let state = AppState::new(socket_path, auth);

    tokio::spawn(async move {
        let _ = remux_gateway::server::serve(listener, rustls_config, state).await;
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    GatewayHandle {
        base_url: format!("https://{addr}"),
        ws_base: format!("wss://{addr}"),
        cert_pem,
        addr,
    }
}
