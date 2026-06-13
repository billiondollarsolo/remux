mod daemon;
mod persistence;
mod pty;
mod scrollback;
mod session_manager;
mod vt;

use std::path::PathBuf;

use clap::Parser;
use tracing::info;

use remux_core::Config;

use daemon::Daemon;

/// Remux session daemon.
#[derive(Parser, Debug)]
#[command(name = "remuxd", version, about = "Remux session daemon")]
struct Args {
    /// Path to the config file.
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,

    /// Path to the Unix domain socket (overrides config).
    #[arg(long, short = 's')]
    socket: Option<PathBuf>,
}

fn main() {
    // Initialize tracing subscriber with env filter
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Load config
    let config = load_config(&args.config);

    // Determine socket path. Precedence: --socket flag, then REMUX_SOCKET_PATH
    // env var, then the configured/default path.
    let socket_path = args
        .socket
        .or_else(|| std::env::var_os("REMUX_SOCKET_PATH").map(PathBuf::from))
        .unwrap_or_else(|| config.daemon.socket_path.clone());

    info!(
        socket = %socket_path.display(),
        scrollback_lines = config.daemon.max_scrollback_lines,
        "remuxd starting"
    );

    // Create and run the daemon
    let daemon = Daemon::new(config);

    let runtime = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    runtime.block_on(async {
        if let Err(e) = daemon.run(socket_path).await {
            tracing::error!(error = %e, "daemon failed");
            std::process::exit(1);
        }
    });
}

/// Load configuration from file or use defaults.
fn load_config(config_path: &Option<PathBuf>) -> Config {
    match config_path {
        Some(path) => {
            info!(path = %path.display(), "loading config from specified path");
            match Config::load(path) {
                Ok(config) => {
                    info!("config loaded successfully");
                    config
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load config, using defaults");
                    Config::default()
                }
            }
        }
        None => {
            // Try default config path
            let default_path = dirs_config_path();
            if default_path.exists() {
                info!(path = %default_path.display(), "loading config from default path");
                match Config::load(&default_path) {
                    Ok(config) => {
                        info!("config loaded successfully");
                        config
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to load default config, using defaults");
                        Config::default()
                    }
                }
            } else {
                info!("no config file found, using defaults");
                Config::default()
            }
        }
    }
}

/// Get the default config file path: ~/.config/remux/config.toml
fn dirs_config_path() -> PathBuf {
    dirs_home()
        .join(".config")
        .join("remux")
        .join("config.toml")
}

/// Get the user's home directory.
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}
