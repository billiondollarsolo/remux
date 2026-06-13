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

/// Serve the gateway over TLS, shutting down gracefully when `shutdown` resolves.
///
/// Used by the binary so a SIGTERM/SIGINT can trigger best-effort deregistration
/// from the control plane before the process exits. The future drives
/// `axum-server` via a [`axum_server::Handle`]; when `shutdown` resolves we ask
/// the handle to stop accepting new connections.
pub async fn serve_with_shutdown<F>(
    listener: TcpListener,
    tls: RustlsConfig,
    state: AppState,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let app = router(state);
    let handle = axum_server::Handle::new();
    let handle_for_shutdown = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        // Stop gracefully with a bounded drain window so in-flight requests can
        // finish but a hung connection can't block shutdown forever.
        handle_for_shutdown.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
    });
    axum_server::from_tcp_rustls(listener, tls)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
}
