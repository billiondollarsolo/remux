//! `remux-control-plane` — the AW6 federation control plane.
//!
//! A TLS-terminating axum service that federates over a fleet of `remux-gateway`
//! instances: gateways register themselves outbound, and the control plane
//! exposes a federated fleet API (`GET /cp/v1/sessions`, `GET /cp/v1/hosts`) plus
//! intent-based routing (`POST /cp/v1/resolve`). The daemon stays
//! Unix-socket-only; the control plane never dials a host it wasn't told about.
//!
//! Out of the box: binds `127.0.0.1:9443`, generates a self-signed cert for
//! loopback, and generates+logs an admin and a register token. TLS is always on.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use remux_control_plane::app::AppState;
use remux_control_plane::auth::AuthConfig;
use remux_control_plane::tls::TlsMaterial;
use remux_gateway::jwt_service::{JwtAuth, JwtSettings};

#[derive(Debug, Parser)]
#[command(
    name = "remux-control-plane",
    about = "AW6 federation control plane for remux: host registry + federated fleet API over TLS"
)]
struct Cli {
    /// Address to bind the HTTPS listener to.
    #[arg(long, default_value = "127.0.0.1:9443")]
    listen: SocketAddr,

    /// Admin bearer token (guards the fleet API: hosts/sessions/resolve). Falls
    /// back to `REMUX_CP_TOKEN`; if unset, a random token is generated + logged.
    #[arg(long, env = "REMUX_CP_TOKEN")]
    token: Option<String>,

    /// Register bearer token (guards register/heartbeat/deregister). Falls back
    /// to `REMUX_CP_REGISTER_TOKEN`; if unset, a random token is generated + logged.
    #[arg(long, env = "REMUX_CP_REGISTER_TOKEN")]
    register_token: Option<String>,

    /// Optional path to a TOML auth-config file adding principal-shaped tokens
    /// and custom roles (RBAC). Merged over the back-compat `--token` /
    /// `--register-token` flags and the built-in roles. Falls back to
    /// `REMUX_CP_AUTH_CONFIG`.
    #[arg(long, value_name = "FILE", env = "REMUX_CP_AUTH_CONFIG")]
    auth_config: Option<PathBuf>,

    // --- Phase B: JWT/OIDC bearer authentication ---
    // A JWT that validates maps its claims to a Principal and flows through the
    // SAME RBAC roles as a static token. At most one key source may be set.
    /// JWT HS256 shared secret. Falls back to `REMUX_CP_JWT_HS256_SECRET`.
    #[arg(long, value_name = "SECRET", env = "REMUX_CP_JWT_HS256_SECRET")]
    jwt_hs256_secret: Option<String>,

    /// JWT static public-key PEM file (RS256/ES256). Falls back to
    /// `REMUX_CP_JWT_PUBLIC_KEY`.
    #[arg(long, value_name = "PEM_FILE", env = "REMUX_CP_JWT_PUBLIC_KEY")]
    jwt_public_key: Option<PathBuf>,

    /// JWKS URL to fetch over HTTPS and cache (RS256/ES256). Falls back to
    /// `REMUX_CP_JWT_JWKS_URL`.
    #[arg(long, value_name = "URL", env = "REMUX_CP_JWT_JWKS_URL")]
    jwt_jwks_url: Option<String>,

    /// JWKS refresh TTL in seconds (default 300).
    #[arg(long, default_value = "300")]
    jwt_jwks_ttl: u64,

    /// Accept self-signed/invalid TLS certs when fetching the JWKS URL.
    #[arg(long, default_value = "false")]
    jwt_jwks_tls_insecure: bool,

    /// Required JWT issuer (`iss`). Falls back to `REMUX_CP_JWT_ISSUER`.
    #[arg(long, value_name = "ISS", env = "REMUX_CP_JWT_ISSUER")]
    jwt_issuer: Option<String>,

    /// Required JWT audience (`aud`). Falls back to `REMUX_CP_JWT_AUDIENCE`.
    #[arg(long, value_name = "AUD", env = "REMUX_CP_JWT_AUDIENCE")]
    jwt_audience: Option<String>,

    /// JWT claim to read roles from (default `roles`). Falls back to
    /// `REMUX_CP_JWT_ROLES_CLAIM`.
    #[arg(long, value_name = "CLAIM", env = "REMUX_CP_JWT_ROLES_CLAIM")]
    jwt_roles_claim: Option<String>,

    /// TLS certificate (PEM). Must be paired with `--tls-key`. If both are
    /// omitted, a self-signed cert is generated for `127.0.0.1`/`localhost`.
    #[arg(long)]
    tls_cert: Option<PathBuf>,

    /// TLS private key (PEM). Must be paired with `--tls-cert`.
    #[arg(long)]
    tls_key: Option<PathBuf>,

    /// Accept self-signed / invalid certificates when calling registered
    /// gateways. Defaults to **true** for v1 (gateways are self-signed by
    /// default); set `--gateway-tls-insecure=false` once gateways present a
    /// trusted/pinned cert.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    gateway_tls_insecure: bool,
}

/// Generate a random, URL-safe bearer token (jupyter-style hex).
fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("remux_control_plane=info,info")
            }),
        )
        .init();

    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(run(cli)) {
        eprintln!("remux-control-plane: fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    // Resolve tokens (flag/env, else generate + log).
    let (admin_token, admin_generated) = match cli.token {
        Some(t) if !t.is_empty() => (t, false),
        _ => (generate_token(), true),
    };
    let (register_token, register_generated) = match cli.register_token {
        Some(t) if !t.is_empty() => (t, false),
        _ => (generate_token(), true),
    };
    let auth = AuthConfig::from_flags_and_config(
        admin_token.clone(),
        register_token.clone(),
        cli.auth_config.as_deref(),
    )
    .map_err(|e| format!("auth config: {e}"))?;

    // Phase B: optional JWT/OIDC validator (None → static tokens only, as before).
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
        tracing::info!(
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
    let rustls_config = tls.into_rustls_config().await?;

    let (listener, addr) = remux_control_plane::server::bind_listener(cli.listen)
        .map_err(|e| format!("failed to bind {}: {e}", cli.listen))?;

    tracing::info!(
        listen = %addr,
        gateway_tls_insecure = cli.gateway_tls_insecure,
        "remux-control-plane serving the /cp/v1 fleet API over TLS"
    );

    if cli.gateway_tls_insecure {
        tracing::warn!(
            "--gateway-tls-insecure is TRUE (v1 default): outbound calls to gateways accept \
             self-signed/invalid certs. Gateway-cert pinning / CA trust is a hardening follow-up."
        );
    }

    // Print generated tokens jupyter-style so the service is usable out of the box.
    if admin_generated || register_generated {
        println!();
        println!("    remux-control-plane is running. Tokens to authenticate:");
        println!();
        if admin_generated {
            println!("        admin (fleet API):   {admin_token}");
        }
        if register_generated {
            println!("        register (gateways): {register_token}");
        }
        println!();
        println!("    Example:");
        println!(
            "        curl -k https://{addr}/cp/v1/hosts -H 'Authorization: Bearer {admin_token}'"
        );
        println!();
    }
    tracing::info!(
        admin_token_id = %remux_control_plane::auth::audit_id_for(&admin_token),
        register_token_id = %remux_control_plane::auth::audit_id_for(&register_token),
        token_count = auth.token_count(),
        "bearer auth active (deny-by-default; RBAC principals: admin -> `fleet-admin`, register -> `registrar`)"
    );
    if cli.auth_config.is_some() {
        tracing::info!("auth-config file loaded (principal-shaped tokens + custom roles)");
    }

    let state = AppState::new(auth).with_gateway_tls_insecure(cli.gateway_tls_insecure);

    remux_control_plane::server::serve(listener, rustls_config, state)
        .await
        .map_err(|e| format!("server error: {e}"))
}
