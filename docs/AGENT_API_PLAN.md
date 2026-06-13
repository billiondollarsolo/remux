# Remux Agent-Native API & Fleet Plan

> **Status:** Proposed (design). This document is the source of truth for the
> work that takes `remux` from "a robust local session runtime" (see
> [`ROBUSTNESS_PLAN.md`](./ROBUSTNESS_PLAN.md), WS0–WS6, landed) to the thing
> that actually makes it *defensible*: a **structured, agent-native control API
> and fleet model** layered on the durable, parsed-state session runtime.
>
> Where `ROBUSTNESS_PLAN.md` was exclusively about making the **local runtime**
> correct, faithful, scriptable, and trustworthy, this plan builds the **product
> differentiator** on top of that foundation. It assumes the runtime is done:
> faithful reattach, raw passthrough, the `send`/`peek`/`wait` automation
> surface, `CaptureScreen`, the controller/observer model, and the
> `PROTOCOL_VERSION` handshake all exist today and are tested.
>
> This plan touches the long-term `spec.md` roadmap (§7 SSH transport, §8
> gateway, §10 fleet, §11 security) and makes opinionated, concrete decisions
> about *what* of it we build, *in what order*, and — critically — *what we are
> NOT building*.
>
> **Security invariant (non-negotiable, per `spec.md` §11):** `remuxd` stays
> **Unix-socket-only, same-user-only, no network listener — ever.** All remote
> and API access is via a **separate process** (an SSH bridge, or a gateway that
> terminates TLS/auth and talks to the daemon over the local socket). We never
> bolt a network listener into the daemon.

---

## 0. Context & Motivation

### 0.1 The trap: competing on "terminal in a browser"

The obvious "phase 3" reading of `spec.md` §8–§9 is: *add an HTTPS server, stream
the PTY over a WebSocket, render it with xterm.js, ship a browser terminal.*
That is a trap. **A browser web-terminal / WebSocket gateway is commoditized
table stakes, not a differentiator.** The space is saturated:

| Prior art | What it is | Browser terminal? |
| --- | --- | --- |
| [`ttyd`](https://github.com/tsl0922/ttyd) | C; share a terminal over the web | ✅ |
| [`gotty`](https://github.com/yudai/gotty) | Go; turn a CLI into a web app | ✅ |
| [`Wetty`](https://github.com/butlerx/wetty) | Node; SSH/login over HTTP | ✅ |
| [`sshx`](https://github.com/ekzhang/sshx) | Rust; collaborative web terminal | ✅ (multiplayer) |
| [`tmate`](https://tmate.io/) | tmux fork; instant terminal sharing | ✅ (+ SSH) |
| [`code-server`](https://github.com/coder/code-server) | VS Code in the browser, integrated terminal | ✅ |
| Google **Cloud Shell** / AWS CloudShell | managed browser shells | ✅ |
| **Coder**, **Gitpod**, **Codespaces** | cloud dev environments | ✅ |

Every one of these already does "pixels of a terminal, in a browser tab, over a
socket." If remux's headline is *also* "terminal in a browser," we are fighting
a dozen funded incumbents on **their** turf, with their feature set (sharing,
auth, multiplayer, collab cursors) as the bar. We lose that race by default.

### 0.2 The actual differentiator: structured runtime state

Remux already has something nearly none of those tools expose: a **daemon that
maintains parsed VT state and exposes it as structured primitives** over a typed
protocol. Concretely, *today*, verified against the code:

| Primitive | Where it lives | What it gives an automated caller |
| --- | --- | --- |
| `Request::SendInput { session, data: Vec<u8> }` | `protocol.rs:83`; CLI `cmd/send.rs` | Binary-safe input injection **without attaching / without stealing control** |
| `Request::CaptureScreen` → `Response::Screen(TerminalSnapshot)` | `protocol.rs:99,164`; CLI `cmd/peek.rs` | The **current screen as structured JSON cells** (color, attrs, cursor, alt-screen) — not a byte scrape |
| `remux wait --idle / --for-regex / --exit` | `cmd/wait.rs` | Block on **semantic state**: output quiesced, a regex matched, the process exited (with its code) |
| `AttachMode::Control` / `AttachMode::Observer` | `protocol.rs:36-40` | A reader (agent/monitor) that consumes the event stream **without taking control** |
| Exit-code taxonomy (`0/3/4/5/6`) | `cmd/wait.rs`, README | Outcomes a script/agent can branch on, not a single `0/1` |
| `PROTOCOL_VERSION` handshake | `protocol.rs:10,55-61` | A versioned wire boundary that can evolve under us |

Compare to the table-stakes tools: ttyd, gotty, Wetty, sshx, tmate stream a
**byte stream**. To know "did the build pass," a caller must scrape escape
sequences out of a raw pipe and guess. Remux can answer with
`peek --json` (a `TerminalSnapshot`) and `wait --for-regex 'PASS|FAIL'`. **Almost
no web-terminal tool exposes structured terminal STATE or wait/capture semantics
over an API.** That is the moat.

### 0.3 The thesis (encode this)

1. **The web terminal is one endpoint, not the headline.** xterm.js over a
   WebSocket is a thin *delivery mechanism* for a structured API. It is the last
   consumer we list, not the first.
2. **The differentiator is an agent-native structured API + fleet/discovery
   model.** The REST surface maps ~1:1 onto the existing `Request`s; the
   `--json` CLI is essentially this API already, just over a Unix socket instead
   of HTTP. We are *exposing* what we built, not inventing it.
3. **Structured state beats byte scraping for agents.** An AI agent driving a
   session wants `wait → peek JSON → branch on exit code`, not a regex over a
   VT100 byte soup. This is the workflow that wins multi-host AI coding tasks.
4. **The daemon stays narrow and local.** Network, auth, TLS, RBAC, audit, and
   browser transport live in a *separate* gateway/bridge process. The daemon's
   only job remains: own PTYs, parse VT state, serve the local socket.

### 0.4 Guiding principles

1. **Decouple the public API from the internal protocol.** The gateway is a
   *translator*. `protocol.rs` keeps evolving under `PROTOCOL_VERSION`; the
   public REST/WS contract is versioned independently (`/v1/...`). A wire-format
   break in the daemon must not break a published API.
2. **Structured first, stream second.** Every capability is exposed as
   structured data (JSON cells, typed events, exit codes) *first*; raw streaming
   is one mode among several, not the only one.
3. **Binary where binary belongs.** Terminal I/O is **not always UTF-8**.
   Interactive streaming uses **binary WebSocket frames**, never base64-in-JSON.
4. **Security is layered and external.** Daemon = local trust boundary. Gateway =
   network trust boundary. Never collapse them.
5. **Same runtime for humans and agents.** A human in xterm.js and an agent
   driving REST hit the *same sessions* via the *same daemon*. No separate
   "agent mode."

---

## 1. Workstream Overview

| WS | Title | Delivers | Depends on | Priority | Est. size |
| --- | --- | --- | --- | --- | --- |
| AW0 | API contract & decoupling layer | Versioned public DTOs + protocol↔DTO mapping in `remux-gateway` | runtime (done) | P0 | M |
| AW1 | SSH transport / `remux bridge` | Human multi-host UX (`--host`, `devbox:backend`) | AW0 (shares DTOs optionally) | P0 | L |
| AW2 | `remux-gateway` crate (REST control plane) ✅ **landed** | Sessions CRUD, input, capture-JSON, wait, scrollback over HTTPS | AW0 | P0 | L |
| AW3 | WebSocket interactive stream (binary framing) ✅ **landed** | Live terminal I/O + structured `/events` channel | AW2 | P1 | M |
| AW4 | Auth & TLS posture (v1: bearer + TLS) ✅ **landed (v1)** | Token auth, TLS termination, deny-by-default | AW2 | P0 (ships with AW2) | M |
| AW5 | Browser terminal (xterm.js consumer) ✅ **shipped (minimal built-in client)** | Reference web UI served by the gateway — **listed last, on purpose** | AW3, AW4 | P2 | M |
| AW6 | Fleet / discovery model | **v1 client-side discovery: done** (host registry + `fleet ls`/`hosts`/`attach` fan-out over SSH); **control plane: deferred** (federation, RBAC, intent routing, migration) | AW1, AW2 | P2 (v1 shipped; control plane later) | XL |

**Critical path:** AW0 → AW2 + AW4 (the structured API, secured) is the
differentiator and ships first. AW1 (SSH) is parallel and serves the human
multi-host story. AW3 adds interactive streaming. AW5 (browser) and AW6 (fleet)
follow. The browser terminal is deliberately **not** on the critical path.

---

## 2. AW0 — API Contract & Decoupling Layer

**Goal:** Establish a *public* API contract that is **independent of**
`remux-core::protocol`, plus the translation layer that maps it onto the existing
daemon `Request`/`Response`/`Event` types. This is the hinge of the whole plan:
it lets the internal protocol keep breaking under `PROTOCOL_VERSION` while the
published `/v1` API stays stable.

### 2.1 Reasoning

The REST surface maps ~1:1 onto existing requests — but "1:1" is a starting
point, not a coupling. If we serialize `remux_core::Request` straight to the wire
we have handcuffed the daemon: every `protocol.rs` change is a public breaking
change. Instead, define **public DTOs** in the gateway crate and a `From`/`Into`
mapping. The cost is a thin translation layer; the payoff is the freedom the
robustness work already relies on (`protocol.rs:7-10` exists precisely so the
wire can evolve).

### 2.2 Mapping table (public API → internal protocol)

| Public API (`/v1`) | Internal `Request` | Internal `Response`/`Event` |
| --- | --- | --- |
| `GET /v1/sessions` | `ListSessions` | `SessionList(Vec<SessionSummary>)` |
| `POST /v1/sessions` | `CreateSession(CreateSessionRequest)` | `Created(SessionDetails)` |
| `GET /v1/sessions/{id}` | `InspectSession` | `SessionDetails` |
| `DELETE /v1/sessions/{id}` | `KillSession { signal }` | `Ok` / `Error` |
| `PATCH /v1/sessions/{id}` (rename) | `RenameSession` | `Ok` |
| `POST /v1/sessions/{id}/input` | `SendInput` (fire-and-forget) | (none) |
| `GET /v1/sessions/{id}/screen` | `CaptureScreen` | `Screen(TerminalSnapshot)` |
| `GET /v1/sessions/{id}/scrollback` | `ReadScrollback { lines }` | `Scrollback(ScrollbackChunk)` |
| `POST /v1/sessions/{id}/wait` | `AttachSession{Observer}` + event loop | derived (`matched`/`idle`/`exited`/`timeout`) |
| `POST /v1/sessions/{id}/resize` | `ResizeSession` | `Ok` |
| `WS /v1/sessions/{id}/stream` | `AttachSession` + `Event::Output` / `SendInput` | binary frames |

Note that `wait` has **no single internal request**: it is a client-side
predicate over the observer event stream (exactly as `cmd/wait.rs` implements it
today). The gateway re-implements that loop server-side. This is the clearest
example of why the public API is *not* the protocol: a useful public verb can be
a composition of internal primitives.

### 2.3 Public DTOs (gateway-owned, versioned)

```rust
// crates/remux-gateway/src/api/v1/dto.rs
// Public, stable, JSON-shaped. Independent of remux_core::protocol.

#[derive(Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,            // uuid as string (stable JSON)
    pub name: String,
    pub status: String,        // "running" | "exited" | "starting" | "failed"
    pub command: Vec<String>,
    pub cwd: String,
    pub created_at: String,    // RFC3339
    pub pid: Option<u32>,
    pub attached_clients: usize,
    pub last_exit_code: Option<i32>,
}

#[derive(Deserialize)]
pub struct CreateSessionBody {
    pub name: Option<String>,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<[String; 2]>,
    #[serde(default = "default_size")]
    pub size: SizeBody,        // { cols, rows }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitBody {
    Idle { ms: u64 },
    Regex { pattern: String },
    Exit,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WaitResult {
    pub result: String,        // "matched" | "idle" | "exited" | "timeout"
    pub exit_code: Option<i32>,
}
```

The screen endpoint returns the existing `TerminalSnapshot` shape (already
`Serialize`, see `terminal.rs`) under a public `ScreenView` alias, so the
structured-cells contract is shared but still gateway-owned.

### 2.4 Tasks

- **T0.1** ✅ **Done.** Created the `remux-gateway` **library** crate in the
  workspace. Depends on `remux-core` (protocol + framing), `serde`/`serde_json`,
  `chrono`, `tokio`, `thiserror` (and `regex` for the `wait` predicate);
  `remux-testkit` as a dev-dependency. **No `axum`/`hyper`/`rustls`/`utoipa`/`tower`
  yet** — the HTTP/WS server is AW2; AW0 only establishes the contract + adapter,
  keeping dependencies lean.
- **T0.2** ✅ **Done.** `/v1` DTOs in `src/api/v1/dto.rs` and the
  `From<protocol::*> for dto::*` (and reverse for request bodies) mappings in
  `src/api/v1/convert.rs`. All `serde(rename)` / string-mapping decisions live in
  that layer; `protocol.rs` is untouched.
- **T0.3** ✅ **Done.** `DaemonConn` (`src/daemon_conn.rs`) opens the **local Unix
  socket**, performs the `Hello { version: PROTOCOL_VERSION }` handshake and
  **refuses** a version mismatch, and exposes typed `request()`/`subscribe()`
  helpers (plus a composed `wait()`) reusing
  `remux_core::framing::{read_message, write_message}`.
- **T0.4** ✅ **Done.** Public `ApiError` (`src/error.rs`) derived from
  `RemuxError` + the exit-code taxonomy, with `fn http_status(&self) -> u16`
  (404/403/504/503/400/502/500). Returns a bare `u16` — no `http`/axum dependency.
- **T0.5** ✅ **Done.** OpenAPI 3.1 generation (`utoipa`) lands with the axum
  handlers: the DTOs derive `ToSchema` (`src/api/v1/dto.rs`) and the handlers carry
  `#[utoipa::path(...)]` (`src/app.rs`), assembled into an `OpenApi` doc in
  `src/api/v1/openapi.rs` (with the `bearer` security scheme + the `ApiErrorBody`
  shape). Served at `GET /v1/openapi.json` (unauth, for discoverability) and
  committed serialized-to-YAML at `docs/openapi.yaml`. A drift test
  (`tests/openapi.rs`) regenerates the spec in-memory and asserts it equals the
  committed file; regenerate with `UPDATE_OPENAPI=1 cargo test -p remux-gateway
  --test openapi`.

### 2.5 Tests

- DTO roundtrip: every `protocol::*` ↔ `dto::*` conversion is lossless for the
  fields the public API promises; unit-tested both directions.
- A `PROTOCOL_VERSION` bump test: simulate a daemon advertising a different
  version in its `Response::Hello`; the gateway must refuse to start serving that
  daemon (clear error), **not** silently proxy a mismatched wire format.
- OpenAPI lint: the generated spec validates against the 3.1 schema in CI.

### 2.6 Definition of Done

- [x] `remux-gateway` crate exists, builds, links `remux-core`.
- [x] `/v1` DTOs are defined separately from `protocol.rs`, with tested mappings
      (DTO JSON roundtrips + `protocol <-> dto` conversion tests, both directions).
- [x] `DaemonConn` connects over the **Unix socket only** and handshakes; an
      integration test drives create → list → capture-screen → kill through the
      `/v1` DTO layer against a real daemon (`tests/daemon_conn_e2e.rs`).
- [x] OpenAPI `/v1` document generated and CI-validated. **Done** (see T0.5):
      generated from the axum handlers via `utoipa`, served at
      `GET /v1/openapi.json`, committed to `docs/openapi.yaml`, and a drift test
      (`tests/openapi.rs`) keeps the committed file in sync (asserts OpenAPI 3.1 +
      the presence of known paths).
- [x] A daemon `PROTOCOL_VERSION` mismatch is refused, not silently proxied
      (`check_protocol_version` is unit-tested directly with `PROTOCOL_VERSION + 1`).
      The public `/v1` DTOs are decoupled from `protocol.rs` so an internal wire
      bump requires zero DTO changes; the OpenAPI feature-flag proof lands with AW2.

---

## 3. AW1 — SSH Transport (`remux bridge` / `--host`)

> **Status update:** AW1 has since shipped — the hidden `remux bridge`
> subcommand, the generalized client transport (`RemuxClient` over any async
> stream), and the global `--host <ssh-target>` routing flag are implemented and
> tested (`crates/remux-cli/src/{client.rs,cmd/bridge.rs}`, `tests/bridge.rs`),
> with the daemon unchanged (still Unix-socket-only). The `host:session` selector
> sugar remains the one optional follow-up. The design below is retained as the
> rationale of record.

**Goal:** Deliver the *human* multi-host experience promised by `spec.md` §7
without any network listener: reach a remote `remuxd` over **SSH as transport**,
so `ssh + tmux + attach` collapses to one command. This is parallel to the
gateway and is the first remote feature precisely because it piggybacks existing
SSH policy, keys, and bastions.

### 3.1 Design

```text
remux --host devbox attach backend
   └─ local remux spawns:  ssh devbox remux bridge
        └─ remote `remux bridge` connects to the remote remuxd Unix socket
           and pipes the framed protocol over ssh stdin/stdout
   └─ local client speaks the normal protocol over that pipe
```

- **`remux bridge` (new, hidden subcommand):** runs on the *remote* host, opens
  the local `remuxd.sock`, performs the `Hello` handshake, and shuttles framed
  messages between its stdio and the socket. It is a dumb byte/​frame pump — no
  auth of its own (SSH already authenticated the user), no network listener.
- **`--host <host>` (global flag):** when present, the client's transport is
  `ssh <host> remux bridge` instead of a direct `UnixStream`. The rest of the CLI
  is unchanged — `attach`, `send`, `peek`, `wait`, `ls` all "just work" remotely
  because they only depend on the framed protocol, not on the transport.
- **`host:session` selector sugar:** `remux attach devbox:backend` parses to
  `--host devbox` + session `backend`, matching `spec.md` §7's
  `remux attach devbox:backend`.
- **Auto-bootstrap:** if `remux bridge` is missing on the remote, surface a clear
  error with the one-line install hint; optionally offer `remux bridge --spawn`
  to auto-spawn `remuxd` first (same double-fork path the local CLI uses).

### 3.2 Why SSH first

- **No exposed port.** Works inside enterprise networks, bastions, and existing
  SSH ACLs on day one (`spec.md` §7 "no exposed ports required").
- **Reuses the entire local protocol.** The bridge is transport-only; the
  controller/observer model, `send`/`peek`/`wait`, and faithful reattach all
  apply unchanged across the SSH pipe.
- **Security stays clean:** the daemon never listens on a network; SSH is the
  authenticated channel; the bridge runs as the SSH-authenticated user and can
  only reach that user's socket.

### 3.3 Relationship to the gateway

SSH transport and the gateway are **complementary, not alternatives**:

| | SSH bridge (AW1) | Gateway (AW2) |
| --- | --- | --- |
| Primary consumer | **Humans** with SSH access | **Agents / browsers / services** |
| Transport | SSH stdio pipe | HTTPS / WSS |
| Auth | SSH keys / bastion | Bearer token + TLS (AW4) |
| Protocol | internal framed protocol verbatim | public `/v1` DTOs |
| Listener? | none (SSH) | gateway process only |

A host can run both: humans `ssh`-bridge in; agents hit the gateway. Both reach
the same `remuxd` over the same local socket.

### 3.4 Tests

- Integration: start a daemon under the harness, run `remux bridge` against its
  socket with piped stdio, drive `ls`/`send`/`peek` through the pipe, assert
  parity with direct-socket results.
- Selector parsing: `devbox:backend` → host `devbox`, name `backend`; bare
  `backend` → local; `user@host:id` forms.
- Bridge handshake: a `PROTOCOL_VERSION` mismatch over the bridge fails loud.

### 3.5 Definition of Done

- [ ] `remux bridge` pumps the framed protocol between stdio and the local socket.
- [ ] `--host` and `host:session` route any subcommand over `ssh host remux bridge`.
- [ ] No network listener introduced anywhere; SSH is the only remote channel.
- [ ] Integration test drives a full create→send→wait→peek loop over the bridge.
- [ ] Clear errors for missing remote `remux`/`remuxd`.

---

## 4. AW2 — `remux-gateway`: Structured REST Control Plane

**Goal:** A thin **axum** service, co-located with a daemon, that connects
locally over the Unix socket and exposes the structured `/v1` API over HTTPS.
This is the differentiator made real: the agent-native control plane.

### 4.1 Architecture

```text
 ┌─────────┐  HTTPS/WSS   ┌──────────────┐  Unix socket  ┌────────┐  PTY  ┌─────┐
 │ Agent / │◄────────────►│ remux-gateway│◄─────────────►│ remuxd │──────►│ PTY │
 │ Browser │  /v1 + token │  (axum/TLS)  │  framed proto │        │       │ ... │
 └─────────┘              └──────────────┘               └────────┘       └─────┘
                          terminates TLS+auth,           local-only,
                          translates DTO↔protocol        no network
```

- Gateway runs **on the same host as the daemon**, as the same user (or a user
  with socket access). It is the *only* process with a network listener.
- It holds a small **pool of `DaemonConn`** (Unix socket connections) and
  multiplexes HTTP requests onto them; long-lived observer streams (WS, wait) get
  their own connection.
- It is **stateless** beyond the connection pool and auth config — restartable,
  horizontally fine behind a load balancer if pinned per-host.

### 4.2 REST endpoints (concrete)

#### List sessions

```http
GET /v1/sessions
Authorization: Bearer <token>
```
```json
200 OK
[
  { "id": "5f3c…", "name": "build", "status": "running",
    "command": ["cargo","build"], "cwd": "/home/mj/api",
    "created_at": "2026-06-13T18:02:11Z", "pid": 48213,
    "attached_clients": 0, "last_exit_code": null }
]
```

#### Create a session

```http
POST /v1/sessions
Authorization: Bearer <token>
Content-Type: application/json

{ "name": "build", "command": ["cargo","build"],
  "cwd": "/home/mj/api", "size": { "cols": 120, "rows": 40 } }
```
```json
201 Created
Location: /v1/sessions/5f3c…
{ "id": "5f3c…", "name": "build", "status": "running", … }
```

#### Send input (binary-safe, fire-and-forget)

```http
POST /v1/sessions/5f3c…/input
Authorization: Bearer <token>
Content-Type: application/json

{ "text": "cargo test\n" }
```
Body variants mirror `cmd/send.rs`'s `InputSource`: exactly one of
`text` (only `\n \t \r \\` interpreted), `bytes_hex` (e.g. `"1b5b41"`), or
`key` (`"Enter"`, `"Up"`, …). Raw binary may also be sent with
`Content-Type: application/octet-stream` and a raw body — the cleanest path,
avoiding any text encoding question. Returns `202 Accepted` (the daemon does not
ack `SendInput`).

#### Capture screen as JSON cells (the thing ttyd/sshx can't do)

```http
GET /v1/sessions/5f3c…/screen
Authorization: Bearer <token>
Accept: application/json
```
```json
200 OK
{
  "cols": 120, "rows": 40,
  "cursor_row": 7, "cursor_col": 12,
  "alternate_screen": false,
  "cells": [
    { "ch": "P", "fg": {"Rgb":[0,255,0]}, "bg": "Default",
      "bold": true, "dim": false, "italic": false,
      "underline": false, "reverse": false, "strikethrough": false },
    { "ch": "A", "fg": {"Indexed":2}, "bg": "Default", "bold": true,
      "dim": false, "italic": false, "underline": false,
      "reverse": false, "strikethrough": false }
  ]
}
```

This is `Response::Screen(TerminalSnapshot)` served straight through (the type is
already `Serialize`, `terminal.rs:29`). `Accept: text/plain` returns the flattened
text (via `snapshot_to_text`, already in `render_snapshot.rs`); `Accept:
text/x-ansi` returns SGR-colored text (`snapshot_to_ansi`). **This endpoint is
the moat**: a caller gets cursor position, per-cell color, and alt-screen state
as data — no byte scraping, no VT parsing on the client. A web terminal that only
streams bytes structurally *cannot* answer "what color is the cell at (7,12)?"

#### Wait on semantic state

```http
POST /v1/sessions/5f3c…/wait
Authorization: Bearer <token>
Content-Type: application/json

{ "kind": "regex", "pattern": "test result: (ok|FAILED)", "timeout_ms": 120000 }
```
```json
200 OK
{ "result": "matched", "exit_code": null }
```
Server-side re-implementation of `cmd/wait.rs`: the gateway attaches as an
`Observer`, runs the predicate over `Event::Output`, and returns the typed
outcome. `kind` ∈ `idle` | `regex` | `exit`. A timeout yields
`{ "result": "timeout", … }` with HTTP `200` (the wait itself succeeded at
determining a timeout) — or `408` if `?http_status=strict` is set, for callers
that prefer status-code branching.

#### Read scrollback

```http
GET /v1/sessions/5f3c…/scrollback?lines=200
Authorization: Bearer <token>
```
Returns the scrollback chunk; `Accept: application/octet-stream` for raw bytes,
`text/plain` for decoded lines.

### 4.3 Decoupling discipline

- Handlers take **DTOs**, call `DaemonConn`, map `protocol::*` → DTO, return JSON.
- No handler ever serializes a `remux_core::protocol` type directly to the HTTP
  body. The one tolerated exception is `TerminalSnapshot`, which is re-exported as
  the public `ScreenView` *contract* (documented, version-pinned in `/v1`).
- `/v2` can be added side-by-side later; `/v1` handlers keep their DTO mapping.

### 4.4 Tests

- Handler tests with a **mock `DaemonConn`** (no real daemon): assert each
  endpoint produces the right DTO from a canned `Response`.
- End-to-end with the harness daemon + a real gateway on a loopback TLS socket:
  create → input → screen (assert JSON cells) → wait → delete.
- Negative: unknown session → `404`; non-controller input → `403`; daemon down →
  `503` (see §5.4).

### 4.5 Definition of Done

- [x] `remux-gateway` serves `/v1` sessions CRUD, input, screen, scrollback, wait.
      **Done** — the axum router in `crates/remux-gateway/src/app.rs` serves
      `GET/POST /v1/sessions`, `GET/DELETE/PATCH /v1/sessions/{id}`, and the
      `/input`, `/screen`, `/scrollback`, `/resize`, `/wait` sub-routes. `{id}`
      resolves UUID-or-name via `selector::parse_selector` (mirrors the CLI).
- [x] Connects **only** over the local Unix socket; no path reaches the daemon by
      network. **Done** — every handler opens a per-request `DaemonConn` to the
      Unix socket; `remuxd` is untouched (still Unix-socket-only).
- [x] Every endpoint goes through the DTO layer (snapshot-as-`ScreenView` aside).
      **Done** — handlers return `SessionView`/`ScreenView`/`ScrollbackView`/
      `WaitResult` via the AW0 `convert` mappings.
- [x] Screen endpoint returns structured cells with color/cursor/alt-screen.
      **Done** — `GET /v1/sessions/{id}/screen` returns the `ScreenView`
      (transparent over `TerminalSnapshot`); the `http_e2e` test reconstructs
      rows from the JSON cell grid.
- [x] OpenAPI doc matches the implemented surface; CI checks drift. **Done** —
      `utoipa` generates the `/v1` spec from the handlers; `GET /v1/openapi.json`
      serves it; `docs/openapi.yaml` is committed and a drift test
      (`tests/openapi.rs`) fails (with a regenerate hint) if the handlers/DTOs
      and the committed spec diverge.

**Errors:** a consistent JSON body `{ "error": "...", "kind": "..." }` is
produced by `ApiErrorResponse` (`IntoResponse`) using `ApiError::http_status()`
(404/403/504/503/400/502/500). E2E-verified: unknown session → 404, missing/bad
token → 401.

---

## 5. AW3 — WebSocket Interactive Stream

**Goal:** Live, bidirectional terminal I/O over WebSocket for interactive
consumers — the one place where streaming bytes is the right model — plus a
structured screen-event channel distinct from raw streaming.

### 5.1 Why binary frames (not JSON, not base64)

Terminal output is a raw byte stream and **is not always valid UTF-8** (mouse
reports, binary-ish escape sequences, mid-multibyte chunk boundaries, programs
emitting arbitrary bytes). Two consequences dictate the design:

1. **Base64-in-JSON is wasteful and lossy-prone.** Wrapping every output chunk as
   `{"type":"output","data":"<base64>"}` inflates payload ~33%, forces a JSON
   parse on every keystroke-latency frame, and tempts a UTF-8 assumption that
   corrupts non-text bytes. `spec.md` §8's `{"type":"output","data":"..."}`
   sketch is illustrative, not the wire format we ship.
2. **The frame type belongs in one byte, the payload stays raw.** We use
   **binary WebSocket frames** with a **1-byte type tag** prefix, then raw bytes:

```text
 WS binary frame layout:
 ┌──────────┬─────────────────────────────┐
 │ type:u8  │ payload: raw bytes …         │
 └──────────┴─────────────────────────────┘

 Client → Server tags          Server → Client tags
   0x00  INPUT  (raw bytes)      0x80  OUTPUT (raw PTY bytes)
   0x01  RESIZE (cols:u16,rows)  0x81  EXITED (exit_code: i32 LE, or absent)
   0x02  PING                    0x82  SNAPSHOT (bincode TerminalSnapshot)
                                 0x83  CONTROL_LOST
                                 0x8f  ERROR (utf-8 message)
```

- `OUTPUT` carries `Event::Output { data }` bytes **verbatim** — zero re-encode,
  zero base64. xterm.js `term.write(uint8array)` consumes it directly.
- `INPUT` carries keystrokes/paste/mouse bytes verbatim into `SendInput`.
- `RESIZE` maps to `ResizeSession`; `SNAPSHOT` lets the server push a
  `StateSnapshot` resync (the resync mechanism the robustness plan's backpressure
  policy wants, WS5/T5.5) without polluting the byte stream.
- **Control JSON frames** (auth, subscribe options) use WebSocket **text**
  frames, so the control plane stays JSON-debuggable while the hot path stays
  binary. The type tag's high bit distinguishes server→client from client→server.

### 5.2 Endpoints

```text
WSS /v1/sessions/{id}/stream      # interactive: INPUT/OUTPUT/RESIZE (binary)
WSS /v1/sessions/{id}/events      # structured: SNAPSHOT/EXITED/UPDATED (JSON text)
```

The split is deliberate and is itself a differentiator:

- `/stream` is the byte pipe an interactive terminal (human or pixel-faithful
  agent) attaches to. It maps to `AttachMode::Control` (or `Observer` with
  `?mode=observer`, exposing the read-only model over the network).
- `/events` is the **structured** channel: a consumer subscribes and receives
  typed JSON events (`SessionUpdated`, `SessionExited { exit_code }`, periodic
  `StateSnapshot` as `ScreenView`) — letting an agent *watch semantic state*
  without parsing the byte stream at all. ttyd/sshx have no equivalent.

### 5.3 Flow

```text
1. Client opens WSS /v1/sessions/{id}/stream  (Authorization via header or
   first text frame {"type":"auth","token":"…"} for browsers that can't set headers)
2. Gateway authenticates (AW4), opens a DaemonConn, sends AttachSession{Control}.
3. Gateway forwards bootstrap as a SNAPSHOT (0x82) frame (faithful repaint).
4. Loop: Event::Output → OUTPUT frame;  client INPUT frame → SendInput;
         client RESIZE → ResizeSession;  Event::SessionExited → EXITED + close.
5. On backpressure/lag: gateway coalesces and sends a fresh SNAPSHOT instead of a
   backlog of OUTPUT frames (reuses VT state — the robustness-plan resync idea).
```

### 5.4 Error & status taxonomy (REST + WS)

Map `RemuxError` / exit codes uniformly across HTTP and WS `ERROR` frames:

| Condition | Internal | HTTP | WS |
| --- | --- | --- | --- |
| Session not found | `SessionNotFound` (exit 3) | `404` | `0x8f` "not_found" + close 4404 |
| Not controlling client | denied (exit 5) | `403` | close 4403 |
| Daemon unreachable | `ConnectionFailed` (exit 6) | `503` | close 4503 |
| Wait timeout | (exit 4) | `200`/`408` | n/a |
| Bad request / regex / hex | `InvalidRequest` | `400` | `0x8f` + close 4400 |
| Auth missing/invalid | — (AW4) | `401` | close 4401 |

### 5.5 Tests

- Binary framing roundtrip: encode/decode each tag; assert raw bytes survive,
  including a non-UTF-8 `OUTPUT` payload (e.g. `0xff 0xfe`) and a chunk split
  mid-multibyte — proving we never base64/UTF-8-assume.
- WS integration: attach to a session running a known program, type via INPUT,
  assert OUTPUT contains the echo; resize and assert the daemon resized.
- Resync: throttle the client, flood output, assert a SNAPSHOT arrives and the
  reconstructed screen is correct (not a corrupted backlog).
- Observer mode: `?mode=observer` cannot send INPUT (gateway rejects the frame).

### 5.6 Definition of Done

- [x] `/stream` uses **binary** frames; OUTPUT (PTY) bytes are forwarded
      **verbatim**. **Done** (`crates/remux-gateway/src/ws.rs`). v1 simplification:
      server→client output and client→server input are raw binary frames (no
      1-byte tag); the type tag is reserved for a later iteration. Control vs raw
      input is distinguished by **frame type** instead: a **text** frame carrying
      `{"type":"resize","cols":N,"rows":N}` is the resize control message, raw
      input is a **binary** frame. `/stream` Control-attaches to the daemon so
      input and resize are accepted (`DaemonConn::subscribe_control`).
- [x] `/events` delivers typed JSON structured events. **Done** — `exited`,
      `updated`, `terminating`, `control_lost`, `snapshot` (the `ScreenView`), and
      `error`, from an Observer subscription. WSS-e2e-verified (`exited`).
- [x] No base64 anywhere on the I/O hot path; OUTPUT bytes are sent as binary
      frames untouched (non-UTF-8 safe by construction). **Done.**
- [x] Observer/Control modes both reachable; control rules enforced at the
      gateway/daemon. **Done** — `/stream` = Control, `/events` = Observer.
- [ ] Lag triggers a snapshot resync rather than corruption. — **Deferred** (the
      backpressure/resync policy is a later iteration; v1 forwards output frames
      directly).

**Auth on WS:** the upgrade is gated by the same bearer middleware, which accepts
the token via the `Authorization` header **or** `?token=<token>` (browsers can't
set headers on a WS handshake). The `ws_e2e` test connects over `wss://` with the
token in the query string.

---

## 6. AW4 — Auth & Security Posture

**Goal:** A safe, minimal v1 auth story that ships *with* the gateway, leaving
the daemon's local-only invariant untouched and deferring the heavy
identity/RBAC machinery to the fleet phase.

### 6.1 The invariant (restate, because it's the whole point)

`remuxd` **never** gets a network listener. It binds a per-user Unix socket with
restrictive permissions and serves the same user only (`spec.md` §11 "Local: Unix
socket permissions"; README architecture). Every network concern — TLS, auth,
cert rotation, user mapping, audit — lives in the gateway or the SSH layer. This
is the architectural rule that keeps the daemon trustworthy and is why we refuse
to "just open a port on remuxd."

### 6.2 v1: static bearer token + TLS

- **TLS termination** at the gateway (`rustls` via `axum-server`). HTTP is refused
  unless explicitly `--insecure` for loopback dev. HSTS on.
- **Static bearer token(s):** the gateway loads one or more tokens (hashed) from
  config / a secrets file / env. Every `/v1` request and WS upgrade requires
  `Authorization: Bearer <token>` (or, for browser WS that can't set headers, an
  auth control frame immediately after open, then a short handshake deadline).
- **Deny by default:** no token configured ⇒ gateway refuses to start a network
  listener (fails closed). No anonymous access, ever.
- **Per-token scope (coarse):** v1 supports at most `read` vs `read+write` per
  token (read = list/inspect/screen/scrollback/observer-stream; write = create/
  input/resize/kill/control-stream). Enough to hand an agent a read-only token.
- **Localhost-bind default:** the gateway binds `127.0.0.1` by default; exposing
  it on `0.0.0.0` is an explicit, documented opt-in (forces the operator to make
  a conscious network-exposure decision).
- **Audit log:** structured `tracing` line per request (token id hash, method,
  session, outcome) — the seed of the fleet-phase audit trail.

### 6.3 Auth hardening Phase A — principal + RBAC ✅ **SHIPPED**

The coarse read/read-write scope split has been replaced by a **principal +
RBAC** model shared by the gateway AND the control plane, in the new
`crates/remux-authz` crate (pure, no network, unit-tested). This is Phase A of
auth hardening; Phases B (OIDC/JWT) and C (mTLS + cert pinning) plug INTO this
model.

- **Fine-grained permissions** spanning both surfaces, each with a stable string
  name (`"session.read"`, `"fleet.resolve"`, `"host.register"`, …):
  gateway — `session.list/read/create/input/resize/kill/rename/stream/wait`,
  `events.read`; control plane — `fleet.hosts.read`, `fleet.sessions.read`,
  `fleet.resolve`, `host.register`.
- **Roles** = named permission sets; a **Policy** maps role names → roles.
  Built-in roles: gateway `viewer ⊂ operator ⊂ admin`; control plane
  `registrar`, `fleet-viewer ⊂ fleet-operator ⊂ fleet-admin`.
- **Principals** (`{subject, roles}`) come from a constant-time `TokenStore`
  (bearer token → principal). `permits(policy, principal, perm)` unions the
  principal's known roles' permissions; an unknown role grants nothing (logged,
  deny-by-default).
- **Back-compat:** the gateway's `--token` → principal `{admin, [admin]}` and
  `--read-token` → `{reader, [viewer]}`; the control plane's `--token` →
  `{fleet-admin, [fleet-admin]}` and `--register-token` → `{registrar,
  [registrar]}`. An optional `--auth-config <FILE>` (env
  `REMUX_GATEWAY_AUTH_CONFIG` / `REMUX_CP_AUTH_CONFIG`) adds principal-shaped
  tokens and custom roles.
- **401 vs 403 preserved:** unknown/missing token → `401`; a known principal
  lacking the route's permission → `403`. Audit lines now carry the principal's
  `subject` + `roles` alongside the hashed `token_id` (never the raw token).

### 6.3a Auth hardening Phase B — OIDC/JWT bearer ✅ **SHIPPED**

A JWT (e.g. from an OIDC provider) can now be used as the bearer credential. It
is validated and its claims are mapped to a `remux_authz::Principal`, so a
JWT-authenticated caller flows through the **exact same** RBAC enforcement as a
static token. Static tokens keep working unchanged.

- **`remux_authz::jwt`** — a pure, offline-testable `JwtValidator` built on the
  `jsonwebtoken` crate (the `aws_lc_rs` backend, sharing rustls' provider — no
  RustCrypto stack pulled in). `JwtConfig { issuer?, audience?, roles_claim
  (default "roles"), subject_claim (default "sub"), key }` where `JwtKey` is one
  of `Hs256(secret)`, `Rs256(pem)` / `Es256(pem)` (static public key), or
  `Jwks(set)` (a parsed `kid → key` map). `validate()` verifies the signature +
  `exp` (+ `iss`/`aud` when configured, leeway 0) and builds `Principal { subject
  ← subject_claim, roles ← roles_claim }`. The **roles claim accepts either** a
  JSON array of strings **or** a space-delimited string (OIDC `scope` style).
  Unknown/extra claims are ignored; any failure (expired, wrong iss/aud, bad
  signature, missing subject) is a clear `JwtError` the caller maps to `401`.
  `remux-authz` only *consumes* parsed keys — `parse_jwks()` turns a JWKS JSON
  document into the `Jwks` set; **no HTTP client** is pulled into the pure crate.
- **Service wiring** (gateway + control plane, shared in
  `remux_gateway::jwt_service`): auth resolution is **static-then-JWT** — the
  presented bearer is tried against the constant-time `TokenStore` FIRST; only on
  a miss, and only if JWT is configured, is it validated as a JWT. Whichever
  yields a `Principal` proceeds to the identical `permits()` check (a valid JWT
  whose roles lack the route permission → `403`, same as static). Flags/env on
  both services: `--jwt-hs256-secret`, `--jwt-public-key <PEM>` (static
  RS256/ES256, offline-friendly), `--jwt-jwks-url <URL>` (fetched over HTTPS with
  the existing `reqwest`, system roots, cached in-memory and refreshed on a TTL;
  on a refresh failure the last good JWKS keeps serving, logged), plus
  `--jwt-issuer`, `--jwt-audience`, `--jwt-roles-claim`. With **no** JWT flag set,
  behavior is exactly as before (static tokens only). The audit line now records
  the auth method (`static` vs `jwt`) alongside subject/roles (never the token).
- **Tested:** `remux-authz` JWT unit tests (HS256 array + space-delimited scope,
  RS256 with a generated keypair, JWKS-by-`kid`, expired / wrong-iss / wrong-aud /
  tampered-signature / missing-subject errors, JWKS parsing); `jwt_service` unit
  tests (settings → validator, HS256 end-to-end, mutually-exclusive key sources);
  gateway `tests/jwt_e2e.rs` (operator JWT read+write, viewer JWT 200-read /
  403-write, expired+garbage → 401, static token still works); control-plane
  `tests/jwt_e2e.rs` (a `fleet-viewer` JWT lists hosts but is `403` on resolve, a
  `fleet-admin` JWT clears the resolve gate, static admin token unaffected).

### 6.3b Auth hardening Phase C — mTLS + gateway-cert pinning ✅ **SHIPPED**

Auth hardening is now complete: **RBAC + JWT/OIDC + mTLS + cert pinning**. Phase C
adds two things, both *additive* — each is just another way to produce a
`remux_authz::Principal` (or to verify a peer's cert), and the `Policy`/`permits`
decision and the audit shape are unchanged.

**(1) Gateway-cert pinning / CA trust — secure by default.** The two outbound
client links — control plane → gateway (`GatewayClient`) and gateway → control
plane (`--register`) — no longer trust self-signed certs blindly. The shared
`remux_gateway::peer_tls` helper exposes a `PeerVerification` with four modes,
resolved from flags in priority order:
- `--gateway-ca <PEM>` / `--register-ca <PEM>` — trust a PEM **CA bundle** for the
  peer (the peer's own self-signed cert may be used here as a CA root);
- `--gateway-pin <SHA256>` / `--register-pin <SHA256>` (**repeatable**) — accept
  ONLY a leaf cert whose **SHA-256 fingerprint** matches (no CA needed; ideal for
  self-signed peers), via a custom `rustls` `ServerCertVerifier` plugged into
  reqwest with `use_preconfigured_tls`;
- *(default)* **system roots** — verify against the OS/webpki store; a self-signed
  peer fails the handshake with a clear, operator-actionable error;
- `--gateway-tls-insecure` / `--register-tls-insecure` — now **default `false`**;
  an explicit, loudly-logged **dev-only** opt-out (accept any cert).

  The insecure-by-default wart is gone. A wrong pin / CA mismatch surfaces as a TLS
  error (CP fan-out row `ok:false`; register loop retries) — never a panic.

**(2) mTLS client-certificate authentication (gateway + control plane).**
`--client-ca <PEM>` enables mTLS: the rustls server is built with a
`WebPkiClientVerifier` against that CA and requests + verifies client certs.
`--mtls-mode require|optional` (default `optional`): `require` makes a valid client
cert mandatory (the handshake refuses connections without one); `optional` uses a
valid cert if presented, else falls back to the existing bearer (token/JWT)
resolution. A custom `axum-server` acceptor (`remux_gateway::mtls::MtlsAcceptor`,
reused by both services) completes the handshake, reads `peer_certificates()`,
extracts the leaf's **CN (or first SAN)** (`x509-parser`), and maps it — via the
**pure** `remux_authz::MtlsIdentities` helper — to a `Principal`, stashed as an
`Option<MtlsPrincipal>` request extension. Roles come from `--mtls-identities
<TOML>` (`[[identities]] subject="…" roles=[…]`); an unmapped-but-valid cert gets
`--mtls-default-roles <r1,r2>` (default **none** → it authenticates but is `403` on
every route until an operator maps it). **Precedence:** a verified client cert is
the authenticated principal — **cert identity WINS** over a bearer presented in the
same request — and it enforces the **SAME** per-route `Permission`s via `permits()`.
`/health` + `/openapi.json` stay public. The audit line records
`auth_method = mtls` with the cert subject + roles.

- **Tested** (rcgen-generated PKI): gateway `tests/mtls_e2e.rs` — operator cert
  read+write, viewer cert 200-read / 403-write, cert-wins-over-bearer precedence,
  unmapped valid cert → 403, `optional` no-cert → bearer still works (and no-auth →
  401), `require` no-cert → connection refused (handshake) + valid cert works;
  control-plane `tests/mtls_e2e.rs` — fleet-admin cert lists+resolves, fleet-viewer
  cert 403 on resolve, `require` no-cert refused. Pinning: control-plane
  `tests/pinning_e2e.rs` (CP pins the gateway's real self-signed leaf → federation
  works; wrong pin → `ok:false` TLS error, no panic; gateway-cert-as-CA works) and
  gateway `tests/register_pinning_e2e.rs` (auto-register with the correct CP pin
  succeeds; a wrong pin fails closed without crashing the gateway; CP-cert-as-CA
  works). Plus `remux_authz::mtls` + `peer_tls` unit tests.

### 6.4 Deferred beyond Phase C

Auth hardening (RBAC + JWT/OIDC + mTLS + pinning) is complete. Still **deferred**
as future work: per-org/team multi-tenant policy, short-lived/rotating credentials
(SPIFFE-style cert rotation), client-cert **revocation (CRL/OCSP)**, and full
external audit pipelines. They remain *additive* over the shipped model.
(`spec.md` §11 "Fleet: RBAC, audit logs".)

### 6.4 Tests

- No-token config ⇒ gateway refuses to bind (fail-closed) — asserted.
- Missing/invalid bearer ⇒ `401`; read-token doing a write ⇒ `403`.
- TLS required: plain HTTP to a TLS listener is rejected; `--insecure` only works
  on loopback.
- Audit line emitted with hashed token id, never the raw token.

### 6.5 Definition of Done

- [x] TLS-terminating gateway; **TLS is always on** (rustls via `axum-server`).
      **Done** (`crates/remux-gateway/src/tls.rs`, `src/server.rs`). Operator
      PEM via `--tls-cert`/`--tls-key`, else a self-signed cert is generated for
      `127.0.0.1`/`localhost` at startup (fingerprint logged). There is no
      plaintext listener. (No `--insecure` path was added; HTTP is simply not
      served.)
- [x] Static bearer token; deny-by-default. **Done** (`src/auth.rs`,
      `src/app.rs` scope middleware). Every `/v1/*` route except `GET /v1/health`
      and `GET /v1/openapi.json` requires `Authorization: Bearer <token>` (WS also
      accepts `?token=`), with a **constant-time** compare. Missing/wrong → `401`
      JSON.
- [x] **Read vs read-write token scopes** (plan §6.2). **Done.** The read-write
      token comes from `--token`/`REMUX_GATEWAY_TOKEN`; an optional read-only token
      from `--read-token`/`REMUX_GATEWAY_READ_TOKEN`. A token→[`Scope`] map
      (`ReadOnly` | `ReadWrite`) is resolved in constant time per presented token.
      Read scope may call the safe endpoints (list/inspect/screen/scrollback/wait
      and the `/events` WS); write scope is required for everything that mutates or
      injects (create/delete/rename/input/resize and the `/stream` WS). A read-only
      token on a write route → **403** (distinct from the 401 for an unknown token),
      enforced **before** the WS upgrade. Deny-by-default throughout. Tested in
      `tests/scopes_e2e.rs`.
- [x] Daemon still Unix-socket-only; no code path adds a daemon network listener.
      **Done** — only `remux-gateway` was touched; `remuxd` is unchanged.
- [x] **Per-request audit logging.** **Done** (`src/app.rs::audit_layer`). A
      structured `tracing` line (target `remux_gateway::audit`) is emitted for
      every `/v1` request with: method, the matched route path (with `{id}`
      placeholders, not the concrete id), HTTP status, resolved scope, the
      non-reversible `token_id` hash (never the raw token), client remote address,
      and latency in ms. WS connect/disconnect are covered by the same layer (the
      upgrade request is a `/v1` request). No secret material is logged; asserted
      in `tests/audit_e2e.rs`.
- [x] **Fine-grained RBAC / principal-scoped tokens — SHIPPED** (Phase A, §6.3).
      The coarse read/read-write scope split has been replaced by the shared
      `crates/remux-authz` principal + RBAC model across BOTH the gateway and the
      control plane: fine-grained `Permission`s, built-in `viewer`/`operator`/
      `admin` (gateway) and `registrar`/`fleet-viewer`/`fleet-operator`/
      `fleet-admin` (control plane) roles, a `Policy`, `Principal`s, and a
      constant-time `TokenStore`, with an optional `--auth-config` file for
      principal-shaped tokens + custom roles. Back-compat token flags preserved;
      401-vs-403 semantics preserved; audit lines extended with subject + roles.
      Tested in `remux-authz` unit tests, `tests/scopes_e2e.rs`, and
      `tests/federation_e2e.rs`.
- [x] **OIDC/JWT (Phase B) — SHIPPED** (§6.3a): a JWT bearer is validated by the
      pure `remux_authz::jwt::JwtValidator` (HS256 / static RS256-ES256 PEM /
      JWKS) and its claims (subject + array-or-scope roles) map to a `Principal`;
      the services try the static `TokenStore` first, then JWT, with the identical
      `permits()` decision and 401/403 semantics. Tested in `remux-authz` unit
      tests, `jwt_service` unit tests, and the gateway + control-plane
      `tests/jwt_e2e.rs`.
- [x] **mTLS + gateway-cert pinning (Phase C) — SHIPPED** (§6.3b). Gateway-cert
      pinning / CA trust made the two outbound links **secure by default**
      (`--gateway-ca`/`--gateway-pin`, `--register-ca`/`--register-pin`;
      `*-tls-insecure` now defaults `false`, a dev-only opt-out), via
      `remux_gateway::peer_tls` (custom rustls `ServerCertVerifier` for pins).
      mTLS client-cert auth (`--client-ca`, `--mtls-mode require|optional`,
      `--mtls-identities`, `--mtls-default-roles`) extracts the peer leaf's CN/SAN
      and maps it to a `Principal` via the pure `remux_authz::MtlsIdentities`; the
      cert identity **wins over a bearer** and enforces the SAME per-route
      permissions. Tested in `remux_authz`/`peer_tls` unit tests, gateway + CP
      `tests/mtls_e2e.rs`, and `tests/pinning_e2e.rs` / `tests/register_pinning_e2e.rs`
      (rcgen PKI). Auth hardening (RBAC + JWT/OIDC + mTLS + pinning) is complete.

---

## 7. AW5 — Browser Terminal (xterm.js) — *Listed Last, On Purpose*

> **Status update:** AW5 has **shipped** as a deliberately minimal, self-contained
> client **served by the gateway binary itself** — no separate Node/Next.js build,
> no JS toolchain. The static assets (`crates/remux-gateway/web/{index.html,app.js,
> style.css}`) are embedded with `include_str!` (zero new deps) and served on
> routes **outside** the `/v1` bearer-auth group (`GET /`, `/app.js`, `/style.css`);
> the user supplies the token in-page (or via `?token=`). It lists sessions from
> `GET /v1/sessions` and attaches one session at a time over the binary `/stream`
> WebSocket (output bytes → `term.write(Uint8Array)`; input → binary frames;
> resize → a `{"type":"resize",…}` **text** frame), using xterm.js + the fit addon
> from a CDN. `--no-web-ui` disables it (`GET /` → `404`). The "rich web app /
> collab / recording" surface below stays **out of scope** — this is the thin
> reference consumer, on purpose. Tests: `crates/remux-gateway/tests/web_ui_e2e.rs`
> (asserts the gateway serves the assets with correct status/content-type/markers
> and that `--no-web-ui` 404s; in-browser rendering is not automatable here).
> Follow-up: vendoring xterm.js for offline/air-gapped use.

**Goal:** A reference web UI that consumes the API. It is intentionally the
**last** consumer, because it is table stakes (§0.1) and exists to *demonstrate*
the API, not to *be* the product.

### 7.1 Design

- Static SPA (xterm.js + a small TS client) served by the gateway or any static
  host. Talks **only** to the public `/v1` API:
  - `GET /v1/sessions` → session browser list (with `pid`, `status`, `cwd`).
  - `WSS /v1/sessions/{id}/stream` → `term.write(bytes)` from OUTPUT frames; key
    events → INPUT frames. Binary frames feed xterm.js directly — no base64.
  - `WSS …/events` → live status (exit badges, rename) without scraping bytes.
- Auth: bearer token entered once (or injected by the embedding app); sent as the
  WS auth control frame.
- **What we deliberately do NOT chase here:** multiplayer cursors, collab editing,
  recording/replay studios, themeable status-bar marketplaces. Those are where
  sshx/code-server/Coder differentiate; matching them is the §0.1 trap. The web
  terminal is a thin client over the real differentiator.

### 7.2 Tests

- Smoke (Playwright): open the SPA against a loopback gateway, attach to a
  session, type a command, assert the echoed bytes render. Resize the window,
  assert a RESIZE frame is sent.

### 7.3 Definition of Done

- [x] xterm.js client attaches via binary WS, renders OUTPUT bytes verbatim.
      **Done** — `web/app.js` writes binary frames as `term.write(new
      Uint8Array(buf))` (non-UTF-8 safe), sends input as binary frames, and resize
      as a `{"type":"resize",…}` text frame; served by the gateway over TLS.
- [x] Session list comes from `/v1/sessions`; one session at a time (v1).
      **Done** — a sidebar fetches `GET /v1/sessions` with the in-page bearer token
      and reconnects the `/stream` socket on selection. (Live `/events` status
      badges are a possible later polish; not needed for the table-stakes client.)
- [x] No feature creep into collab/recording; it stays a reference consumer.
      **Done** — single-session, no recording/collab; served by the gateway with a
      `--no-web-ui` off switch.

---

## 8. AW6 — Fleet / Discovery Model (Design-Level, the Longer-Term Moat)

> **Status update:** the **client-side first slice has SHIPPED.** A static host
> registry (`[[fleet.hosts]]` in config — `name`, `ssh`, `labels`) plus the
> `remux fleet` command (alias `f`) deliver multi-host **discovery** today:
> `fleet hosts` lists the registry; `fleet ls [--json] [--label k=v]…` fans out
> **concurrently** over the existing SSH transport (`ssh <host> remux bridge`),
> lists each host's sessions, and aggregates them tagged by host; `fleet attach
> <host>:<session>` resolves a registry name to its ssh target and reuses the
> remote attach path. Unreachable hosts are reported per-host (a row/JSON entry
> marked `unreachable`/`"ok": false`) and **never abort** the whole command. The
> connect+list step goes through an **injectable connector** (`gather_sessions`
> takes a `Fn(&FleetHost) -> Command`) so tests substitute a local
> `remux bridge --socket …` for real `ssh`; the aggregation/row/JSON building is
> a set of **pure** functions (`build_rows`/`build_json`). This is purely a
> **client** feature: no control-plane service, no gateway/daemon changes, no
> RBAC — `remuxd` stays Unix-socket-only. See
> `crates/remux-core/src/config.rs` (`FleetConfig`/`FleetHost`),
> `crates/remux-cli/src/cmd/fleet.rs`, and `crates/remux-cli/tests/fleet.rs`.
> **Control-plane CORE update (SHIPPED):** the federated **control-plane
> service** is now built as `crates/remux-control-plane` (binary
> `remux-control-plane`), a TLS axum service that federates over gateways:
> - **Outbound host registry** (in-memory `Arc<RwLock<HashMap<String,
>   HostEntry>>`). Gateways register **themselves** (outbound) via
>   `POST /cp/v1/register` (register-token, idempotent upsert by name);
>   `POST /cp/v1/heartbeat` refreshes `last_seen`; `DELETE /cp/v1/hosts/{name}`
>   deregisters; `GET /cp/v1/hosts` (admin-token) lists `{name,url,labels,
>   last_seen,healthy}` with `healthy = now-last_seen < ttl`. The daemon keeps
>   its no-inbound-listener invariant.
> - **Federated fleet API** (admin-token): `GET /cp/v1/sessions[?label=k=v]…`
>   does a server-side **concurrent fan-out** (`tokio::task::JoinSet`) of
>   `GET /v1/sessions` to all healthy hosts matching ALL labels, tagging each
>   session with its host; unreachable/erroring hosts are reported per-host
>   (`ok:false` + `error`), never fatal.
> - **Intent routing v1**: `POST /cp/v1/resolve {labels, command?, reuse_name?}`
>   picks the first healthy host matching all labels (deterministic, by name),
>   reuses a same-named session if present, else creates one via the gateway's
>   `POST /v1/sessions`, returning `{host, gateway_url, session_id, name,
>   created}`.
> - **`GatewayClient`** (reqwest) wraps a gateway base URL + bearer token with
>   bounded per-gateway timeouts. v1 trusts **self-signed** gateway certs
>   (`--gateway-tls-insecure`, default `true`, logged as a warning) — gateway
>   cert **pinning / CA trust** is the hardening follow-up.
> - **Bootstrap/security**: TLS always on (self-signed for loopback or operator
>   PEM); deny-by-default bearer auth with constant-time compare and **two**
>   token kinds (admin token for the fleet API, register token for the
>   registration surface); per-request audit logging (method, path, status,
>   token-kind, peer, latency — never raw tokens). Proven by a multi-service
>   TLS e2e test (two daemons + two in-process gateways + the control plane:
>   `crates/remux-control-plane/tests/federation_e2e.rs`).
>
> **Self-running federation update (SHIPPED):** two pieces that make the
> federation self-assembling and usable have now landed:
> - **Gateway auto-registration** — `remux-gateway --register <CP_URL>` has the
>   gateway register **itself** outbound on startup (`POST /cp/v1/register` with
>   its own read-write bearer as the call-back token), then heartbeat every
>   `ttl/2` (`--register-ttl`, default 30), and best-effort deregister
>   (`DELETE /cp/v1/hosts/{name}`) on SIGTERM/SIGINT graceful shutdown. Flags:
>   `--register-token` (env `REMUX_GATEWAY_REGISTER_TOKEN`), `--advertise-url`
>   (default `https://<--listen>`), `--register-name` (default hostname),
>   repeatable `--label k=v`, `--register-tls-insecure` (default `true`, trusts
>   the CP's self-signed cert, logged as a warning). Registration failures are
>   **never fatal** — logged and retried with bounded backoff while the `/v1` API
>   keeps serving (`crates/remux-gateway/src/register.rs`; proven by
>   `crates/remux-gateway/tests/register_e2e.rs`: a real daemon + real gateway
>   auto-register into an in-process control plane and become healthy + reachable
>   via fan-out).
> - **`remux open` CLI (intent routing)** — `remux open` (alias `o`) drives
>   `POST /cp/v1/resolve { labels, command?, reuse_name? }`, then **routes the
>   caller**: if the resolved `host` is in the local `[[fleet.hosts]]` registry it
>   attaches over SSH to the resolved session (reusing the `--host`/`fleet attach`
>   remote-attach path); otherwise it prints the resolved target (human or
>   `--json`) with a hint, exit 0. This is the elegant split — the **control
>   plane** owns *intent → host/session*, the **local fleet registry** owns *host
>   → SSH reachability*. `--control-plane`/`--token` fall back to a new
>   `[control_plane]` config section (`remux_core::ControlPlaneConfig`), then
>   `REMUX_CP_URL`/`REMUX_CP_TOKEN`. The resolve + target-decision + formatting
>   are **pure** functions (`resolve_endpoint`/`decide_target`/`format_target`),
>   unit-tested for created/reuse × in-fleet/not-in-fleet
>   (`crates/remux-cli/src/cmd/open.rs`).
>
> **Still DEFERRED (the explicit NEXT steps):** **RBAC / OIDC / mTLS** and
> principal-scoped tokens fleet-wide, **gateway-cert pinning / CA trust** (v1
> still trusts self-signed certs via `--gateway-tls-insecure` /
> `--register-tls-insecure`), a **cached fleet index** (fan-out is live per
> request), and **cross-host session migration** + agent-ownership arbitration.
> §8.4 below is updated accordingly.

**Goal:** Sketch the layer that turns "one daemon, one host" into "one pane for
humans **and** agents across a fleet." This is `spec.md` §10. We **design** it
now to keep AW0–AW4 forward-compatible; we **build** it later. The v1
client-side discovery slice (host registry + SSH fan-out) is now built (see the
status note above); the control-plane build-out remains design-level.

### 8.1 Concepts (`spec.md` §10: Host, Session, Workspace, Agent)

- **Host registry:** a control-plane service tracking hosts, each running a
  gateway. Hosts register with labels (`project=api`, `env=dev`, `region=…`),
  capabilities, and health. Registration is gateway→control-plane (outbound),
  preserving the "no inbound listener on the daemon" rule.
- **Session discovery across hosts:** `GET /fleet/v1/sessions` fans out
  `GET /v1/sessions` across registered gateways (or reads a cached index),
  returning sessions tagged with their host. One query, whole fleet.
- **Intent-based routing** (`spec.md` §10 example):

  ```bash
  remux open --project api --env dev
  ```
  The control plane resolves intent → host → existing-or-new session: pick a host
  matching the labels, reuse a live session if one exists (and isn't owned by an
  agent that shouldn't be interrupted), else create one, then route the caller
  (human → SSH bridge or gateway WS; agent → gateway REST).

### 8.2 "One pane for humans AND agents"

The same fleet index backs both the human TUI/web browser *and* the agent API.
An agent enumerates sessions across hosts and drives them via `/v1`; a human sees
the identical set in `remux ui` / the web list. There is no separate agent
inventory — the differentiator (structured state) composes naturally into a fleet
view because every session is already introspectable as data.

### 8.3 Forward-compatibility constraints on earlier workstreams

- Gateways must be **independently addressable and self-describing** (AW2/AW4
  already give each a `/v1` + token); the control plane is "just" a federation
  over them.
- Tokens (AW4) must be **principal-shaped** so they upgrade to RBAC subjects
  without a redesign.
- The `/v1` contract (AW0) must be **stable**, since the control plane calls it
  fleet-wide; this is exactly why AW0 decouples the public API from `protocol.rs`.

### 8.4 Explicitly deferred

The **client-side discovery slice** shipped first (static host registry,
`fleet hosts`/`fleet ls`/`fleet attach`, concurrent SSH fan-out), and the
**control-plane CORE** has now shipped too (see the §8 status note):
`remux-control-plane` with the **outbound** host-registry service
(register/heartbeat/deregister/list, health-tracked by TTL), the **federated
fleet API** (`GET /cp/v1/sessions` concurrent gateway fan-out + label filtering +
per-host error isolation), and **intent routing v1** (`POST /cp/v1/resolve`).
**Gateway auto-registration** (`remux-gateway --register`) and the **`remux open`
CLI** intent-routing front-end have now **SHIPPED** too (see the status note
above). What remains **future work**:

- **RBAC / OIDC / mTLS**, multi-tenant isolation, and principal-scoped tokens
  fleet-wide (v1 ships two coarse static tokens: admin + register);
- **gateway-cert pinning / CA trust** (v1 trusts self-signed gateway certs via
  `--gateway-tls-insecure`, default on);
- a **cached fleet index** (today fan-out is live per request);
- **cross-host session migration** and agent-ownership arbitration.

The shipped slice is forward-compatible with all of the above: the registry
shape (`name`/`ssh`/`labels`) and the fan-out/aggregation seam generalize from
"static config + SSH" to "control-plane index + gateway REST" without a client
redesign.

---

## 9. Agent Workflows (Why Structured State Wins)

Concrete end-to-end flows an AI agent runs against the API, showing why
structured state beats scraping a byte stream.

### 9.1 Single-host: build-and-branch

```bash
# Create a session and capture its id (structured, not scraped)
ID=$(curl -s -XPOST https://gw/v1/sessions -H "$AUTH" \
  -d '{"name":"build","command":["bash"],"size":{"cols":120,"rows":40}}' | jq -r .id)

# Drive it (binary-safe input)
curl -s -XPOST https://gw/v1/sessions/$ID/input -H "$AUTH" \
  -d '{"text":"cargo test 2>&1; echo DONE_$?\n"}'

# Wait on SEMANTIC state, not a sleep
RESULT=$(curl -s -XPOST https://gw/v1/sessions/$ID/wait -H "$AUTH" \
  -d '{"kind":"regex","pattern":"DONE_[0-9]+","timeout_ms":600000}')
# -> {"result":"matched","exit_code":null}

# Read structured screen to extract the exit marker deterministically
curl -s https://gw/v1/sessions/$ID/screen -H "$AUTH" -H 'Accept: text/plain' \
  | grep -oE 'DONE_[0-9]+'
# -> DONE_0   → tests passed; branch the agent's plan on this
```

The agent never parsed a VT100 byte soup. It waited on a regex over decoded
output and read a flattened-but-faithful screen. With a raw byte stream (ttyd
et al.), it would have to reassemble escape sequences, guess where the prompt is,
and hope the chunk boundaries fell on character boundaries.

### 9.2 Why the JSON screen matters (vs byte scraping)

Consider an agent monitoring a TUI installer that paints a progress bar and a
colored status field. Over a byte stream, "is the status line green (success) or
red (failure)?" requires emulating the terminal client-side. Over
`GET /v1/sessions/{id}/screen`:

```jsonc
// cell at (status_row, status_col)
{ "ch": "✓", "fg": {"Rgb":[0,200,0]}, "bg":"Default", "bold": true, … }
```

The agent reads `fg == Rgb(0,200,0)` directly. **The daemon already parsed the
VT; the agent inherits that for free.** No web-terminal-only tool can offer this.

### 9.3 Multi-host: fan-out across the fleet (AW6 + AW2)

```python
# Pseudocode an agent runs against the fleet control plane
hosts = GET("/fleet/v1/sessions?project=api&env=dev")   # discovery
for h in hosts.hosts_without("migrate"):
    sid = POST(f"https://{h.gw}/v1/sessions",
               json={"name":"migrate","command":["./migrate.sh"]}).id
    POST(f"https://{h.gw}/v1/sessions/{sid}/input", json={"key":"Enter"})

# Wait on all in parallel, branch per-host on exit code
for h, sid in started:
    r = POST(f"https://{h.gw}/v1/sessions/{sid}/wait", json={"kind":"exit"})
    if r.exit_code != 0:
        screen = GET(f"https://{h.gw}/v1/sessions/{sid}/screen")  # structured
        report_failure(h, extract_error(screen))                 # cell-precise
```

One agent, many hosts, each session driven by `create → send → wait → peek →
branch on exit code`. This is the multi-system AI coding workflow `spec.md` §0/§10
targets, and it is **only** ergonomic because every session is structured state,
not a pixel stream.

---

## 10. Cross-Cutting Definition of Done

The agent-native API + fleet differentiator is "delivered (v1)" when:

- [x] The public `/v1` API exists, is documented (OpenAPI 3.1 — served at
      `GET /v1/openapi.json`, committed to `docs/openapi.yaml`), and is
      **decoupled** from `remux_core::protocol` (AW0).
- [ ] `remuxd` remains Unix-socket-only; no workstream added a daemon network
      listener (AW1, AW2, AW4 all respect this).
- [ ] Humans reach remote hosts via `remux --host` / `host:session` over SSH,
      no exposed ports (AW1).
- [ ] Agents/services drive sessions over REST: CRUD, input, **screen-as-JSON**,
      wait, scrollback (AW2).
- [ ] Interactive consumers stream over **binary** WebSocket frames; OUTPUT bytes
      are verbatim, non-UTF-8 safe; a structured `/events` channel exists (AW3).
- [x] v1 auth = TLS + static bearer with read/read-write scopes, deny-by-default,
      per-request audit logging; daemon local-only (AW4). OIDC/mTLS/fine-grained
      RBAC remain deferred to AW6.
- [x] A reference xterm.js consumer exists and is **not** the headline (AW5) —
      a minimal built-in client served by the gateway (`--no-web-ui` to disable);
      rich web app / collab / recording remain out of scope.
- [ ] The fleet model is designed and earlier layers are forward-compatible with
      it (AW6).
- [ ] An end-to-end test drives `create → send → wait → peek-json → branch on
      exit code` through the gateway in CI.

---

## 11. Suggested Sequencing

| PR | Scope | Depends on |
| --- | --- | --- |
| PR1 | AW0: `remux-gateway` crate, `/v1` DTOs, `DaemonConn`, OpenAPI scaffold | runtime (done) |
| PR2 | AW1: `remux bridge` + `--host`/`host:session` routing | — (parallel) |
| PR3 | AW2: REST sessions CRUD + input + screen + scrollback | PR1 |
| PR4 | AW4: TLS + bearer auth + scopes (ships *with* AW2 surface) | PR1, PR3 |
| PR5 | AW2: `wait` endpoint (server-side observer predicate) | PR3 |
| PR6 | AW3: WS `/stream` binary framing + `/events` structured channel | PR3, PR4 |
| PR7 | AW5: xterm.js reference SPA | PR6 |
| PR8 | AW6: fleet design doc → host-registry prototype (later) | PR3 |

PR1+PR3+PR4 (the secured structured API) is the differentiator and should land
first; PR2 (SSH) is independent and serves the human story in parallel.

---

## 12. Risks & Open Questions

| Risk / Question | Notes / proposed resolution |
| --- | --- |
| Public API drifts back into coupling with `protocol.rs` | Enforce via the DTO layer + a test that bumps the internal `PROTOCOL_VERSION` and proves `/v1` is unchanged (AW0/T0.5). |
| Base64 creeps onto the WS hot path "for simplicity" | Binary framing is a hard requirement; the §5.5 non-UTF-8 roundtrip test is the guard. Reject any JSON-output-frame PR. |
| Gateway becomes a second daemon (state, PTYs) | The gateway must stay **stateless** beyond conn-pool + auth; PTYs and VT state live only in `remuxd`. |
| `wait` semantics differ between CLI and gateway | Share the predicate logic: extract `cmd/wait.rs`'s loop into a reusable `remux-core`/shared module both the CLI and gateway call. |
| Browser can't set `Authorization` on WS | Use a first-frame auth control message with a short handshake deadline; documented in AW3/AW4. |
| Static tokens are weak for real deployments | Acceptable for v1; explicitly time-boxed — OIDC/JWT/mTLS/RBAC land with AW6. Tokens are principal-shaped now to ease that migration. |
| Snapshot-as-`ScreenView` ties `/v1` to `TerminalSnapshot` | Accept the coupling deliberately (it *is* the differentiator's contract); pin it under `/v1` and version-bump if `terminal.rs` changes shape. |
| Multi-host auth/identity (fleet) | Out of scope for v1; AW6 design keeps gateways independently addressable + token-authed so federation is additive. |
| TLS/cert management burden | v1: operator-provided cert/key paths; ACME/auto-cert deferred. Document loopback `--insecure` dev path. |
| Exposing observer streaming over the network = info leak | Read scope still grants screen/scrollback/observer; treat read tokens as sensitive. Audit every access (AW4). |

---

## 13. References

- Strategic vision (gateway, web, fleet, security): `spec.md` §§6–11.
- Robustness foundation this builds on: [`docs/ROBUSTNESS_PLAN.md`](./ROBUSTNESS_PLAN.md).
- Existing protocol (the 1:1 mapping source): `crates/remux-core/src/protocol.rs`
  (`Request`/`Response`/`Event`, `PROTOCOL_VERSION`, `AttachMode`, `CaptureScreen`).
- Structured screen type (the moat's payload): `crates/remux-core/src/terminal.rs`
  (`TerminalSnapshot`, `CellData`, `CellColor`).
- Existing automation surface (the API, in CLI form):
  `crates/remux-cli/src/cmd/{send,peek,wait}.rs`,
  `crates/remux-cli/src/render_snapshot.rs` (`snapshot_to_text`/`snapshot_to_ansi`).
- Session/identity types: `crates/remux-core/src/session.rs`
  (`SessionId`, `SessionStatus`, `TermSize`).
- Framing reused by `DaemonConn`/bridge: `crates/remux-core/src/framing.rs`.
- Commoditized prior art (the §0.1 trap to avoid): ttyd
  <https://github.com/tsl0922/ttyd>, gotty <https://github.com/yudai/gotty>,
  Wetty <https://github.com/butlerx/wetty>, sshx <https://github.com/ekzhang/sshx>,
  tmate <https://tmate.io/>, code-server <https://github.com/coder/code-server>,
  Coder / Gitpod / Codespaces / Cloud Shell.
- WS / control sequences: <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>;
  WebSocket binary frames: RFC 6455.
