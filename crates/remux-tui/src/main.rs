//! Thin wrapper binary over the `remux_tui` library. The real implementation
//! lives in `lib.rs` so the `remux` CLI can reuse it as `remux ui`.

#[tokio::main]
async fn main() {
    let socket_path = remux_core::Config::default().daemon.socket_path;

    if remux_tui::run(socket_path).await.is_err() {
        std::process::exit(1);
    }
}
