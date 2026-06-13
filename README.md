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
- **remux-cli** — command-line interface with auto-daemon-spawn (and SSH remote transport)
- **remux-tui** — ratatui-based session browser
- **remux-gateway** — TLS HTTP/WebSocket server exposing the agent-native `/v1` API
- **remux-testkit** — test harness and client for integration tests

## Build

Requires Rust 1.75+ and a C compiler (for `libc`/`nix` bindings).

```sh
cargo build
cargo build --release
```

Produces four binaries:

| Binary          | Crate         | Purpose                            |
|-----------------|---------------|------------------------------------|
| `remux`         | remux-cli     | CLI client                         |
| `remuxd`        | remux-daemon  | Background session daemon          |
| `remux-tui`     | remux-tui     | Terminal UI (also `remux ui`)      |
| `remux-gateway` | remux-gateway | TLS HTTP/WebSocket API server      |

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

### Remote access over SSH

Any command takes a global `--host <ssh-target>` flag to run against a remote
host's daemon — no exposed ports, no extra config. It works by running the same
framed protocol over `ssh <host> remux bridge` (a hidden connect-and-pipe
subcommand), so the remote `remuxd` stays Unix-socket-only:

```sh
remux --host devbox ls                 # list sessions on devbox
remux --host devbox attach backend     # attach to a remote session
remux --host devbox send build --text "make\n"
```

SSH provides the auth and encryption (your keys, agent, config, and
`known_hosts`); remux adds nothing to the trusted core. `remux` must be on the
remote host's `PATH`.

### Fleet (multi-host)

Register a static set of hosts and query them all at once. `remux fleet`
(alias `f`) is **client-side fan-out over the SSH transport above** — it runs
`ssh <host> remux bridge` against each configured host concurrently. There is
**no control-plane service**: no daemon registry, no gateway, no RBAC. The
federated control plane (intent routing, cross-host migration) is still future
work (see [`docs/AGENT_API_PLAN.md`](docs/AGENT_API_PLAN.md) §8).

Configure hosts in your config file with `[[fleet.hosts]]` blocks:

```toml
[[fleet.hosts]]
name = "devbox"
ssh = "user@devbox"
labels = { project = "api", env = "dev" }

[[fleet.hosts]]
name = "prod1"
ssh = "ops@prod1.example.com"
labels = { env = "prod" }
```

Then:

```sh
remux fleet hosts                      # list configured hosts + labels
remux fleet ls                         # sessions across the whole fleet
remux fleet ls --json                  # [{ host, ssh, ok, error, sessions }]
remux fleet ls --label env=dev         # only hosts matching ALL given labels
remux fleet attach devbox:backend      # resolve `devbox` → ssh, attach `backend`
```

`fleet ls` queries hosts in parallel and adds a leading `HOST` column.
**Unreachable hosts never abort the command**: a host that fails to connect is
shown as `unreachable` (with the error in place of the command) and the
reachable hosts still list normally. In `--json` each host carries
`"ok": false` and an `"error"` string. `fleet attach <host>:<session>` resolves
`<host>` as a registry **name** (not an ssh target) and reuses the same remote
attach path as `--host`, erroring clearly if the name isn't registered.

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

## Gateway (HTTP / WebSocket API)

`remux-gateway` exposes the daemon as an authenticated, TLS-secured **agent-native
`/v1` API** — the thing a plain web terminal can't do: structured session state,
not just a byte stream. It runs as a **separate process** and connects to the
daemon over the local Unix socket; **`remuxd` never listens on a network port**.

```sh
remux-gateway                                    # TLS on 127.0.0.1:8443; self-signed cert + random token (logged)
remux-gateway --listen 0.0.0.0:8443 \
  --token "$RW_TOKEN" --read-token "$RO_TOKEN" \
  --auth-config auth.toml \
  --tls-cert cert.pem --tls-key key.pem
```

- **TLS always on.** Supply `--tls-cert`/`--tls-key`, or a self-signed cert is
  generated for `127.0.0.1`/`localhost` (fingerprint logged).
- **Principal + RBAC bearer auth, deny-by-default.** A presented bearer token
  resolves (constant-time) to a **principal** (`{subject, roles}`); each route
  requires a fine-grained **permission**, and the principal's roles are evaluated
  against a **policy** (the shared `remux-authz` model). An unknown/missing token
  is `401`; a known principal lacking the route's permission is `403`. Every
  request is audit-logged (method, path, status, principal subject + roles,
  hashed token id, peer, latency — never the raw token).
- **Back-compat token flags.** `--token`/`REMUX_GATEWAY_TOKEN` maps to the
  built-in **`admin`** role (all gateway permissions); optional
  `--read-token`/`REMUX_GATEWAY_READ_TOKEN` maps to the built-in **`viewer`**
  role (read-only). So the old read-write / read-only behaviour is preserved:
  a `viewer` token may call the `GET` endpoints, `POST /wait`, and the `/events`
  stream, but mutating routes and the `/stream` socket require a writing role
  (`403` otherwise).

#### Gateway roles & permissions

| Built-in role | Permissions |
|---|---|
| `viewer`   | `session.list`, `session.read`, `session.wait`, `events.read` |
| `operator` | `viewer` + `session.create`, `session.input`, `session.resize`, `session.kill`, `session.rename`, `session.stream` |
| `admin`    | every gateway permission |

#### `--auth-config` (principal-shaped tokens + custom roles)

`--auth-config <FILE>` (env `REMUX_GATEWAY_AUTH_CONFIG`) loads a TOML file that
adds principal-shaped tokens and optional custom roles. Custom roles are merged
**over** the built-ins; its tokens are layered on top of the back-compat flags
(a flag token wins a duplicate-secret collision):

```toml
[[tokens]]
token = "ci-secret"            # the bearer secret
subject = "ci-bot"             # audit identity
roles = ["operator"]           # built-in or custom role names

[[roles]]                      # optional custom roles
name = "deployer"
permissions = ["session.create", "session.input", "session.read"]
```

A `deployer` token above can create + input + read, but **not** kill (it lacks
`session.kill`).

#### JWT / OIDC bearer (Phase B)

A **JWT** (e.g. from an OIDC provider) can be used as the bearer credential
instead of — or alongside — static tokens. The gateway validates the token and
maps its claims to a principal, so **JWT callers use the exact same RBAC roles
and the same 401/403 semantics** as static tokens. Static tokens keep working
unchanged; if no JWT flag is set, behaviour is exactly as before.

Resolution order: a presented bearer is checked against the static tokens
**first** (constant time); only on a miss is it validated as a JWT. A JWT that
validates but whose roles lack the route's permission is `403` (same as static);
an expired / wrong-issuer / wrong-audience / bad-signature / unknown token is
`401`.

Pick **one** key source:

| Flag (env) | Key source |
| --- | --- |
| `--jwt-hs256-secret` (`REMUX_GATEWAY_JWT_HS256_SECRET`) | HS256 shared secret (symmetric) |
| `--jwt-public-key <PEM>` (`REMUX_GATEWAY_JWT_PUBLIC_KEY`) | a static RS256/ES256 public-key PEM file (offline-friendly) |
| `--jwt-jwks-url <URL>` (`REMUX_GATEWAY_JWT_JWKS_URL`) | a JWKS endpoint fetched over HTTPS, cached in-memory and refreshed on a TTL |

Plus the optional claim configuration (env equivalents
`REMUX_GATEWAY_JWT_ISSUER` / `_AUDIENCE` / `_ROLES_CLAIM`):

- `--jwt-issuer <ISS>` — require this `iss` (otherwise `iss` is not checked).
- `--jwt-audience <AUD>` — require this `aud` (otherwise `aud` is not checked).
- `--jwt-roles-claim <CLAIM>` — the claim to read roles from (default `roles`).
  The claim may be a **JSON array of strings** *or* a **space-delimited string**
  (OIDC `scope` style), so `"roles":["operator"]` and `"scope":"viewer operator"`
  both work. The subject defaults to the `sub` claim.
- `--jwt-jwks-ttl <SECS>` (default 300) and `--jwt-jwks-tls-insecure` tune the
  JWKS-URL path; on a refresh failure the last good key set keeps serving.

```sh
# HS256, mapping the token's `roles` claim to gateway RBAC roles:
remux-gateway --listen 0.0.0.0:8443 \
  --token "$RW_TOKEN" \
  --jwt-hs256-secret "$JWT_SECRET" \
  --jwt-issuer https://idp.example/ --jwt-audience remux

# Or an OIDC provider's JWKS (RS256/ES256), reading roles from `scope`:
remux-gateway --jwt-jwks-url https://idp.example/.well-known/jwks.json \
  --jwt-issuer https://idp.example/ --jwt-roles-claim scope
```

The audit line records the auth method (`static` vs `jwt`) alongside the
principal's subject and roles (never the token). The control plane takes the same
`--jwt-*` flags (env prefix `REMUX_CP_JWT_*`) for its `/cp/v1` fleet API.

### Auto-join a control plane (`--register`)

A gateway can **register itself** with a [control plane](#control-plane-fleet-federation)
on startup so the fleet is self-assembling — no external caller has to POST the
registration. It dials **outbound** to the control plane (the daemon still never
listens on a network port) and keeps the registration fresh with a background
heartbeat.

```sh
remux-gateway --listen 0.0.0.0:8443 --token "$RW_TOKEN" \
  --register https://cp.internal:9443 \
  --register-token "$REGISTER_TOKEN" \
  --advertise-url https://10.0.0.4:8443 \
  --register-name web-1 --label env=dev --label region=us
```

- `--register <CP_URL>` turns it on; `--register-token`/`REMUX_GATEWAY_REGISTER_TOKEN`
  is the control plane's register token.
- `--advertise-url <URL>` is the gateway's externally-reachable base URL the
  control plane dials back (defaults to `https://<--listen>` — set it explicitly
  when binding a wildcard address). The gateway hands the control plane its own
  read-write `--token` so the CP can call its `/v1` API.
- `--register-name <NAME>` defaults to the system hostname; `--label k=v` is
  repeatable (used for fan-out / intent routing); `--register-ttl <SECS>`
  (default 30) sets the registration TTL — the heartbeat runs every `ttl/2`.
- On startup the gateway POSTs `/cp/v1/register`, then heartbeats; on
  SIGTERM/SIGINT it best-effort `DELETE`s `/cp/v1/hosts/{name}`. Registration
  failures **never crash the gateway** — they're logged and retried with bounded
  backoff while the `/v1` API keeps serving.
- `--register-tls-insecure` (default **true** for v1) trusts the control plane's
  self-signed cert; pinning is the deferred follow-up.

### REST endpoints

```
GET    /v1/health                      # liveness (no auth)
GET    /v1/openapi.json                # OpenAPI 3.1 document (no auth)
GET    /v1/sessions                    # list
POST   /v1/sessions                    # create  -> 201 SessionView
GET    /v1/sessions/{id}               # inspect ({id} = uuid or name)
DELETE /v1/sessions/{id}               # kill
PATCH  /v1/sessions/{id}               # rename
POST   /v1/sessions/{id}/input         # send input ({text}|{bytes_hex}|raw) -> 202
GET    /v1/sessions/{id}/screen        # current screen as JSON cells (ScreenView)
GET    /v1/sessions/{id}/scrollback    # ?lines=N
POST   /v1/sessions/{id}/resize        # {cols, rows}
POST   /v1/sessions/{id}/wait          # {kind: idle|regex|exit} ?timeout_ms=
```

### WebSocket channels

- `wss://…/v1/sessions/{id}/stream` — an attachable terminal: **binary** frames
  carry raw I/O byte-exact (output → client, input → daemon); a **text** frame
  `{"type":"resize","cols":N,"rows":N}` resizes. Requires the `session.stream`
  permission (the `operator`/`admin` roles).
- `wss://…/v1/sessions/{id}/events` — **structured JSON** lifecycle events
  (exited/updated/…). The `events.read` permission (any role incl. `viewer`) is
  enough.

WS clients pass the token via `?token=…` (browser-friendly) or the
`Authorization` header. The published contract is decoupled from the internal
IPC protocol, so the wire can evolve under `PROTOCOL_VERSION` without breaking
`/v1`; see `docs/openapi.yaml` and `docs/AGENT_API_PLAN.md`.

### Browser client

Visiting `https://<host>:8443/` serves a **built-in xterm.js terminal** — a
minimal reference client baked into the gateway binary (no separate web build).
Paste a bearer token into the field (or open `…/?token=<token>` to prefill it),
pick a session from the sidebar, and the page attaches over the binary
`/stream` WebSocket. It is a deliberately thin consumer of `/v1`; it does not do
collaboration, recording, or multi-session layouts.

- `--no-web-ui` disables it (then `GET /` returns `404`); the `/v1` API is
  unaffected.
- xterm.js loads from a CDN (jsdelivr); vendoring it for fully offline /
  air-gapped use is a follow-up.

## Control plane (fleet federation)

`remux-control-plane` is a **separate TLS service** that federates over many
gateways — one pane across a fleet for both humans and agents. It is the AW6
control-plane core (`spec.md` §10, `docs/AGENT_API_PLAN.md` §8).

```sh
remux-control-plane                              # TLS on 127.0.0.1:9443; self-signed cert + admin/register tokens (logged)
remux-control-plane --listen 0.0.0.0:9443 \
  --token "$ADMIN_TOKEN" --register-token "$REGISTER_TOKEN" \
  --auth-config auth.toml \
  --tls-cert cert.pem --tls-key key.pem
```

**Security model.** The daemon stays **local-only** (Unix socket, no network
listener). Gateways **register outbound** to the control plane — the control
plane never dials a host it was not first told about, so no inbound listener is
added anywhere new. The control plane uses the **same principal + RBAC model**
as the gateway (the shared `remux-authz` crate), deny-by-default, constant-time
token resolution. An unknown/missing token is `401`; a known principal lacking
the route's permission is `403`. Every request is audit-logged (method, path,
status, principal subject + roles, hashed token id, peer, latency — never raw
tokens). v1 **trusts self-signed gateway certs** (`--gateway-tls-insecure`,
default `true`, logged as a warning); gateway-cert pinning / CA trust is the
remaining Phase C follow-up.

**Back-compat token flags.** `--token`/`REMUX_CP_TOKEN` maps to the built-in
**`fleet-admin`** role (every control-plane permission, a superuser that may also
register); `--register-token`/`REMUX_CP_REGISTER_TOKEN` maps to the
lower-privilege **`registrar`** role (register / heartbeat / deregister only).

#### Control-plane roles & permissions

| Built-in role    | Permissions |
|---|---|
| `registrar`      | `host.register` (register / heartbeat / deregister) |
| `fleet-viewer`   | `fleet.hosts.read`, `fleet.sessions.read` |
| `fleet-operator` | `fleet-viewer` + `fleet.resolve` |
| `fleet-admin`    | every control-plane permission |

`--auth-config <FILE>` (env `REMUX_CP_AUTH_CONFIG`) adds principal-shaped tokens
and custom roles using the **same TOML format** as the gateway (above). For
example, a token bound to `fleet-viewer` can list hosts and read federated
sessions but is `403` on `POST /cp/v1/resolve`.

### Endpoints

```
GET    /cp/v1/health                   # liveness (no auth)
POST   /cp/v1/register                 # gateway joins: {name,url,labels,token,ttl_secs?}  (host.register)
POST   /cp/v1/heartbeat                # refresh last_seen: {name}                          (host.register)
DELETE /cp/v1/hosts/{name}             # deregister                                         (host.register)
GET    /cp/v1/hosts                    # list {name,url,labels,last_seen,healthy}           (fleet.hosts.read)
GET    /cp/v1/sessions[?label=k=v]…    # concurrent fan-out of /v1/sessions, tagged by host (fleet.sessions.read)
POST   /cp/v1/resolve                  # intent routing: {labels,command?,reuse_name?}      (fleet.resolve)
```

- **Registry** — an in-memory map keyed by host name; registration is an
  idempotent upsert that sets `last_seen=now`. A host is `healthy` while
  `now - last_seen < ttl`; expired hosts are excluded from fan-out.
- **Federated sessions** — fans out `GET /v1/sessions` concurrently to every
  healthy host matching **all** `label=k=v` selectors, aggregating the results
  tagged by host. An unreachable or erroring host is reported per-host
  (`{ host, url, ok:false, error, sessions:[] }`) and **never** fails the whole
  query.
- **Resolve** — picks the first healthy host matching all labels
  (deterministic), returns an existing session of `reuse_name` if one is live,
  else creates one via that gateway's `POST /v1/sessions`, responding
  `{ host, gateway_url, session_id, name, created }`.

### `remux open` — intent routing

`remux open` is the human/agent front-end for the control plane's `resolve`
endpoint. You describe **intent** (labels); the control plane decides **which**
host and session satisfies it (reusing a live one or creating a new one); then
the CLI **attaches** if it knows how to reach that host.

```sh
remux open --control-plane https://cp.internal:9443 --token "$ADMIN_TOKEN" \
  --label project=api --label env=dev --reuse api-shell -- /bin/bash
remux o --label env=dev          # alias `o`; --control-plane/--token fall back to config/env
remux open --label env=dev --json   # print-only branch as JSON
```

- `--control-plane <URL>` / `--token <TOK>` fall back to the `[control_plane]`
  config section, then the `REMUX_CP_URL` / `REMUX_CP_TOKEN` environment
  variables.
- `--label k=v` (repeatable) selects the host; `--reuse <name>` reuses an
  existing same-named session if present; a trailing `--command`/command is what
  the control plane creates if none exists.
- **The split:** the control plane returns `{ host, gateway_url, session_id,
  name, created }`. If that `host` is in your local `[[fleet.hosts]]` registry
  with an `ssh` target, `remux open` attaches over SSH to the resolved session
  (the same remote-attach path as `--host`/`fleet attach`). If the host isn't in
  your local registry, it **doesn't fail** — it prints the resolved target (or
  JSON with `--json`) and hints that adding the host to `[[fleet.hosts]]` enables
  auto-attach, or that the gateway's browser UI can be used. So the control plane
  owns *intent → host/session*, and the local fleet registry owns *host → SSH
  reachability*.

RBAC/OIDC/mTLS, gateway-cert pinning, and cross-host migration remain the
explicitly-deferred next steps.

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

# Optional client-side fleet registry for `remux fleet` (fan-out over SSH).
[[fleet.hosts]]
name = "devbox"                                     # logical name (fleet attach devbox:...)
ssh = "user@devbox"                                 # ssh target
labels = { project = "api", env = "dev" }           # filter with `fleet ls --label k=v`

# Optional control-plane endpoint for `remux open` (intent routing). The flag
# (--control-plane/--token) and REMUX_CP_URL/REMUX_CP_TOKEN envs override these.
[control_plane]
url = "https://cp.internal:9443"                    # control-plane base URL
token = "admin-token"                               # admin bearer token
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
