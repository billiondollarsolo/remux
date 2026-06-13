//! Serving the gateway over TLS (rustls via `axum-server`).
//!
//! Two entry points:
//! - [`serve`] takes a pre-bound `std::net::TcpListener` (so the caller — notably
//!   tests binding `127.0.0.1:0` — can learn the chosen port) plus a
//!   [`RustlsConfig`] and the app [`AppState`], and serves until the process
//!   ends. It returns once the listener is bound (it is already bound by the
//!   caller) and drives the server to completion.
//! - [`bind_listener`] is a small helper to bind a loopback (or any) address and
//!   return the listener + its resolved `SocketAddr`.

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

/// Serve the gateway over TLS on an already-bound `TcpListener`.
///
/// This drives `axum-server` to completion. The minimal public helper the plan
/// asks for: tests bind `127.0.0.1:0`, read `local_addr()`, then call this in a
/// spawned task.
pub async fn serve(
    listener: TcpListener,
    tls: RustlsConfig,
    state: AppState,
) -> std::io::Result<()> {
    let app = router(state);
    // `into_make_service_with_connect_info` makes the peer `SocketAddr` available
    // to handlers/middleware via `ConnectInfo<SocketAddr>` — used by the audit
    // middleware to log the client remote address.
    axum_server::from_tcp_rustls(listener, tls)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}
