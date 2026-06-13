# remux

> **Pre-alpha.** This project is under heavy development. Everything is subject
> to change — the protocol, CLI interface, config format, and internal APIs are
> all still evolving. Not ready for production use.

A terminal session runtime — detach and reattach shell sessions, with
crash-resilient scrollback that survives a daemon restart.

Remux is a tmux alternative built in Rust. A background daemon (`remuxd`) owns PTY
processes and exposes session management over a Unix domain socket. The CLI
(`remux`) communicates with the daemon to create, list, attach to, and manage
sessions. A terminal UI (`remux ui`) provides an interactive session browser.

## Why remux over tmux?

remux keeps tmux's best trait — sessions that outlive your terminal — and
rethinks the rest around a **protocol-first runtime** instead of a
screen-scraping control mode. Concretely:

| Capability | tmux | remux |
|------------|------|-------|
| Sessions persist across disconnect | ✅ | ✅ |
| Survives the **server/daemon restart** | ❌ server death loses everything | ✅ scrollback + metadata persisted; sessions recovered as `Exited` for inspection |
| Structured control API | control mode / text scraping | typed IPC protocol with a versioned handshake |
| Machine-readable output | limited | `--json` on `ls` / `inspect` / `peek` / `wait` / `new` |
| Headless input injection | `send-keys` (quoting & escape hazards) | `remux send` — binary-safe `--text` / `--bytes-hex` / `--key` / `--stdin`, without stealing control |
| Read the current screen programmatically | `capture-pane` (text only) | `remux peek` — plain text, **ANSI color**, or JSON `TerminalSnapshot` |
| Wait on session state | ❌ (poll it yourself) | `remux wait --idle / --for-regex / --exit` with a real `--timeout` |
| Exit codes scripts can branch on | mostly `0/1` | `0` ok · `3` not found · `4` timeout · `5` denied · `6` daemon down |
| Faithful reattach of full-screen apps | ✅ | ✅ repaints from parsed VT state (color, cursor, alt-screen) |
| Detached terminal-query answering | n/a | ✅ daemon answers DA / cursor-position reports so a backgrounded TUI never hangs |
| Input transparency | layered key handling | raw byte passthrough (modifiers, paste, mouse, UTF-8) |
| Multiple clients on one session | shared | controller + **read-only observers** (`remux attach --read-only`) |
| Implementation | C | Rust (memory-safe); single daemon + thin client |

The payoff is a session runtime that's **scriptable and agent-friendly by
design**: an AI agent or CI job can `new` a session, `send` keystrokes, `wait`
for it to go idle or match a pattern, `peek` the screen as JSON, and branch on
exit codes — no TTY, no control-mode parsing.

**Scope note:** remux deliberately does *not* do in-terminal splits/panes — you
run multiple first-class sessions and switch between them (`remux ui`) rather
than tiling one window. If you need split layouts inside a single pane, tmux
still wins there.

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
remux attach api             # by name (aliases: `remux a`, `remux at`)
remux attach <uuid>          # by ID
remux attach api --read-only # observe without sending input
```

On attach, the daemon replays scrollback history and **repaints the screen from a
parsed VT snapshot**, so full-screen apps (`vim`, `htop`, `less`) come back
intact — colors, cursor, and alternate-screen state included. It then streams
live output. Only the controlling client can send keyboard input; other clients
attach as observers.

Detach and in-attach commands use a **GNU-screen-style `Ctrl-a` prefix**
(configurable via `detach_key`):

| Keys | Action |
|------|--------|
| `Ctrl-a d` / `Ctrl-a Ctrl-d` | Detach |
| `Ctrl-a a` | Send a literal `Ctrl-a` to the session |
| `Ctrl-a l` / `Ctrl-a Ctrl-l` | Redraw the screen |

All other input is forwarded to the PTY byte-for-byte (raw passthrough), so
modifiers, paste, mouse reporting, and UTF-8 work transparently.

### Automation (headless / scripting / AI agents)

Every interactive action has a non-interactive equivalent with machine-readable
output and meaningful exit codes — drive sessions without a TTY:

```sh
remux send api --text "ls -la\n"     # binary-safe input (only \n \t \r \\ interpreted)
remux send api --bytes-hex 1b5b41    # raw bytes (ESC [ A)
remux send api --key Enter           # named keys
echo data | remux send api --stdin   # pipe stdin

remux peek api                       # render current screen as plain text
remux peek api --ansi                # with colors (pipe-safe; no cursor moves)
remux peek api --json                # structured TerminalSnapshot

remux wait api --idle 500ms          # block until output goes quiet
remux wait api --for-regex 'PASS|FAIL' --timeout 30s
remux wait api --exit                # block until the session exits (returns its code)
```

`send` injects input without attaching and without stealing control from an
attached client. Exit codes: `0` success, `1` generic, `3` session not found,
`4` timeout (`wait`), `5` permission denied, `6` daemon unreachable.

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

### Persistence

Remux persists **session metadata and scrollback history**, not live processes.
When `remuxd` exits, the PTYs it owns die with it (they are its children), so a
daemon restart cannot resurrect a running program. What it *can* do:

- **Metadata** for every session is written to
  `~/.local/share/remux/sessions/<id>.json` on create/rename.
- **Scrollback** is written to `<id>.scrollback` when
  `persist_scrollback = true` — flushed on session exit and periodically
  (every ~10s) for crash-resilience.

On startup the daemon recovers prior sessions and presents them as **`Exited`**
(the process is gone). You can still `ls`, `inspect`, and read their history with
`remux logs`; attaching to a recovered session is rejected because there is no
live process. Recovered sessions can be cleared with `remux kill`, and old
persisted sessions are pruned automatically per `cleanup_exited_after_hours`
(set to `0` to disable cleanup).

There is no live-process recovery across restarts.

### TUI

```sh
remux ui                     # interactive session manager (alias: `remux i`)
remux-tui                    # the same UI as a standalone binary
```

Interactive session list with keyboard navigation (arrows, Enter to attach,
`k` to kill, `r` to refresh, Ctrl-Q to quit).

### Shell completions

```sh
remux completions bash > /etc/bash_completion.d/remux
remux completions zsh  > "${fpath[1]}/_remux"
remux completions fish > ~/.config/fish/completions/remux.fish
```

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
persist_scrollback = false                          # write scrollback to disk so it survives a daemon restart
cleanup_exited_after_hours = 168                    # prune persisted sessions older than this; 7 days (0 = never)

[client]
default_shell = "/bin/bash"                         # default: $SHELL or /bin/sh
detach_key = "ctrl-a"                               # prefix key; Ctrl-a d detaches

[data]
dir = "/home/user/.local/share/remux"               # default: dirs::data_dir()/remux
```

### Paths

| Path | Default | Purpose |
|------|---------|---------|
| Config | `~/.config/remux/config.toml` | Daemon and client settings |
| Socket | `~/.local/state/remux/remuxd.sock` | IPC Unix domain socket |
| Sessions | `~/.local/share/remux/sessions/` | Persisted session metadata + scrollback |

## IPC Protocol

All communication between clients and the daemon uses a binary-framed protocol
over a Unix domain socket:

- **Debug builds**: newline-delimited JSON (easy to inspect with `socat`)
- **Release builds**: 4-byte little-endian length prefix + bincode payload

Messages are one of three categories:

**Requests** (client → daemon):
`Hello`, `Ping`, `ListSessions`, `CreateSession`, `InspectSession`,
`AttachSession`, `DetachSession`, `ResizeSession`, `SendInput`, `CaptureScreen`,
`ReadScrollback`, `RenameSession`, `KillSession`

**Responses** (daemon → client):
`Hello`, `Pong`, `Ok`, `Error`, `SessionList`, `SessionDetails`, `Created`,
`Attached`, `Screen`, `Scrollback`

**Events** (daemon → client, streamed after attach):
`Output`, `StateSnapshot`, `SessionUpdated`, `SessionExited`,
`SessionTerminating`, `ControlLost`, `Error`

On connect the client sends `Hello { version }`; the daemon rejects a mismatched
`PROTOCOL_VERSION` rather than risk silent wire corruption. `SendInput` is
fire-and-forget — the daemon does not send a response, preventing stale messages
from accumulating during an attach event loop. When a session is detached, the
daemon answers terminal queries (Device Attributes, cursor-position reports) on
its behalf so a backgrounded TUI doesn't hang.

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
