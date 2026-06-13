//! `remux-gateway` — the TLS-terminating, bearer-authed HTTPS/WSS server that
//! exposes the structured `/v1` API by translating it onto the local daemon
//! Unix socket. The daemon stays Unix-socket-only; this is the only process with
//! a network listener.
//!
//! Out of the box: binds `127.0.0.1:8443`, generates a self-signed cert for
//! loopback and a random bearer token (logged jupyter-style) so it works with no
//! configuration. TLS is always on.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use remux_core::Config;
use remux_gateway::app::AppState;
use remux_gateway::auth::AuthConfig;
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
    /// Falls back to `REMUX_GATEWAY_READ_TOKEN`.
    #[arg(long, env = "REMUX_GATEWAY_READ_TOKEN")]
    read_token: Option<String>,

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
    // (the read-write token wins, granting the broader scope).
    let read_token = cli.read_token.filter(|t| !t.is_empty());
    let auth = AuthConfig::with_scopes(token.clone(), read_token.clone());

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
        token_id = %auth.token_audit_id(),
        read_only_token = auth.has_read_only(),
        "bearer auth active (deny-by-default; read-write + optional read-only scopes)"
    );
    if read_token.is_some() {
        tracing::info!("read-only token configured (observe-only scope)");
    }

    if cli.no_web_ui {
        tracing::info!("built-in browser client disabled (--no-web-ui); GET / returns 404");
    } else {
        tracing::info!(url = %format!("https://{addr}/"), "built-in browser client served at GET /");
    }

    let state = AppState::new(socket_path, auth).with_web_ui(!cli.no_web_ui);

    remux_gateway::server::serve(listener, rustls_config, state)
        .await
        .map_err(|e| format!("server error: {e}"))
}
