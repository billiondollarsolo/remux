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

/// Start the gateway with both the admin [`TEST_TOKEN`] (→ `admin` role) and the
/// read-only [`TEST_READ_TOKEN`] (→ `viewer` role) configured (for the
/// principal/permission-enforcement tests).
pub async fn start_gateway_with_scopes(socket_path: PathBuf) -> GatewayHandle {
    let auth = AuthConfig::with_scopes(TEST_TOKEN.to_string(), Some(TEST_READ_TOKEN.to_string()));
    start_gateway_with_auth(socket_path, auth).await
}

/// The bearer token bound to a custom `deployer` role via an auth-config file
/// (the principal-via-config test). The deployer can create + input + read, but
/// NOT kill.
pub const TEST_DEPLOYER_TOKEN: &str = "test-gateway-deployer-token-777";

/// Start the gateway with the admin [`TEST_TOKEN`] PLUS an auth-config file that
/// defines a custom `deployer` role and binds [`TEST_DEPLOYER_TOKEN`] to it.
/// Returns the gateway handle; the temp config file is removed once loaded.
pub async fn start_gateway_with_auth_config(socket_path: PathBuf) -> GatewayHandle {
    let path = std::env::temp_dir().join(format!(
        "remux-gw-e2e-auth-{}-{}.toml",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(
        &path,
        format!(
            r#"
                [[tokens]]
                token = "{TEST_DEPLOYER_TOKEN}"
                subject = "deployer-bot"
                roles = ["deployer"]

                [[roles]]
                name = "deployer"
                permissions = ["session.create", "session.input", "session.read", "session.list"]
            "#
        ),
    )
    .expect("write auth-config");
    let auth =
        AuthConfig::from_flags_and_config(TEST_TOKEN.to_string(), None, Some(path.as_path()))
            .expect("load auth-config");
    let _ = std::fs::remove_file(&path);
    start_gateway_with_auth(socket_path, auth).await
}

/// The HS256 shared secret the JWT e2e tests mint + validate with.
pub const TEST_JWT_HS256_SECRET: &str = "test-jwt-hs256-secret-0123456789abcdef";

/// Start the gateway with the static admin [`TEST_TOKEN`] AND a JWT/OIDC HS256
/// validator using [`TEST_JWT_HS256_SECRET`]. Static tokens still work; a JWT
/// signed with the secret authenticates via the same RBAC roles.
pub async fn start_gateway_with_jwt(socket_path: PathBuf) -> GatewayHandle {
    use remux_gateway::jwt_service::{JwtAuth, JwtSettings};
    let settings = JwtSettings {
        hs256_secret: Some(TEST_JWT_HS256_SECRET.to_string()),
        ..Default::default()
    };
    let jwt = JwtAuth::from_settings(&settings)
        .await
        .expect("build jwt auth")
        .expect("jwt enabled");
    let auth = AuthConfig::new(TEST_TOKEN.to_string()).with_jwt(Some(jwt));
    start_gateway_with_auth(socket_path, auth).await
}

/// Mint an HS256 JWT signed with [`TEST_JWT_HS256_SECRET`] carrying `sub` and a
/// `roles` array, valid for one hour.
pub fn mint_jwt(sub: &str, roles: &[&str]) -> String {
    mint_jwt_with(sub, roles, 3600)
}

/// Mint an HS256 JWT with an explicit expiry offset (seconds from now; may be
/// negative for an expired token).
pub fn mint_jwt_with(sub: &str, roles: &[&str], exp_offset_secs: i64) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let exp = jsonwebtoken::get_current_timestamp() as i64 + exp_offset_secs;
    let claims = serde_json::json!({
        "sub": sub,
        "roles": roles,
        "exp": exp,
    });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(TEST_JWT_HS256_SECRET.as_bytes()),
    )
    .expect("mint test JWT")
}

/// Start the gateway with the built-in browser client (AW5) **disabled**
/// (`--no-web-ui` equivalent), with the fixed read-write [`TEST_TOKEN`].
pub async fn start_gateway_no_web_ui(socket_path: PathBuf) -> GatewayHandle {
    let auth = AuthConfig::new(TEST_TOKEN.to_string());
    start_gateway_with_auth_and_web(socket_path, auth, false).await
}

/// Start the gateway with an explicit [`AuthConfig`] (web UI enabled).
pub async fn start_gateway_with_auth(socket_path: PathBuf, auth: AuthConfig) -> GatewayHandle {
    start_gateway_with_auth_and_web(socket_path, auth, true).await
}

/// Start the gateway with an explicit [`AuthConfig`] and an explicit web-UI flag.
pub async fn start_gateway_with_auth_and_web(
    socket_path: PathBuf,
    auth: AuthConfig,
    web_ui: bool,
) -> GatewayHandle {
    ensure_crypto_provider();

    let tls = TlsMaterial::generate_self_signed().expect("generate self-signed cert");
    let cert_pem = tls.cert_pem.clone();
    let rustls_config = tls.into_rustls_config().await.expect("build rustls config");

    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind ephemeral loopback port");

    let state = AppState::new(socket_path, auth).with_web_ui(web_ui);

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
