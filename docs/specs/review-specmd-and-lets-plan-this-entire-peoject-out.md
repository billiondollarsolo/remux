# Remux — Implementation Specification

## Overview

Remux is a Rust-based terminal session runtime that provides durable PTY-backed sessions with clean attach/detach semantics. It replaces tmux by focusing on persistent execution contexts rather than terminal multiplexing.

The project is two phases:
1. **Phase 1**: Local daemon + CLI + TUI session manager
2. **Phase 2**: SSH transport to remote hosts

Remux is both a standalone CLI tool and an embeddable Rust library (`remux-core`). Other tools can integrate it the same way they integrate xterm.js or wterm — as a session management backend.

---

## Key Design Decisions

| Decision | Choice |
|----------|--------|
| Virtual terminal library | `alacritty_terminal` |
| IPC encoding | JSON (dev) / bincode (prod) |
| Daemon start | CLI auto-spawns remuxd |
| Session lookup | Name-based (UUID internal) |
| Attach conflict resolution | Steal control, old client becomes observer |
| Scrollback storage | In-memory ring buffer (default 20000 lines) |
| SSH library | `russh` |
| Configuration format | TOML |
| Detach key | Ctrl-Q |
| Default shell | `$SHELL` env var, fallback `/bin/sh` |
| Session limit | Unlimited |
| Duplicate session name | Error (spec.md says "append suffix" — overridden per user decision) |
| Crash recovery | Not supported — daemon crash = session loss |
| Host configuration | SSH config + `hosts.toml` overrides |
| Platforms | Linux + macOS |
| Testing strategy | Integration-first |

---

## Architecture

### Components

```
remux/
├── crates/
│   ├── remux-core/        # Shared types, protocol, config, session models (library)
│   ├── remux-daemon/      # remuxd binary — session runtime daemon
│   ├── remux-cli/         # remux binary — CLI client + attach loop
│   ├── remux-tui/         # TUI session manager (ratatui-based)
│   └── remux-testkit/     # Integration test helpers (daemon harness, fake clients)
├── packaging/             # systemd/launchd service files
├── Cargo.toml             # Workspace root
└── docs/
```

### remux-core (library crate)

Shared types and logic for all components. Other Rust tools can use this crate directly.

- Protocol messages (`Request`, `Response`, `Event`)
- Error types (`RemuxError`)
- Config loading (`toml` + `serde`)
- Session models (`SessionMeta`, `SessionStatus`, `TermSize`)
- Session selectors (`SessionSelector`)
- Terminal snapshot types

### remux-daemon

Per-user background daemon. Owns PTYs and sessions.

- Unix domain socket server (tokio)
- Session registry (HashMap of `SessionRuntime`)
- PTY/process lifecycle (portable-pty + nix)
- Scrollback ring buffer
- Virtual terminal state via alacritty_terminal
- Session persistence (metadata to disk)
- Event fanout to attached clients
- Auto-spawned by CLI or managed by systemd/launchd

### remux-cli

Primary user interface.

- Commands: `new`, `ls`, `attach`, `detach`, `inspect`, `logs`, `rename`, `kill`
- Raw terminal mode for attach (crossterm)
- Input forwarding to daemon
- Output rendering from daemon stream
- JSON output mode (`--json`)
- Ctrl-Q to detach
- Auto-spawns daemon if not running

### remux-tui

Session management TUI (ratatui + crossterm).

- Session list with status, name, command, idle time
- Scrollback preview
- Quick attach/kill/detach
- Real-time session status updates

### remux-testkit

Integration test helpers.

- Daemon harness (start/stop remuxd in tests)
- PTY test utilities
- Fake clients for attach/detach/resize tests

---

## Data Model

### Session

```rust
pub struct SessionId(pub uuid::Uuid);

pub struct SessionMeta {
    pub id: SessionId,
    pub name: String,
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: SessionStatus,
    pub last_exit_code: Option<i32>,
    pub controlling_client: Option<ClientId>,
    pub attached_clients: Vec<ClientId>,
    pub last_size: TermSize,
}

pub enum SessionStatus {
    Starting,
    Running,
    Exited,
    Failed,
}

pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}
```

### Session Runtime (daemon-internal)

```rust
pub struct SessionRuntime {
    pub meta: SessionMeta,
    pub scrollback: ScrollbackBuffer,
    pub vt_state: VirtualTerminalState,
    pub process: ProcessHandle,
    pub pty: PtyHandle,
}
```

---

## IPC Protocol

Unix domain socket with framed message protocol.

### Encoding

- **Dev mode** (`cfg(debug_assertions)`): newline-delimited JSON
- **Release mode**: bincode with length-prefix framing

### Request Types

```rust
pub enum Request {
    Ping,
    ListSessions,
    CreateSession(CreateSessionRequest),
    InspectSession { session: SessionSelector },
    AttachSession { session: SessionSelector, size: TermSize, mode: AttachMode },
    DetachSession { session: SessionSelector },
    ResizeSession { session: SessionSelector, size: TermSize },
    SendInput { session: SessionSelector, data: Vec<u8> },
    ReadScrollback { session: SessionSelector, lines: usize },
    RenameSession { session: SessionSelector, new_name: String },
    KillSession { session: SessionSelector, signal: Option<i32> },
    SubscribeSessionEvents { session: SessionSelector },
}
```

### Response Types

```rust
pub enum Response {
    Pong,
    Ok,
    Error(RemuxError),
    SessionList(Vec<SessionSummary>),
    SessionDetails(SessionDetails),
    Created(SessionDetails),
    Attached(AttachBootstrap),
    Scrollback(ScrollbackChunk),
}
```

### Event Stream

```rust
pub enum Event {
    Output { session: SessionId, data: Vec<u8> },
    StateSnapshot { session: SessionId, snapshot: TerminalSnapshot },
    SessionUpdated(SessionSummary),
    SessionExited { session: SessionId, exit_code: Option<i32> },
    Error(RemuxError),
}
```

---

## CLI UX

### Commands

```bash
remux new [--name NAME] [-- COMMAND...]    # Create session
remux ls [--json] [--preview]              # List sessions
remux attach NAME                          # Attach to session
remux detach                               # Detach from current session (Ctrl-Q)
remux inspect NAME [--json]                # Show session details
remux logs NAME [--lines N]                # Show scrollback
remux rename OLD NEW                       # Rename session
remux kill NAME                            # Kill session

# Phase 2 - SSH
remux connect HOST                         # Connect to remote host
remux attach HOST:NAME                     # Attach to remote session
remux ls --host HOST                       # List remote sessions
```

### Session Naming

When `--name` is omitted:
1. Use basename of current working directory
2. If collision, error and suggest explicit `--name`
3. Optionally infer from command (e.g., `cargo test` in repo `api` → `api-test`)

### `remux ls` Output

```
NAME       STATUS   PID    CREATED   CWD              CMD
backend    run      4821   3h ago    ~/src/backend    bash
infra      run      4991   22m ago   ~/src/infra      just dev
db-migrate exited   -      4h ago    ~/src/backend    alembic upgrade head
```

### Detach

- **Ctrl-Q** detaches from current session
- Clear message: `[detached from session "backend"]`
- Session continues running

---

## Attach/Detach Flow

### Create + Attach

1. User runs `remux new --name backend`
2. CLI checks if daemon is running, auto-spawns if needed
3. CLI sends `CreateSession` to daemon over Unix socket
4. Daemon allocates PTY, spawns `$SHELL` (or `-- COMMAND`)
5. Daemon registers session, returns `Created(SessionDetails)`
6. CLI enters raw terminal mode, sends `AttachSession`
7. Daemon returns `AttachBootstrap` (metadata + scrollback + VT snapshot)
8. Daemon streams live `Output` events
9. CLI forwards keystrokes as `SendInput` requests
10. User presses Ctrl-Q → CLI sends `DetachSession`, exits raw mode

### Reattach

1. User runs `remux attach backend`
2. CLI sends `AttachSession` with current terminal size
3. Daemon returns `AttachBootstrap` with:
   - Session metadata
   - Recent scrollback from ring buffer
   - Current virtual terminal snapshot (cursor, styles, alt-screen state)
4. Daemon streams live output
5. Client renders VT snapshot + live stream → coherent reattach

### Attach Conflict

1. User B runs `remux attach backend` while User A is attached
2. Daemon demotes User A to observer (read-only)
3. User B becomes controlling client
4. User A sees `[session "backend" control taken by another client]`

---

## Configuration

### Paths

- Config: `~/.config/remux/config.toml`
- Data: `~/.local/share/remux/`
- State: `~/.local/state/remux/remuxd.sock`
- (Phase 2) Hosts: `~/.config/remux/hosts.toml`

### config.toml

```toml
[data]
dir = "~/.local/share/remux"

[daemon]
socket_path = "~/.local/state/remux/remuxd.sock"
max_scrollback_lines = 20000
persist_scrollback = false
cleanup_exited_after_hours = 168

[client]
default_shell = "$SHELL"
detach_key = "ctrl-q"
```

### hosts.toml (Phase 2)

```toml
[hosts.devbox]
host = "dev.example.com"
user = "mj"
port = 22
identity_file = "~/.ssh/id_ed25519"

[hosts.prod-builder]
host = "10.0.1.50"
# Inherits from ~/.ssh/config if not specified
```

---

## Phase 2 — SSH Transport

### Design

- Client uses `russh` to SSH into remote host
- Spawns a bridge process on the remote host
- Bridge connects to remote remuxd Unix socket
- Forwards the IPC protocol over the SSH channel
- User experience is native Remux, SSH is hidden

### Flow

```
remux CLI → russh SSH session → remote host → remux-bridge → remote remuxd socket
```

### CLI UX

```bash
remux connect devbox           # Verify connection, list sessions
remux attach devbox:backend    # Attach to remote session
remux new --host devbox api    # Create session on remote host
remux ls --host devbox         # List remote sessions
```

### Host Resolution

1. Check `~/.config/remux/hosts.toml` for host alias
2. Fall back to `~/.ssh/config` for matching Host entry
3. Error if host not found in either

---

## Tech Stack

| Component | Crate |
|-----------|-------|
| Async runtime | `tokio` |
| CLI parsing | `clap` |
| Serialization | `serde`, `serde_json`, `bincode` |
| Config | `toml` |
| UUID | `uuid` |
| Timestamps | `chrono` |
| Logging | `tracing`, `tracing-subscriber` |
| PTY | `portable-pty` |
| Unix APIs | `nix` |
| Terminal state | `alacritty_terminal` |
| TUI | `ratatui`, `crossterm` |
| SSH (Phase 2) | `russh` |

---

## Testing Strategy

### Integration Tests (priority)

- Create session → detach → reattach → process still alive
- Command continues running after client disconnect
- Resize propagates correctly to PTY
- Exited session visible in listing with correct exit code
- Scrollback available after reattach
- Alt-screen app smoke test (vim, less)
- Multiple sessions running simultaneously
- Duplicate session name returns error
- Auto-spawn daemon on first CLI use

### Unit Tests

- Session registry behavior
- Session naming logic
- Protocol serialization roundtrip
- Config parsing
- Scrollback ring buffer
- Ring buffer wrapping behavior

### Manual Test Matrix

- bash, zsh, fish
- vim, nano
- htop, top
- less, more
- cargo watch
- Long-running log tail
- git interactive rebase
- Ctrl-C, Ctrl-Z, Ctrl-D handling

### Verification Commands

```bash
cargo test          # All tests pass
cargo build         # Clean build
cargo clippy        # No warnings
```

---

## Service Management

### Linux (systemd)

`~/.config/systemd/user/remuxd.service`

```ini
[Unit]
Description=Remux terminal session daemon

[Service]
ExecStart=%h/.cargo/bin/remuxd
Restart=on-failure

[Install]
WantedBy=default.target
```

### macOS (launchd)

`~/Library/LaunchAgents/com.remux.daemon.plist`

---

## Failure Semantics

### Supported
- Session survives client disconnect
- Session survives client crash
- Session survives multiple detach/reattach cycles
- Daemon auto-restart via systemd/launchd

### NOT Supported
- Session survival across daemon crash
- Session survival across host reboot
- Seamless daemon upgrade without session loss

---

## Persistence Model

### Persist to Disk

- Session metadata (name, id, status, command, cwd, timestamps)
- Exited session records until cleanup policy removes them (default 168 hours)

### Do NOT Persist

- Full live PTY state across daemon restart
- Scrollback (in-memory only, lost on daemon crash)
- Seamless recovery after daemon crash

Crash recovery is explicitly out of scope for v1. The daemon should be robust, and can be supervised via systemd/launchd for auto-restart, but active sessions are lost if the daemon process itself crashes.

---

## Observability

Structured logging via `tracing` + `tracing-subscriber` from day one.

### Log Categories

- Daemon startup/shutdown
- Session create/attach/detach/exit
- PTY spawn errors
- IPC errors (socket read/write failures, malformed messages)
- Resize/input control events
- Process exit detection
- Auto-spawn events

---

## Resize Handling

The controlling client owns terminal size. When the controlling client resizes:

1. CLI sends `ResizeSession` with new dimensions
2. Daemon updates PTY size via `TIOCSWINSZ` ioctl
3. Daemon updates virtual terminal model metadata
4. PTY forwards SIGWINCH to child process group

Resize is only accepted from the controlling client. Observers receive output at whatever size the PTY is set to.

---

## Backpressure and Flow Control

A slow client must not stall session output processing.

- Daemon maintains bounded output queues per attached client
- If a client's queue is full, oldest messages are dropped
- On next client read, daemon can send a VT snapshot to restore coherent state
- PTY output is always consumed immediately regardless of client state
- This ensures the child process is never blocked by a slow consumer

---

## User Stories

### Phase 1 — Local Runtime

#### US-1: Workspace Bootstrap
**Description:** As a developer, I want to create a Rust workspace with remux-core, remux-daemon, remux-cli, remux-tui, and remux-testkit crates so that implementation can begin.

**Acceptance Criteria:**
- [ ] `cargo build` succeeds for all 5 crates
- [ ] `cargo test` passes (empty test suites)
- [ ] `cargo clippy` passes with no warnings
- [ ] Workspace has correct crate dependencies declared
- [ ] remux-core exposes protocol types, session models, error types, and config types
- [ ] remux-cli has clap skeleton with all subcommands defined
- [ ] remux-daemon has main entrypoint with tracing initialized
- [ ] remux-tui has ratatui skeleton rendering empty frame
- [ ] remux-testkit has daemon harness skeleton (start/stop daemon in tests)

#### US-2: Session Data Model and Protocol
**Description:** As a developer, I want the core data types and IPC protocol fully defined so that daemon and CLI can communicate.

**Acceptance Criteria:**
- [ ] `SessionMeta`, `SessionId`, `SessionStatus`, `TermSize` defined in remux-core
- [ ] `Request`, `Response`, `Event` enums fully defined
- [ ] `CreateSessionRequest`, `AttachBootstrap`, `SessionSummary` types defined
- [ ] `RemuxError` enum covers all error cases
- [ ] All types implement `Serialize`/`Deserialize` for both JSON and bincode
- [ ] Unit tests for serialization roundtrip (JSON mode)
- [ ] Unit tests for serialization roundtrip (bincode mode)
- [ ] `Config` struct with TOML deserialization and defaults

#### US-3: Daemon Unix Socket Server
**Description:** As a developer, I want remuxd to listen on a Unix domain socket and respond to protocol messages so that clients can connect.

**Acceptance Criteria:**
- [ ] Daemon binds to `~/.local/state/remux/remuxd.sock`
- [ ] Socket permissions restricted to current user (0700)
- [ ] Daemon handles `Ping` → returns `Pong`
- [ ] Daemon handles `ListSessions` → returns empty list
- [ ] Daemon handles unknown requests → returns `Error`
- [ ] Multiple clients can connect simultaneously
- [ ] Client disconnect does not crash daemon
- [ ] Integration test: connect to socket, send Ping, receive Pong

#### US-4: PTY Session Creation
**Description:** As a user, I want to create a named session that spawns a shell in a PTY so I have a durable terminal session.

**Acceptance Criteria:**
- [ ] `remux new --name backend` creates a session named "backend"
- [ ] Session spawns `$SHELL` (or fallback `/bin/sh`) in a new PTY
- [ ] Session appears in `remux ls` with status "run", PID, and creation time
- [ ] `remux new --name backend` errors if "backend" already exists
- [ ] `remux new` (no name) auto-names from cwd basename
- [ ] `remux new --name test -- /bin/bash -c "echo hello"` runs custom command
- [ ] Session working directory matches user's cwd at creation time
- [ ] `remux ls --json` returns valid JSON array with session details
- [ ] `remux ls --preview` shows last few lines of scrollback for each running session
- [ ] Integration test: create session, verify it appears in list, verify process is running

#### US-5: Session Attach/Detach Loop
**Description:** As a user, I want to attach to a session, interact with it, and detach without killing the process so my work continues.

**Acceptance Criteria:**
- [ ] `remux attach backend` enters raw terminal mode
- [ ] User sees live shell prompt from the session
- [ ] Keystrokes are forwarded to the session's PTY
- [ ] Session output appears in the terminal
- [ ] Ctrl-Q detaches cleanly with message `[detached from session "backend"]`
- [ ] After detach, the shell process is still running (verified via PID check)
- [ ] `remux attach backend` again reconnects successfully
- [ ] Attaching to non-existent session shows clear error
- [ ] Attaching to exited session shows clear message with exit code
- [ ] Integration test: create session, attach, detach, verify process alive, reattach

#### US-6: Scrollback Buffer
**Description:** As a user, I want to see output that was produced before I attached so I don't lose context when reconnecting.

**Acceptance Criteria:**
- [ ] Daemon maintains in-memory ring buffer (default 20000 lines)
- [ ] On attach, client receives scrollback from ring buffer
- [ ] `remux logs backend` shows scrollback output
- [ ] `remux logs backend --lines 50` shows last 50 lines
- [ ] Ring buffer wraps correctly (oldest lines dropped when full)
- [ ] Integration test: create session, produce output, detach, reattach, see previous output

#### US-7: Virtual Terminal State
**Description:** As a user, I want reattach to show coherent terminal state (cursor, colors, alt-screen) so that full-screen apps like vim look correct.

**Acceptance Criteria:**
- [ ] Daemon feeds PTY output through alacritty_terminal VT parser
- [ ] On attach, client receives VT snapshot (not raw byte replay)
- [ ] Reattach after running vim shows correct screen state
- [ ] Reattach after running less shows correct screen state
- [ ] Cursor position preserved on reattach
- [ ] Text colors/styles preserved on reattach
- [ ] Alt-screen state handled correctly (vim exits cleanly back to shell)
- [ ] Integration test: start vim in session, detach, reattach, verify vim renders correctly

#### US-8: Session Kill and Cleanup
**Description:** As a user, I want to kill sessions and have exited sessions cleaned up so my session list stays manageable.

**Acceptance Criteria:**
- [ ] `remux kill backend` terminates the session's process group
- [ ] Killed session appears in `remux ls` with status "exited" and exit code
- [ ] `remux kill backend` on already-exited session is a no-op (not an error)
- [ ] `remux kill nonexistent` returns clear error
- [ ] Exited sessions cleaned up after configurable time (default 168 hours)
- [ ] Sessions that exit naturally (shell exit) marked as "exited"
- [ ] Integration test: create session, kill it, verify process terminated, verify listing updated

#### US-9: Session Inspect and Rename
**Description:** As a user, I want to inspect session details and rename sessions so I can organize my work.

**Acceptance Criteria:**
- [ ] `remux inspect backend` shows name, status, PID, cwd, command, created_at, attached clients
- [ ] `remux inspect backend --json` returns valid JSON with all details
- [ ] `remux rename backend api-backend` renames the session
- [ ] `remux attach api-backend` works after rename
- [ ] `remux rename` to an already-taken name returns error
- [ ] Integration test: create session, rename, attach by new name, verify old name fails

#### US-10: Auto-Spawn Daemon
**Description:** As a user, I want the daemon to start automatically when I use any CLI command so I don't need to manage it manually.

**Acceptance Criteria:**
- [ ] `remux ls` auto-spawns remuxd if not running
- [ ] `remux new` auto-spawns remuxd if not running
- [ ] `remux attach` auto-spawns remuxd if not running
- [ ] Auto-spawn respects socket path from config
- [ ] If daemon fails to start, clear error message with troubleshooting hint
- [ ] If daemon is already running, no second instance is spawned (socket lock)
- [ ] Integration test: kill daemon, run CLI command, verify daemon restarts and command succeeds

#### US-11: TUI Session Manager
**Description:** As a user, I want a TUI interface to browse, preview, and manage sessions so I can see all my work at a glance.

**Acceptance Criteria:**
- [ ] `remux tui` (or `remux`) opens TUI session manager
- [ ] TUI shows list of all sessions with name, status, command, idle time
- [ ] Arrow keys navigate session list
- [ ] Pressing Enter attaches to selected session
- [ ] Pressing 'k' kills selected session (with confirmation)
- [ ] TUI shows scrollback preview for selected session
- [ ] TUI updates in real-time when sessions change status
- [ ] Ctrl-Q exits TUI back to shell
- [ ] TUI works correctly at various terminal sizes (80x24 to full screen)
- [ ] Integration test: create multiple sessions, open TUI, verify all visible, attach via TUI

#### US-12: Service Management and Observability
**Description:** As a user, I want systemd/launchd service files so remuxd auto-starts and restarts on crash, and I want structured logging for debugging.

**Acceptance Criteria:**
- [ ] systemd user service file provided in packaging/
- [ ] launchd plist provided in packaging/
- [ ] `systemctl --user enable remuxd` works
- [ ] `systemctl --user start remuxd` starts daemon
- [ ] Daemon restarts on crash when managed by systemd
- [ ] Documentation for setting up service management
- [ ] `tracing` initialized in daemon with structured log output
- [ ] Log categories emit: startup, session create/attach/detach/exit, PTY errors, IPC errors
- [ ] Log level configurable via config or RUST_LOG env var

### Phase 2 — SSH Transport

#### US-13: SSH Connection Infrastructure
**Description:** As a developer, I want russh-based SSH connection handling so the CLI can reach remote remuxd instances.

**Acceptance Criteria:**
- [ ] russh dependency added to remux-cli
- [ ] SSH connection establishes using key from ~/.ssh/ or ssh-agent
- [ ] Host resolution checks hosts.toml first, then ~/.ssh/config
- [ ] Connection timeout handled gracefully (default 10s)
- [ ] Authentication failure shows clear error
- [ ] Unit tests for host config parsing
- [ ] Unit tests for SSH config fallback

#### US-14: Remote Bridge Process
**Description:** As a developer, I want a bridge process that runs on the remote host and connects to the local remuxd socket so the protocol can be forwarded over SSH.

**Acceptance Criteria:**
- [ ] Bridge process spawned on remote host via SSH
- [ ] Bridge connects to remote remuxd Unix socket
- [ ] Bridge forwards IPC protocol messages bidirectionally
- [ ] Bridge exits cleanly when SSH channel closes
- [ ] Bridge auto-detects remote remuxd socket path
- [ ] Integration test: start local daemon, start bridge, verify Ping/Pong through bridge

#### US-15: Remote Session Management
**Description:** As a user, I want to list, create, and attach to sessions on remote hosts so I don't need to SSH manually.

**Acceptance Criteria:**
- [ ] `remux connect devbox` verifies connection and lists remote sessions
- [ ] `remux ls --host devbox` lists remote sessions
- [ ] `remux new --host devbox --name api` creates remote session
- [ ] `remux attach devbox:api` attaches to remote session
- [ ] Remote attach shows live terminal, keystrokes forwarded
- [ ] Ctrl-Q detaches from remote session cleanly
- [ ] Connection drops handled gracefully with reconnection hint
- [ ] Integration test: create remote session, attach, detach, reattach

#### US-16: hosts.toml Configuration
**Description:** As a user, I want to configure remote hosts with SSH settings so I can connect easily without remembering connection details.

**Acceptance Criteria:**
- [ ] `~/.config/remux/hosts.toml` parsed for host definitions
- [ ] Host aliases work: `remux connect devbox` resolves to full connection info
- [ ] identity_file, port, user configurable per host
- [ ] SSH config fallback works for hosts not in hosts.toml
- [ ] Missing host shows error with available hosts
- [ ] Unit tests for host config parsing and resolution

---

## Implementation Phases

### Phase 1: Local Runtime (US-1 through US-12)

#### Step 1: Bootstrap (US-1)
- Create Cargo workspace
- Scaffold all 4 crates with basic structure
- Verify: `cargo build && cargo test && cargo clippy`

#### Step 2: Core Types (US-2)
- Define all protocol types, session models, error types, config
- Add serialization tests
- Verify: `cargo test`

#### Step 3: Daemon Socket (US-3)
- Implement Unix socket server in remux-daemon
- Handle Ping, ListSessions
- Verify: `cargo test` (integration test connects to socket)

#### Step 4: Session Creation (US-4)
- Implement PTY spawning and session registry
- Wire CreateSession + ListSessions to CLI
- Verify: `remux new --name test && remux ls`

#### Step 5: Attach/Detach (US-5)
- Implement raw terminal mode in CLI
- Wire AttachSession, SendInput, DetachSession
- Verify: `remux attach test`, interact, Ctrl-Q, reattach

#### Step 6: Scrollback (US-6)
- Implement ring buffer in daemon
- Wire ReadScrollback and attach bootstrap
- Verify: Produce output, detach, reattach, see history

#### Step 7: VT State (US-7)
- Integrate alacritty_terminal for VT parsing
- Include VT snapshot in attach bootstrap
- Verify: Start vim, detach, reattach, vim renders correctly

#### Step 8: Kill, Inspect, Rename (US-8, US-9)
- Wire KillSession, InspectSession, RenameSession
- Implement cleanup policy
- Verify: `remux kill`, `remux inspect`, `remux rename`

#### Step 9: Auto-Spawn (US-10)
- Implement daemon auto-spawn in CLI
- Socket locking to prevent double-daemon
- Verify: Kill daemon, run command, daemon restarts

#### Step 10: TUI (US-11)
- Implement ratatui TUI with session list
- Wire to daemon for real-time updates
- Verify: `remux tui`, browse sessions, attach via TUI

#### Step 11: Polish (US-12)
- Service management files
- Error message refinement
- Config file loading
- Verify: `cargo test && cargo clippy && cargo build --release`

### Phase 2: SSH Transport (US-13 through US-16)

#### Step 12: SSH Connection (US-13)
- Add russh dependency
- Implement host resolution (hosts.toml + SSH config)
- Verify: `cargo test` (SSH config parsing tests)

#### Step 13: Remote Bridge (US-14)
- Implement bridge process
- Bidirectional protocol forwarding
- Verify: Integration test through bridge

#### Step 14: Remote CLI Commands (US-15, US-16)
- Wire `connect`, `ls --host`, `new --host`, `attach host:name`
- Implement hosts.toml
- Verify: `remux connect localhost`, `remux attach localhost:test`

---

## Verification

After each step:

```bash
cargo test
cargo build
cargo clippy
```

Full Phase 1 verification:

```bash
# Start fresh
pkill remuxd 2>/dev/null || true
rm -rf ~/.local/state/remux/

# Create session
remux new --name test-session
remux ls | grep test-session

# Attach, produce output, detach
remux attach test-session  # type "echo hello_remux", Ctrl-Q

# Verify session still running
remux ls | grep "run.*test-session"

# Reattach, verify scrollback
remux attach test-session  # should see "hello_remux" in scrollback, Ctrl-Q

# Inspect
remux inspect test-session --json | jq .

# Kill
remux kill test-session
remux ls | grep "exited.*test-session"
```

---

## Non-Functional Requirements

- **Memory**: Scrollback bounded by configurable limit (default 20000 lines)
- **Latency**: Keystroke-to-screen < 50ms on local socket
- **Reliability**: Daemon must survive client crashes without session loss
- **Security**: Unix socket restricted to owning user, no network listener
- **Compatibility**: bash, zsh, fish; vim, htop, less; Linux, macOS

---

## Divergences from spec.md

The original spec.md describes 4 phases (local, SSH, gateway/web, fleet). This implementation spec narrows to 2 phases based on user direction:

| spec.md | Implementation Spec | Reason |
|---------|-------------------|--------|
| 4 phases (local + SSH + gateway + fleet) | 2 phases (local + SSH) | User: "skip web/gateway, no fleet. CLI tool that other tools integrate." |
| Phase 3 gateway + web dashboard | Removed | User: "not sure why we need a dashboard" |
| Phase 4 fleet control plane | Removed | User: "not sure we need this" |
| `vt100` first, evaluate later | `alacritty_terminal` committed | User chose alacritty_terminal for robustness |
| Session naming: "append suffix on collision" | Error on duplicate name | User chose explicit error |
| `default_shell = "/bin/bash"` in config | `$SHELL` env var, fallback `/bin/sh` | User chose env var respect |
| `remux-tui` optional, later phase | Part of Phase 1 MVP | User wanted TUI from the start |
| No detach key specified | Ctrl-Q to detach | User chose Ctrl-Q |
| No attach conflict resolution specified | Steal control | User chose steal behavior |
