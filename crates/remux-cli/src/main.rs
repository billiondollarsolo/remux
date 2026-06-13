mod client;
mod cmd;
mod daemon_spawn;
mod exit;
mod raw_mode;
mod render;
mod render_snapshot;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use remux_core::Config;

use client::RemuxClient;

#[derive(Parser)]
#[command(name = "remux", version, about = "Terminal session runtime")]
struct Cli {
    /// Path to the daemon socket (overrides config)
    #[arg(long, global = true)]
    socket: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new session
    New {
        /// Session name (defaults to cwd basename)
        #[arg(long)]
        name: Option<String>,
        /// Output the created session details as JSON
        #[arg(long)]
        json: bool,
        /// Command to run (defaults to $SHELL)
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// List sessions
    Ls {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show scrollback preview
        #[arg(long)]
        preview: bool,
    },
    /// Attach to a session
    Attach {
        /// Session name or ID
        name: String,
    },
    /// Send input to a session without attaching (fire-and-forget)
    #[command(group(
        clap::ArgGroup::new("input")
            .required(true)
            .args(["text", "bytes_hex", "key", "stdin"]),
    ))]
    Send {
        /// Session name or ID
        name: String,
        /// Send the string's bytes. Only these escapes are interpreted:
        /// \n, \t, \r, \\. No shell or other interpretation (binary-safe).
        #[arg(long)]
        text: Option<String>,
        /// Decode a hex string (e.g. "1b5b41" for ESC [ A) into raw bytes.
        #[arg(long = "bytes-hex")]
        bytes_hex: Option<String>,
        /// Send a named key: Enter, Tab, Esc, Up, Down, Right, Left,
        /// Backspace, Space.
        #[arg(long)]
        key: Option<String>,
        /// Read all of stdin and send it as raw bytes.
        #[arg(long)]
        stdin: bool,
    },
    /// Show session details
    Inspect {
        /// Session name or ID
        name: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show session scrollback logs
    Logs {
        /// Session name or ID
        name: String,
        /// Number of lines to show
        #[arg(long, default_value = "50")]
        lines: usize,
    },
    /// Rename a session
    Rename {
        /// Current name
        old_name: String,
        /// New name
        new_name: String,
    },
    /// Kill a session
    Kill {
        /// Session name or ID
        name: String,
    },
}

/// Load configuration from the first existing config file location, falling
/// back to defaults.
fn load_config() -> Config {
    let config_dirs = [dirs_config_path(), PathBuf::from("/tmp/remux/config.toml")];
    for config_path in &config_dirs {
        if config_path.exists() {
            if let Ok(config) = Config::load(config_path) {
                return config;
            }
        }
    }
    Config::default()
}

fn get_socket_path(cli_socket: Option<&str>, config: &Config) -> PathBuf {
    if let Some(path) = cli_socket {
        return PathBuf::from(path);
    }
    config.daemon.socket_path.clone()
}

fn dirs_config_path() -> PathBuf {
    // Use XDG_CONFIG_HOME or default ~/.config
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
        });
    base.join("remux").join("config.toml")
}

#[tokio::main]
async fn main() {
    // Initialize tracing (disabled by default; enable with RUST_LOG=remux_cli=debug).
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let config = load_config();
    let socket_path = get_socket_path(cli.socket.as_deref(), &config);

    // Ensure the daemon is running. A failure here means the daemon is
    // unreachable (exit code 6 per the exit-code taxonomy, §5.3).
    if let Err(e) = daemon_spawn::ensure_daemon_running(&socket_path) {
        eprintln!("error: {e}");
        process::exit(6);
    }

    // Connect to the daemon. A connect failure is also "daemon unreachable" (6).
    let mut client = match RemuxClient::connect(&socket_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(exit::exit_code_for(&e));
        }
    };

    // Dispatch to command handler.
    let result = match cli.command {
        Commands::New {
            name,
            json,
            command,
        } => cmd::new::run(&mut client, name, command, json).await,
        Commands::Ls { json, preview } => cmd::ls::run(&mut client, json, preview).await,
        Commands::Attach { name } => {
            cmd::attach::run(client, name, &config.client.detach_key).await
        }
        Commands::Send {
            name,
            text,
            bytes_hex,
            key,
            stdin,
        } => {
            let source = if let Some(t) = text {
                cmd::send::InputSource::Text(t)
            } else if let Some(h) = bytes_hex {
                cmd::send::InputSource::BytesHex(h)
            } else if let Some(k) = key {
                cmd::send::InputSource::Key(k)
            } else if stdin {
                cmd::send::InputSource::Stdin
            } else {
                // clap's ArgGroup(required=true) guarantees one source is set.
                unreachable!("clap ArgGroup guarantees an input source is present")
            };
            cmd::send::run(&mut client, name, source).await
        }
        Commands::Inspect { name, json } => cmd::inspect::run(&mut client, name, json).await,
        Commands::Logs { name, lines } => cmd::logs::run(&mut client, name, lines).await,
        Commands::Rename { old_name, new_name } => {
            cmd::rename::run(&mut client, old_name, new_name).await
        }
        Commands::Kill { name } => cmd::kill::run(&mut client, name).await,
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(exit::exit_code_for(&e));
    }
}
