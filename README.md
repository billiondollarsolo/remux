# remux

> **Pre-alpha.** This project is under heavy development. Everything is subject
> to change — the protocol, CLI interface, config format, and internal APIs are
> all still evolving. Not ready for production use.

A terminal session runtime — persist, detach, and reattach shell sessions.

Remux is a tmux alternative built in Rust. A background daemon (`remuxd`) owns PTY
processes and exposes session management over a Unix domain socket. The CLI
(`remux`) communicates with the daemon to create, list, attach to, and manage
sessions. A terminal UI (`remux-tui`) provides an interactive session browser.

## Architecture

```
 ┌──────────┐       Unix socket        ┌──────────┐       fork/exec       ┌─────┐
 │  remux   │◄────────────────────────►│  remuxd  │──────────────────────►│ PTY │
 │  (CLI)   │   newline-JSON / bincode │ (daemon) │   portable-pty        │ bash│
 ├──────────┤                          │          │                       │ vim │
 │ remux-tui│                          │ alacritty│                       │ ... │
 └──────────┘                          │  VT      │                       └─────┘
                                      └──────────┘
```

- **remux-core** — shared types, protocol definitions, config, framing
- **remux-daemon** — session lifecycle, PTY management, VT state, IPC server
- **remux-cli** — command-line interface with auto-daemon-spawn
- **remux-tui** — ratatui-based session browser
- **remux-testkit** — test harness and client for integration tests

## Build

Requires Rust 1.75+ and a C compiler (for `libc`/`nix` bindings).

```sh
cargo build
cargo build --release
```

Produces three binaries:

| Binary      | Crate         | Purpose                  |
|-------------|---------------|--------------------------|
| `remux`     | remux-cli     | CLI client               |
| `remuxd`    | remux-daemon  | Background session daemon|
| `remux-tui` | remux-tui     | Terminal UI              |

## Install

```sh
./packaging/install.sh
```

Builds in release mode and copies `remux` and `remuxd` to `~/.cargo/bin/`.
On Linux, installs a systemd user service. On macOS, installs a launchd plist.

## Usage

### Start a session

```sh
remux new                    # $SHELL in current directory
remux new --name api vim     # named session running vim
remux new htop               # implicit name from command
```

### List sessions

```sh
remux ls
remux ls --json              # machine-readable output
```

### Attach

```sh
remux attach api             # by name
remux attach <uuid>          # by ID
# Ctrl-Q to detach
```

On attach, the daemon sends scrollback history and a VT snapshot, then streams
live output. Only the controlling client can send keyboard input; other clients
attach as observers.

### Inspect, rename, kill

```sh
remux inspect api
remux rename api api-server
remux kill api
```

### View scrollback

```sh
remux logs api
remux logs api --lines 100
```

### TUI

```sh
remux-tui
```

Interactive session list with keyboard navigation (arrows, Enter to attach,
`k` to kill, `r` to refresh, Ctrl-Q to quit).

### Daemon

The CLI auto-spawns `remuxd` on first use via double-fork. To run it manually:

```sh
remuxd                       # uses default config + socket paths
remuxd -c /path/to/config.toml
remuxd -s /tmp/custom.sock
```

## Configuration

Config is loaded from `~/.config/remux/config.toml` (or the path given to
`remuxd -c`). All values are optional — defaults are used for anything missing.

```toml
[daemon]
socket_path = "/run/user/1000/remux/remuxd.sock"   # default: dirs::state_dir()/remux/remuxd.sock
max_scrollback_lines = 20000
persist_scrollback = false
cleanup_exited_after_hours = 168                    # 7 days

[client]
default_shell = "/bin/bash"                         # default: $SHELL or /bin/sh
detach_key = "ctrl-q"

[data]
dir = "/home/user/.local/share/remux"               # default: dirs::data_dir()/remux
```

### Paths

| Path | Default | Purpose |
|------|---------|---------|
| Config | `~/.config/remux/config.toml` | Daemon and client settings |
| Socket | `~/.local/state/remux/remuxd.sock` | IPC Unix domain socket |
| Sessions | `~/.local/share/remux/sessions/` | Persisted session metadata |

## IPC Protocol

All communication between clients and the daemon uses a binary-framed protocol
over a Unix domain socket:

- **Debug builds**: newline-delimited JSON (easy to inspect with `socat`)
- **Release builds**: 4-byte little-endian length prefix + bincode payload

Messages are one of three categories:

**Requests** (client → daemon):
`Ping`, `ListSessions`, `CreateSession`, `InspectSession`, `AttachSession`,
`DetachSession`, `ResizeSession`, `SendInput`, `ReadScrollback`, `RenameSession`,
`KillSession`

**Responses** (daemon → client):
`Pong`, `Ok`, `Error`, `SessionList`, `SessionDetails`, `Created`, `Attached`,
`Scrollback`

**Events** (daemon → client, streamed after attach):
`Output`, `StateSnapshot`, `SessionUpdated`, `SessionExited`, `ControlLost`,
`Error`

`SendInput` is fire-and-forget — the daemon does not send a response, preventing
stale messages from accumulating during an attach event loop.

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `portable-pty` | Cross-platform PTY spawning and I/O |
| `alacritty_terminal` | VT emulator (terminal state, rendering, scrollback) |
| `ratatui` | TUI framework |
| `crossterm` | Terminal raw mode, input events, styling |
| `tokio` | Async runtime (daemon server, client I/O) |
| `serde` + `serde_json` + `bincode` | Protocol serialization |
| `clap` | CLI argument parsing |
| `nix` | UNIX signals, process groups |
| `tracing` | Structured logging |

## Development

```sh
cargo build                  # debug build (JSON protocol)
cargo test                   # run all tests
cargo clippy                 # lint
cargo test -p remux-daemon   # daemon-specific tests
```

The testkit crate (`remux-testkit`) provides a `DaemonHarness` that starts a
temporary `remuxd` instance and a `TestClient` with convenience methods for all
IPC operations, useful for writing integration tests.

## License

All rights reserved.
