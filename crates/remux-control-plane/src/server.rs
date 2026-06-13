//! Serving the control plane over TLS (rustls via `axum-server`).
//!
//! Mirrors the gateway's serve helpers so tests can bind `127.0.0.1:0`, learn the
//! port, and drive the server in a spawned task.

use std::net::{SocketAddr, TcpListener};

use axum_server::tls_rustls::RustlsConfig;

use crate::app::{router, AppState};

/// Bind a `std::net::TcpListener` to `addr` (use port `0` for an ephemeral port)
/// and return it with its resolved address.
pub fn bind_listener(addr: SocketAddr) -> std::io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Serve the control plane over TLS on an already-bound `TcpListener`, driving
/// `axum-server` to completion. `ConnectInfo<SocketAddr>` is wired so the audit
/// middleware can log the peer address.
pub async fn serve(
    listener: TcpListener,
    tls: RustlsConfig,
    state: AppState,
) -> std::io::Result<()> {
    let app = router(state);
    axum_server::from_tcp_rustls(listener, tls)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}
