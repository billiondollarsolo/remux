//! Shared helpers for the control-plane end-to-end tests: locate `remuxd`, start
//! daemon harnesses, start real gateways in-process over TLS, and start the
//! control plane in-process over TLS — all on ephemeral loopback ports.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use remux_control_plane::app::AppState as CpState;
use remux_control_plane::auth::AuthConfig as CpAuth;
use remux_control_plane::tls::TlsMaterial as CpTls;
use remux_gateway::app::AppState as GwState;
use remux_gateway::auth::AuthConfig as GwAuth;
use remux_gateway::tls::TlsMaterial as GwTls;
use remux_testkit::DaemonHarness;

/// Admin (fleet API) token the e2e tests authenticate with.
pub const ADMIN_TOKEN: &str = "cp-admin-token-abc123";
/// Register token gateways use to join the control plane.
pub const REGISTER_TOKEN: &str = "cp-register-token-xyz789";
/// The bearer token each gateway is configured with (and hands the CP).
pub const GW_TOKEN: &str = "gateway-token-shared-001";

/// Ensure the process-level rustls crypto provider is installed exactly once.
pub fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Locate the freshly built `remuxd` binary.
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
        "could not find `{exe}`; run `cargo build -p remux-daemon` first (searched from {})",
        manifest_dir.display()
    );
}

/// Start a real `remuxd` under the harness.
pub async fn start_harness() -> DaemonHarness {
    DaemonHarness::start_with_binary(&locate_remuxd())
        .await
        .expect("failed to start remuxd harness")
}

/// A running in-process gateway.
pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub base_url: String,
}

/// Start a real `remux-gateway` in-process over TLS on `127.0.0.1:0`, pointed at
/// `socket_path`, authenticating callers with [`GW_TOKEN`].
pub async fn start_gateway(socket_path: PathBuf) -> GatewayHandle {
    ensure_crypto_provider();
    let tls = GwTls::generate_self_signed().expect("gw self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("gw rustls config");
    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind gateway port");
    let state = GwState::new(socket_path, GwAuth::new(GW_TOKEN.to_string()));
    tokio::spawn(async move {
        let _ = remux_gateway::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    GatewayHandle {
        base_url: format!("https://{addr}"),
        addr,
    }
}

/// A running in-process control plane.
pub struct ControlPlaneHandle {
    pub addr: SocketAddr,
    pub base_url: String,
}

/// Start the control plane in-process over TLS on `127.0.0.1:0` with the fixed
/// [`ADMIN_TOKEN`]/[`REGISTER_TOKEN`]. `gateway_tls_insecure` is `true` (the
/// gateways above are self-signed).
pub async fn start_control_plane() -> ControlPlaneHandle {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("cp rustls config");
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind control-plane port");
    let auth = CpAuth::new(ADMIN_TOKEN.to_string(), REGISTER_TOKEN.to_string());
    let state = CpState::new(auth)
        .with_gateway_tls_insecure(true)
        .with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    ControlPlaneHandle {
        addr,
        base_url: format!("https://{addr}"),
    }
}

/// A bearer token bound to the built-in `fleet-viewer` role via an auth-config
/// file (the principal/permission test). Can list/read but not resolve.
pub const FLEET_VIEWER_TOKEN: &str = "cp-fleet-viewer-token-555";

/// Start the control plane with the back-compat [`ADMIN_TOKEN`]/[`REGISTER_TOKEN`]
/// PLUS an auth-config file binding [`FLEET_VIEWER_TOKEN`] to the built-in
/// `fleet-viewer` role. Returns the handle and the viewer token. The temp config
/// file is removed once loaded.
pub async fn start_control_plane_with_auth_config() -> (ControlPlaneHandle, String) {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let rustls_config = tls.into_rustls_config().await.expect("cp rustls config");
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind control-plane port");

    let path = std::env::temp_dir().join(format!(
        "remux-cp-e2e-auth-{}-{}.toml",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(
        &path,
        format!(
            r#"
                [[tokens]]
                token = "{FLEET_VIEWER_TOKEN}"
                subject = "dashboard"
                roles = ["fleet-viewer"]
            "#
        ),
    )
    .expect("write cp auth-config");
    let auth = CpAuth::from_flags_and_config(
        ADMIN_TOKEN.to_string(),
        REGISTER_TOKEN.to_string(),
        Some(path.as_path()),
    )
    .expect("load cp auth-config");
    let _ = std::fs::remove_file(&path);

    let state = CpState::new(auth)
        .with_gateway_tls_insecure(true)
        .with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve(listener, rustls_config, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (
        ControlPlaneHandle {
            addr,
            base_url: format!("https://{addr}"),
        },
        FLEET_VIEWER_TOKEN.to_string(),
    )
}

/// A reqwest client that accepts self-signed certs (the gateways and CP use them).
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build reqwest client")
}

/// Create a named session directly on a gateway (so the daemon has a session to
/// federate). Returns the created session id.
pub async fn create_session_on_gateway(
    http: &reqwest::Client,
    gw_base: &str,
    name: &str,
) -> String {
    let resp = http
        .post(format!("{gw_base}/v1/sessions"))
        .bearer_auth(GW_TOKEN)
        .json(&serde_json::json!({ "name": name, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create session");
    assert_eq!(resp.status(), 201, "gateway create should be 201");
    resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}
