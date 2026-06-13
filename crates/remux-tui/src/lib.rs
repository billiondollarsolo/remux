//! Remux terminal UI, exposed as a library so the `remux` CLI can fold it into
//! a `remux ui` subcommand. The standalone `remux-tui` binary is a thin wrapper
//! over [`run`].

mod app;
mod client;
mod tui;
mod ui;

use std::path::PathBuf;

use remux_core::RemuxError;

/// Launch the interactive session-manager UI against the daemon listening on
/// `socket_path`. Sets up and always restores the terminal, even on error.
pub async fn run(socket_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let client = match client::RemuxClient::connect(&socket_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error connecting to remuxd: {e}");
            eprintln!("Is the daemon running? Start it with: remuxd");
            return Err(Box::new(e));
        }
    };

    let mut terminal = tui::init()?;

    let mut app_state = app::App::new(client);
    let result = app_state.run(&mut terminal).await;

    // Always restore terminal state.
    if let Err(restore_err) = tui::restore(terminal) {
        eprintln!("Warning: failed to restore terminal: {restore_err}");
    }

    if let Err(e) = result {
        // Surface the remux error message when it is one.
        let msg = match e.downcast_ref::<RemuxError>() {
            Some(re) => re.to_string(),
            None => e.to_string(),
        };
        eprintln!("Error: {msg}");
        return Err(e);
    }

    Ok(())
}
