//! `remux-gateway` — the TLS-terminating, bearer-authed HTTPS/WSS server that
//! exposes the structured `/v1` API by translating it onto the local daemon
//! Unix socket. The daemon stays Unix-socket-only; this is the only process with
//! a network listener.
//!
//! Out of the box: binds `127.0.0.1:8443`, generates a self-signed cert for
//! loopback and a random bearer token (logged jupyter-style) so it works with no
//! configuration. TLS is always on.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use remux_authz::MtlsIdentities;
use remux_core::Config;
use remux_gateway::app::AppState;
use remux_gateway::auth::AuthConfig;
use remux_gateway::jwt_service::{JwtAuth, JwtSettings};
use remux_gateway::mtls::{MtlsAcceptor, MtlsConfig, MtlsMode};
use remux_gateway::peer_tls::PeerVerification;
use remux_gateway::register::RegisterConfig;
use remux_gateway::tls::TlsMaterial;

#[derive(Debug, Parser)]
#[command(
    name = "remux-gateway",
    about = "TLS REST + WebSocket gateway for the remux daemon (agent-native /v1 API)"
)]
struct Cli {
    /// Path to the daemon's Unix socket. Precedence: this flag, then
    /// `REMUX_SOCKET_PATH`, then the `remux_core::Config` default.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Address to bind the HTTPS/WSS listener to.
    #[arg(long, default_value = "127.0.0.1:8443")]
    listen: SocketAddr,

    /// Bearer token granting **read-write** (full) access. If unset, falls back
    /// to `REMUX_GATEWAY_TOKEN`; if that is also unset, a random token is
    /// generated and logged at startup.
    #[arg(long, env = "REMUX_GATEWAY_TOKEN")]
    token: Option<String>,

    /// Optional bearer token granting **read-only** access (observe-only
    /// endpoints + the `/events` WS; rejected with 403 on any write route).
    /// Maps to the built-in `viewer` role. Falls back to
    /// `REMUX_GATEWAY_READ_TOKEN`.
    #[arg(long, env = "REMUX_GATEWAY_READ_TOKEN")]
    read_token: Option<String>,

    /// Optional path to a TOML auth-config file adding principal-shaped tokens
    /// and custom roles (RBAC). Merged over the back-compat `--token` /
    /// `--read-token` flags and the built-in roles. Falls back to
    /// `REMUX_GATEWAY_AUTH_CONFIG`.
    #[arg(long, value_name = "FILE", env = "REMUX_GATEWAY_AUTH_CONFIG")]
    auth_config: Option<PathBuf>,

    // --- Phase B: JWT/OIDC bearer authentication ---
    // A JWT that validates maps its claims to a Principal and flows through the
    // SAME RBAC roles as a static token. At most one key source may be set; if
    // none is set, behavior is exactly as before (static tokens only).
    /// JWT HS256 shared secret (symmetric). Falls back to
    /// `REMUX_GATEWAY_JWT_HS256_SECRET`.
    #[arg(long, value_name = "SECRET", env = "REMUX_GATEWAY_JWT_HS256_SECRET")]
    jwt_hs256_secret: Option<String>,

    /// JWT static public-key PEM file (RS256 or ES256, offline-friendly). Falls
    /// back to `REMUX_GATEWAY_JWT_PUBLIC_KEY`.
    #[arg(long, value_name = "PEM_FILE", env = "REMUX_GATEWAY_JWT_PUBLIC_KEY")]
    jwt_public_key: Option<PathBuf>,

    /// JWKS URL to fetch over HTTPS and cache (RS256/ES256). Falls back to
    /// `REMUX_GATEWAY_JWT_JWKS_URL`.
    #[arg(long, value_name = "URL", env = "REMUX_GATEWAY_JWT_JWKS_URL")]
    jwt_jwks_url: Option<String>,

    /// JWKS refresh TTL in seconds (default 300).
    #[arg(long, default_value = "300")]
    jwt_jwks_ttl: u64,

    /// Accept self-signed/invalid TLS certs when fetching the JWKS URL.
    #[arg(long, default_value = "false")]
    jwt_jwks_tls_insecure: bool,

    /// Required JWT issuer (`iss`). When set, a token with a different issuer is
    /// rejected. Falls back to `REMUX_GATEWAY_JWT_ISSUER`.
    #[arg(long, value_name = "ISS", env = "REMUX_GATEWAY_JWT_ISSUER")]
    jwt_issuer: Option<String>,

    /// Required JWT audience (`aud`). Falls back to `REMUX_GATEWAY_JWT_AUDIENCE`.
    #[arg(long, value_name = "AUD", env = "REMUX_GATEWAY_JWT_AUDIENCE")]
    jwt_audience: Option<String>,

    /// JWT claim to read roles from (default `roles`; accepts a JSON array or a
    /// space-delimited string). Falls back to `REMUX_GATEWAY_JWT_ROLES_CLAIM`.
    #[arg(long, value_name = "CLAIM", env = "REMUX_GATEWAY_JWT_ROLES_CLAIM")]
    jwt_roles_claim: Option<String>,

    /// TLS certificate (PEM). Must be paired with `--tls-key`. If both are
    /// omitted, a self-signed cert is generated for `127.0.0.1`/`localhost`.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// TLS private key (PEM). Must be paired with `--tls-cert`.
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Disable the built-in browser client (AW5). When set, `GET /` (and the
    /// static asset routes) return `404`; the `/v1` API is unaffected. The UI is
    /// served by default.
    #[arg(long)]
    no_web_ui: bool,

    /// AW6: auto-register this gateway with a control plane at `<CP_URL>` on
    /// startup, then heartbeat on a timer and best-effort deregister on
    /// shutdown. When unset, no registration happens.
    #[arg(long, value_name = "CP_URL")]
    register: Option<String>,

    /// The register-token the control plane authenticates this gateway's
    /// registration with. Falls back to `REMUX_GATEWAY_REGISTER_TOKEN`.
    #[arg(long, env = "REMUX_GATEWAY_REGISTER_TOKEN")]
    register_token: Option<String>,

    /// The gateway's externally-reachable base URL the control plane dials back
    /// (e.g. `https://10.0.0.4:8443`). Defaults to `https://<--listen>`.
    #[arg(long, value_name = "URL")]
    advertise_url: Option<String>,

    /// The logical host name to register under. Defaults to the system hostname.
    #[arg(long, value_name = "NAME")]
    register_name: Option<String>,

    /// Selector label `key=value` to register with (repeatable). Used by the
    /// control plane for fan-out / intent routing.
    #[arg(long = "label", value_name = "k=v")]
    labels: Vec<String>,

    /// Registration TTL in seconds; the heartbeat runs every `ttl/2`.
    #[arg(long, default_value = "30")]
    register_ttl: u64,

    /// PEM CA bundle to trust for the control plane's TLS cert when registering
    /// (Phase C). The control plane's own self-signed cert may be used here as a
    /// CA root. Mutually preferred over `--register-pin`.
    #[arg(long, value_name = "PEM")]
    register_ca: Option<PathBuf>,

    /// SHA-256 fingerprint (hex, `:`/whitespace ignored) of the control plane's
    /// LEAF certificate to pin (Phase C, repeatable). Accepts ONLY a matching
    /// leaf — no CA needed, ideal for a self-signed control plane.
    #[arg(long = "register-pin", value_name = "SHA256")]
    register_pins: Vec<String>,

    /// DEV ONLY: accept ANY control-plane TLS cert when registering. Defaults to
    /// `false` (secure): without a CA/pin the control plane is verified against
    /// system roots and a self-signed cert fails with a clear error. Loudly logged.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    register_tls_insecure: bool,

    // --- Phase C: mTLS client-certificate authentication (inbound) ---
    /// Enable mTLS: request + verify client certificates against this PEM CA
    /// bundle. When set, a verified client cert's identity becomes the
    /// authenticated principal (cert identity wins over a bearer).
    #[arg(long, value_name = "PEM")]
    client_ca: Option<PathBuf>,

    /// mTLS enforcement mode: `optional` (use a valid client cert if presented,
    /// else fall back to bearer auth) or `require` (a valid client cert is
    /// mandatory; the handshake refuses connections without one). Default
    /// `optional`. Ignored unless `--client-ca` is set.
    #[arg(long, default_value = "optional")]
    mtls_mode: String,

    /// TOML file mapping client-cert identities (CN or first SAN) to roles
    /// (`[[identities]] subject="…" roles=[…]`). Unmapped valid certs get
    /// `--mtls-default-roles`.
    #[arg(long, value_name = "TOML")]
    mtls_identities: Option<PathBuf>,

    /// Comma-separated default roles for a valid-but-unmapped client cert. Default
    /// none → such a cert authenticates but is `403` on every route until mapped.
    #[arg(long, value_name = "r1,r2", default_value = "")]
    mtls_default_roles: String,
}

/// Parse a `--label key=value` argument into a `(key, value)` pair.
fn parse_label(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("invalid --label {s:?} (expected key=value)")),
    }
}

/// The default `advertise_url` derived from the `--listen` address: an HTTPS URL
/// at that host/port. A wildcard bind (`0.0.0.0`/`::`) can't be dialed back, so
/// this is only a fallback — operators with a wildcard bind should pass
/// `--advertise-url` explicitly.
fn default_advertise_url(listen: SocketAddr) -> String {
    format!("https://{listen}")
}

/// Resolve the host name to register under: the flag, else the system hostname,
/// else a stable fallback.
fn resolve_register_name(flag: Option<String>) -> String {
    if let Some(name) = flag.filter(|n| !n.is_empty()) {
        return name;
    }
    hostname_or_fallback()
}

/// Read the system hostname, falling back to `"remux-gateway"` if unavailable.
fn hostname_or_fallback() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "remux-gateway".to_string())
}

fn resolve_socket_path(flag: Option<PathBuf>) -> PathBuf {
    if let Some(p) = flag {
        return p;
    }
    if let Some(env) = std::env::var_os("REMUX_SOCKET_PATH") {
        return PathBuf::from(env);
    }
    Config::default().daemon.socket_path
}

/// Generate a random, URL-safe bearer token (jupyter-style hex).
fn generate_token() -> String {
    // 256 bits of randomness from two v4 UUIDs (uuid pulls a CSPRNG via getrandom).
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

fn main() {
    // Install the default rustls crypto provider before any TLS use.
    // axum-server's `tls-rustls` feature pulls `rustls/aws-lc-rs`, so that's the
    // available provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("remux_gateway=info,info")),
        )
        .init();

    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(run(cli)) {
        eprintln!("remux-gateway: fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let socket_path = resolve_socket_path(cli.socket);

    // Resolve the read-write bearer token (flag/env, else generate + log it
    // jupyter-style).
    let (token, generated) = match cli.token {
        Some(t) if !t.is_empty() => (t, false),
        _ => (generate_token(), true),
    };
    // Optional read-only token. A value equal to the read-write token is ignored
    // (the admin mapping wins, granting the broader `admin` role).
    let read_token = cli.read_token.filter(|t| !t.is_empty());
    let auth = AuthConfig::from_flags_and_config(
        token.clone(),
        read_token.clone(),
        cli.auth_config.as_deref(),
    )
    .map_err(|e| format!("auth config: {e}"))?;

    // Phase B: optional JWT/OIDC validator. When no JWT key source is set this is
    // `None` and behavior is exactly as before (static tokens only).
    let jwt_settings = JwtSettings {
        hs256_secret: cli.jwt_hs256_secret.clone(),
        public_key_pem: cli.jwt_public_key.clone(),
        jwks_url: cli.jwt_jwks_url.clone(),
        issuer: cli.jwt_issuer.clone(),
        audience: cli.jwt_audience.clone(),
        roles_claim: cli.jwt_roles_claim.clone(),
        jwks_ttl_secs: Some(cli.jwt_jwks_ttl),
        jwks_tls_insecure: cli.jwt_jwks_tls_insecure,
    };
    let jwt = JwtAuth::from_settings(&jwt_settings)
        .await
        .map_err(|e| format!("jwt config: {e}"))?;
    if jwt.is_some() {
        let kind = if jwt_settings
            .hs256_secret
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            "HS256 shared secret"
        } else if jwt_settings.public_key_pem.is_some() {
            "static public-key PEM (RS256/ES256)"
        } else {
            "JWKS URL (RS256/ES256, cached + refreshed)"
        };
        tracing::info!(
            jwt_key = kind,
            issuer = cli.jwt_issuer.as_deref().unwrap_or("(any)"),
            audience = cli.jwt_audience.as_deref().unwrap_or("(any)"),
            roles_claim = cli.jwt_roles_claim.as_deref().unwrap_or("roles"),
            "JWT/OIDC bearer auth enabled (static tokens tried first, then JWT; same RBAC roles)"
        );
    }
    let auth = auth.with_jwt(jwt);

    // Resolve TLS material (operator PEM or self-signed for loopback).
    let tls = TlsMaterial::resolve(cli.tls_cert, cli.tls_key)?;
    if tls.self_signed {
        tracing::info!(
            fingerprint = %tls.fingerprint,
            "no --tls-cert/--tls-key given: generated a self-signed cert for 127.0.0.1/localhost"
        );
    } else {
        tracing::info!(fingerprint = %tls.fingerprint, "loaded operator-provided TLS cert");
    }

    // Phase C: optional mTLS (inbound client-certificate auth). When `--client-ca`
    // is set, build an mTLS acceptor; otherwise serve plain TLS as before.
    let mtls = match cli.client_ca.as_deref() {
        Some(ca_path) => {
            let mode = MtlsMode::parse(&cli.mtls_mode)?;
            let default_roles: Vec<String> = cli
                .mtls_default_roles
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            let identities = match cli.mtls_identities.as_deref() {
                Some(path) => remux_authz::load_mtls_identities(path, default_roles.clone())
                    .map_err(|e| format!("mtls identities: {e}"))?,
                None => MtlsIdentities::new([], default_roles.clone()),
            };
            let cfg = MtlsConfig::from_paths(ca_path, mode, identities)
                .map_err(|e| format!("mtls setup: {e}"))?;
            let rustls = cfg
                .server_config(&tls.cert_pem, &tls.key_pem)
                .map_err(|e| format!("mtls server config: {e}"))?;
            tracing::info!(
                mode = %mode,
                client_ca = %ca_path.display(),
                identities = cfg.identity_count(),
                default_roles = %if default_roles.is_empty() { "(none)".to_string() } else { default_roles.join(",") },
                "mTLS client-certificate auth enabled (verified cert identity wins over bearer)"
            );
            Some(MtlsAcceptor::new(rustls, cfg))
        }
        None => None,
    };
    // The plain-TLS config is only needed when mTLS is off.
    let rustls_config = if mtls.is_none() {
        Some(tls.into_rustls_config().await?)
    } else {
        None
    };

    // Bind the TLS listener.
    let (listener, addr) = remux_gateway::server::bind_listener(cli.listen)
        .map_err(|e| format!("failed to bind {}: {e}", cli.listen))?;

    tracing::info!(
        listen = %addr,
        socket = %socket_path.display(),
        "remux-gateway serving the /v1 API over TLS (daemon stays Unix-socket-only)"
    );

    if generated {
        // Jupyter-style: print the ready-to-use URL with the generated token so
        // it works out of the box.
        println!();
        println!("    remux-gateway is running. Use this bearer token to authenticate:");
        println!();
        println!("        {token}");
        println!();
        println!("    Example:");
        println!("        curl -k https://{addr}/v1/sessions -H 'Authorization: Bearer {token}'");
        println!();
    } else {
        tracing::info!("using bearer token from --token/REMUX_GATEWAY_TOKEN");
    }
    tracing::info!(
        token_id = %remux_gateway::auth::audit_id_for(&token),
        token_count = auth.token_count(),
        "bearer auth active (deny-by-default; RBAC principals: admin token -> `admin` role)"
    );
    if read_token.is_some() {
        tracing::info!("read-only token configured (`viewer` role)");
    }
    if cli.auth_config.is_some() {
        tracing::info!("auth-config file loaded (principal-shaped tokens + custom roles)");
    }

    if cli.no_web_ui {
        tracing::info!("built-in browser client disabled (--no-web-ui); GET / returns 404");
    } else {
        tracing::info!(url = %format!("https://{addr}/"), "built-in browser client served at GET /");
    }

    let state = AppState::new(socket_path, auth).with_web_ui(!cli.no_web_ui);

    // AW6: optional outbound auto-registration with a control plane. A shutdown
    // watch channel lets a SIGTERM/SIGINT signal both the server (graceful stop)
    // and the registration task (best-effort deregister) at once.
    let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
    if let Some(cp_url) = cli.register.clone() {
        let register_token = cli
            .register_token
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_default();
        if register_token.is_empty() {
            tracing::warn!(
                "--register set without a register token \
                 (--register-token/REMUX_GATEWAY_REGISTER_TOKEN); registration will likely be rejected"
            );
        }
        let mut labels = BTreeMap::new();
        for l in &cli.labels {
            match parse_label(l) {
                Ok((k, v)) => {
                    labels.insert(k, v);
                }
                Err(e) => return Err(e),
            }
        }
        let advertise_url = cli
            .advertise_url
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| default_advertise_url(addr));
        let name = resolve_register_name(cli.register_name.clone());

        // Resolve the control-plane TLS-verification posture (secure by default).
        let register_ca_pem = match cli.register_ca.as_deref() {
            Some(path) => Some(
                std::fs::read(path)
                    .map_err(|e| format!("failed to read --register-ca {}: {e}", path.display()))?,
            ),
            None => None,
        };
        let (verification, vlabel) = PeerVerification::resolve(
            cli.register_tls_insecure,
            register_ca_pem,
            cli.register_pins.clone(),
        );
        tracing::info!(
            cp_url = %cp_url,
            name = %name,
            advertise_url = %advertise_url,
            ttl_secs = cli.register_ttl,
            tls_verification = vlabel,
            "auto-registering with the control plane (outbound; daemon stays Unix-socket-only)"
        );
        let reg_cfg = RegisterConfig {
            cp_url,
            register_token,
            advertise_url,
            name,
            labels,
            // The gateway advertises its OWN read-write bearer so the CP can
            // call back into its /v1 API.
            gateway_token: token.clone(),
            ttl_secs: cli.register_ttl,
            verification,
        };
        remux_gateway::register::spawn(reg_cfg, shutdown_tx.subscribe());
    }

    // Translate SIGTERM/SIGINT into the shutdown signal so registration can
    // deregister and the server can drain gracefully.
    let mut server_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let shutdown = async move {
        // Resolve once the shutdown flag flips to true.
        loop {
            if *server_shutdown.borrow() {
                break;
            }
            if server_shutdown.changed().await.is_err() {
                break;
            }
        }
    };

    match (mtls, rustls_config) {
        (Some(acceptor), _) => {
            remux_gateway::server::serve_mtls_with_shutdown(listener, acceptor, state, shutdown)
                .await
        }
        (None, Some(rustls_config)) => {
            remux_gateway::server::serve_with_shutdown(listener, rustls_config, state, shutdown)
                .await
        }
        (None, None) => unreachable!("rustls_config is Some whenever mTLS is off"),
    }
    .map_err(|e| format!("server error: {e}"))
}

/// Await a SIGTERM or SIGINT (Ctrl-C). On non-unix or signal-install failure,
/// fall back to Ctrl-C alone.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
