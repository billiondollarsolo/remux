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
use clap_complete::Shell;
use remux_core::Config;

use client::RemuxClient;

#[derive(Parser)]
#[command(name = "remux", version, about = "Terminal session runtime")]
pub struct Cli {
    /// Path to the daemon socket (overrides config)
    #[arg(long, global = true)]
    socket: Option<String>,

    /// Run the command against a remote host over SSH (e.g. `--host devbox`).
    /// Tunnels the protocol via `ssh <host> remux bridge`; no local daemon is
    /// started for remote commands.
    #[arg(long, global = true)]
    host: Option<String>,

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
    #[command(visible_alias = "list")]
    Ls {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show scrollback preview
        #[arg(long)]
        preview: bool,
    },
    /// Attach to a session
    #[command(visible_aliases = ["a", "at"])]
    Attach {
        /// Session name or ID
        name: String,
        /// Attach read-only (Observer): render output but never forward input
        #[arg(long)]
        read_only: bool,
        /// Disable the persistent bottom status line for this attach
        #[arg(long)]
        no_status: bool,
    },
    /// Launch the interactive session-manager UI
    #[command(visible_alias = "i")]
    Ui,
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
    /// Capture and render a session's current screen
    Peek {
        /// Session name or ID
        name: String,
        /// Output the snapshot as pretty-printed JSON
        #[arg(long, conflicts_with = "ansi")]
        json: bool,
        /// Render with SGR color/attributes preserved (safe to pipe)
        #[arg(long)]
        ansi: bool,
    },
    /// Block until a session satisfies a predicate
    #[command(group(
        clap::ArgGroup::new("predicate")
            .required(true)
            .args(["idle", "for_regex", "exit"]),
    ))]
    Wait {
        /// Session name or ID
        name: String,
        /// Succeed when no output arrives for this duration (e.g. 500ms, 2s, 1m)
        #[arg(long)]
        idle: Option<String>,
        /// Succeed when output matches this regex
        #[arg(long = "for-regex")]
        for_regex: Option<String>,
        /// Succeed when the session exits (process exits with the child's code)
        #[arg(long)]
        exit: bool,
        /// Overall timeout (e.g. 30s); on expiry the process exits with code 4
        #[arg(long)]
        timeout: Option<String>,
        /// Emit the outcome as JSON: {"result":...,"exit_code":N}
        #[arg(long)]
        json: bool,
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
    #[command(visible_alias = "k")]
    Kill {
        /// Session name or ID
        name: String,
    },
    /// Generate shell completions (bash, zsh, fish, ...)
    Completions {
        /// Target shell
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Multi-host fleet discovery over SSH (client-side fan-out, no control plane)
    #[command(visible_alias = "f", subcommand)]
    Fleet(FleetCommands),
    /// Internal: pipe the protocol between stdin/stdout and the local daemon
    /// socket. Run on a remote host via `ssh <host> remux bridge` to back the
    /// `--host` remote transport. Not intended for direct use.
    #[command(hide = true)]
    Bridge,
}

#[derive(Subcommand)]
enum FleetCommands {
    /// List the configured fleet hosts (name, ssh target, labels)
    Hosts {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List sessions across the fleet (concurrent fan-out over SSH)
    #[command(visible_alias = "list")]
    Ls {
        /// Output as JSON: [{ host, ssh, ok, error, sessions }]
        #[arg(long)]
        json: bool,
        /// Only query hosts matching this label (key=value). Repeatable;
        /// a host must match ALL given labels.
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    /// Attach to <host>:<session>, resolving <host> via the registry
    #[command(visible_aliases = ["a", "at"])]
    Attach {
        /// Target in the form <host>:<session> (e.g. devbox:backend)
        target: String,
        /// Attach read-only (Observer): render output but never forward input
        #[arg(long)]
        read_only: bool,
        /// Disable the persistent bottom status line for this attach
        #[arg(long)]
        no_status: bool,
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

    // `completions` is a purely local command: it needs no daemon.
    if let Commands::Completions { shell } = cli.command {
        if let Err(e) = cmd::completions::run(shell) {
            eprintln!("error: {e}");
            process::exit(exit::exit_code_for(&e));
        }
        return;
    }

    let config = load_config();
    let socket_path = get_socket_path(cli.socket.as_deref(), &config);

    // `bridge` is the remote-host half of the SSH transport: it always connects
    // to the LOCAL daemon socket and pipes bytes. It honors the normal daemon
    // auto-spawn so a fresh remote host can be bridged into. `--host` is ignored
    // here (a bridge always serves its own local daemon).
    if let Commands::Bridge = cli.command {
        if let Err(e) = daemon_spawn::ensure_daemon_running(&socket_path) {
            eprintln!("error: {e}");
            process::exit(6);
        }
        if let Err(e) = cmd::bridge::run(&socket_path).await {
            eprintln!("error: {e}");
            process::exit(exit::exit_code_for(&e));
        }
        return;
    }

    // `fleet` manages its own per-host transports (fan-out over SSH for `ls`,
    // a remote bridge for `attach`); it never touches the local daemon socket.
    // Handle it before the local/remote client connection below.
    if let Commands::Fleet(fleet_cmd) = cli.command {
        run_fleet(fleet_cmd, &config).await;
        return;
    }

    // Establish the client transport. With `--host`, tunnel over SSH to the
    // remote daemon and SKIP the local daemon auto-spawn (we never start a local
    // daemon for a remote command). Otherwise use the local Unix socket, spawning
    // the daemon if needed.
    let mut client = if let Some(ref host) = cli.host {
        match RemuxClient::connect_remote(host).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(exit::exit_code_for(&e));
            }
        }
    } else {
        // Ensure the daemon is running. A failure here means the daemon is
        // unreachable (exit code 6 per the exit-code taxonomy, §5.3).
        if let Err(e) = daemon_spawn::ensure_daemon_running(&socket_path) {
            eprintln!("error: {e}");
            process::exit(6);
        }
        // Connect to the daemon. A connect failure is also "daemon unreachable" (6).
        match RemuxClient::connect(&socket_path).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                process::exit(exit::exit_code_for(&e));
            }
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
        Commands::Attach {
            name,
            read_only,
            no_status,
        } => {
            let status_line = config.client.status_line && !no_status;
            cmd::attach::run(
                client,
                name,
                &config.client.detach_key,
                read_only,
                status_line,
            )
            .await
        }
        Commands::Ui => match remux_tui::run(socket_path.clone()).await {
            Ok(()) => Ok(()),
            Err(_) => process::exit(1),
        },
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
        Commands::Peek { name, json, ansi } => {
            let format = if json {
                cmd::peek::PeekFormat::Json
            } else if ansi {
                cmd::peek::PeekFormat::Ansi
            } else {
                cmd::peek::PeekFormat::Text
            };
            cmd::peek::run(&mut client, name, format).await
        }
        Commands::Wait {
            name,
            idle,
            for_regex,
            exit,
            timeout,
            json,
        } => {
            // Resolve the predicate (clap's ArgGroup guarantees exactly one).
            let predicate = if let Some(idle_str) = idle {
                match cmd::wait::parse_duration(&idle_str) {
                    Some(d) => cmd::wait::WaitPredicate::Idle(d),
                    None => {
                        eprintln!("error: invalid --idle duration: {idle_str:?}");
                        process::exit(1);
                    }
                }
            } else if let Some(re) = for_regex {
                cmd::wait::WaitPredicate::ForRegex(re)
            } else if exit {
                cmd::wait::WaitPredicate::Exit
            } else {
                unreachable!("clap ArgGroup guarantees a predicate is present")
            };

            let timeout_dur = match timeout {
                Some(t) => match cmd::wait::parse_duration(&t) {
                    Some(d) => Some(d),
                    None => {
                        eprintln!("error: invalid --timeout duration: {t:?}");
                        process::exit(1);
                    }
                },
                None => None,
            };

            match cmd::wait::run(client, name, predicate, timeout_dur, json).await {
                Ok(code) => process::exit(code),
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(exit::exit_code_for(&e));
                }
            }
        }
        Commands::Inspect { name, json } => cmd::inspect::run(&mut client, name, json).await,
        Commands::Logs { name, lines } => cmd::logs::run(&mut client, name, lines).await,
        Commands::Rename { old_name, new_name } => {
            cmd::rename::run(&mut client, old_name, new_name).await
        }
        Commands::Kill { name } => cmd::kill::run(&mut client, name).await,
        // Handled before the daemon connection above.
        Commands::Completions { .. } => unreachable!("completions handled early"),
        Commands::Bridge => unreachable!("bridge handled early"),
        Commands::Fleet(_) => unreachable!("fleet handled early"),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(exit::exit_code_for(&e));
    }
}

/// Run a `remux fleet` subcommand. Fleet commands operate on the configured
/// host registry and the SSH transport directly — they do not connect to the
/// local daemon — so this is dispatched before the normal client setup.
async fn run_fleet(cmd: FleetCommands, config: &Config) {
    let hosts = &config.fleet.hosts;
    match cmd {
        FleetCommands::Hosts { json } => {
            cmd::fleet::run_hosts(hosts, json);
        }
        FleetCommands::Ls { json, labels } => {
            // Parse `--label k=v` selectors up front so a bad one fails clearly.
            let mut selectors = Vec::with_capacity(labels.len());
            for l in &labels {
                match cmd::fleet::parse_label(l) {
                    Ok(pair) => selectors.push(pair),
                    Err(e) => {
                        eprintln!("error: {e}");
                        process::exit(1);
                    }
                }
            }
            cmd::fleet::run_ls(hosts, &selectors, json).await;
        }
        FleetCommands::Attach {
            target,
            read_only,
            no_status,
        } => {
            // Resolve <host>:<session> against the registry, then reuse the
            // existing remote attach path (effectively `--host <ssh> attach`).
            let parsed = match cmd::fleet::parse_fleet_target(&target) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(1);
                }
            };
            let host = match cmd::fleet::resolve_host(hosts, &parsed.host) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(1);
                }
            };
            let client = match RemuxClient::connect_remote(&host.ssh).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(exit::exit_code_for(&e));
                }
            };
            let status_line = config.client.status_line && !no_status;
            if let Err(e) = cmd::attach::run(
                client,
                parsed.session,
                &config.client.detach_key,
                read_only,
                status_line,
            )
            .await
            {
                eprintln!("error: {e}");
                process::exit(exit::exit_code_for(&e));
            }
        }
    }
}
