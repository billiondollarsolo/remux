mod app;
mod client;
mod tui;
mod ui;

use remux_core::RemuxError;

#[tokio::main]
async fn main() {
    let socket_path = remux_core::Config::default().daemon.socket_path;

    let client = match client::RemuxClient::connect(&socket_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error connecting to remuxd: {e}");
            eprintln!("Is the daemon running? Start it with: remuxd");
            std::process::exit(1);
        }
    };

    let mut terminal = match tui::init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to initialize terminal: {e}");
            std::process::exit(1);
        }
    };

    let mut app_state = app::App::new(client);
    let result = app_state.run(&mut terminal).await;

    // Always restore terminal state
    if let Err(restore_err) = tui::restore(terminal) {
        eprintln!("Warning: failed to restore terminal: {restore_err}");
    }

    if let Err(e) = result {
        // Check if it's a client error so we can show the remux error message
        let msg = match e.downcast_ref::<RemuxError>() {
            Some(re) => re.to_string(),
            None => e.to_string(),
        };
        eprintln!("Error: {msg}");
        std::process::exit(1);
    }
}
