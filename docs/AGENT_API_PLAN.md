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
| AW2 | `remux-gateway` crate (REST control plane) | Sessions CRUD, input, capture-JSON, wait, scrollback over HTTPS | AW0 | P0 | L |
| AW3 | WebSocket interactive stream (binary framing) | Live terminal I/O + structured screen endpoint | AW2 | P1 | M |
| AW4 | Auth & TLS posture (v1: bearer + TLS) | Token auth, TLS termination, deny-by-default | AW2 | P0 (ships with AW2) | M |
| AW5 | Browser terminal (xterm.js consumer) | Reference web UI — **listed last, on purpose** | AW3, AW4 | P2 | M |
| AW6 | Fleet / discovery model | Host registry, cross-host session discovery, intent routing | AW1, AW2 | P2 (design now, build later) | XL |

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

- **T0.1** Create the `remux-gateway` crate (binary `remux-gateway`) in the
  workspace. Depends on `remux-core` (for protocol + framing) and `axum`,
  `tokio`, `tower-http`, `rustls`/`axum-server`.
- **T0.2** Define the `/v1` DTOs (`dto.rs`) and `From<protocol::*> for dto::*`
  (and the reverse for request bodies). Keep all `serde(rename)` decisions here.
- **T0.3** Implement a `DaemonConn` adapter: opens the **local Unix socket**,
  performs the `Hello { version: PROTOCOL_VERSION }` handshake (`protocol.rs:55`),
  and exposes typed `request()`/`subscribe()` helpers reusing
  `remux_core::framing::{read_message, write_message}`.
- **T0.4** Define the public `ApiError` → HTTP status mapping (see §5.4), derived
  from `RemuxError` and the exit-code taxonomy.
- **T0.5** Publish an OpenAPI 3.1 document (`docs/openapi.yaml`) generated from
  the DTOs (e.g. `utoipa`). The spec is the contract; the daemon protocol is not.

### 2.5 Tests

- DTO roundtrip: every `protocol::*` ↔ `dto::*` conversion is lossless for the
  fields the public API promises; unit-tested both directions.
- A `PROTOCOL_VERSION` bump test: simulate a daemon advertising a different
  version in its `Response::Hello`; the gateway must refuse to start serving that
  daemon (clear error), **not** silently proxy a mismatched wire format.
- OpenAPI lint: the generated spec validates against the 3.1 schema in CI.

### 2.6 Definition of Done

- [ ] `remux-gateway` crate exists, builds, links `remux-core`.
- [ ] `/v1` DTOs are defined separately from `protocol.rs`, with tested mappings.
- [ ] `DaemonConn` connects over the **Unix socket only** and handshakes.
- [ ] OpenAPI `/v1` document generated and CI-validated.
- [ ] A daemon `PROTOCOL_VERSION` change requires **zero** public DTO changes
      (proven by a test that bumps the internal const behind a feature flag).

---

## 3. AW1 — SSH Transport (`remux bridge` / `--host`)

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

- [ ] `remux-gateway` serves `/v1` sessions CRUD, input, screen, scrollback, wait.
- [ ] Connects **only** over the local Unix socket; no path reaches the daemon by
      network.
- [ ] Every endpoint goes through the DTO layer (snapshot-as-`ScreenView` aside).
- [ ] Screen endpoint returns structured cells with color/cursor/alt-screen.
- [ ] OpenAPI doc matches the implemented surface; CI checks drift.

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

- [ ] `/stream` uses binary frames with the 1-byte tag; OUTPUT bytes are verbatim.
- [ ] `/events` delivers typed JSON structured events incl. `ScreenView` snapshots.
- [ ] No base64 anywhere on the I/O hot path; non-UTF-8 output verified intact.
- [ ] Observer/Control modes both reachable; control rules enforced at the gateway.
- [ ] Lag triggers a snapshot resync rather than corruption.

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

### 6.3 Deferred to the fleet/control-plane phase (AW6)

OIDC / JWT, mTLS, fine-grained **RBAC**, per-org/team policy, short-lived
credentials, and full audit pipelines are explicitly **out of scope for v1** and
land with the control plane (`spec.md` §11 "Fleet: RBAC, audit logs"). v1's
coarse token model is forward-compatible: a token becomes a degenerate principal;
scopes become RBAC permissions.

### 6.4 Tests

- No-token config ⇒ gateway refuses to bind (fail-closed) — asserted.
- Missing/invalid bearer ⇒ `401`; read-token doing a write ⇒ `403`.
- TLS required: plain HTTP to a TLS listener is rejected; `--insecure` only works
  on loopback.
- Audit line emitted with hashed token id, never the raw token.

### 6.5 Definition of Done

- [ ] TLS-terminating gateway; HTTP refused except explicit loopback dev.
- [ ] Static bearer tokens with read / read+write scopes; deny-by-default.
- [ ] Daemon still Unix-socket-only; no code path adds a daemon network listener.
- [ ] Audit logging of every API call (hashed token id).
- [ ] OIDC/JWT/mTLS/RBAC documented as deferred to AW6.

---

## 7. AW5 — Browser Terminal (xterm.js) — *Listed Last, On Purpose*

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

- [ ] xterm.js SPA attaches via binary WS, renders OUTPUT bytes verbatim.
- [ ] Session list + live status come from `/v1` + `/events`, no scraping.
- [ ] No feature creep into collab/recording; it stays a reference consumer.

---

## 8. AW6 — Fleet / Discovery Model (Design-Level, the Longer-Term Moat)

**Goal:** Sketch the layer that turns "one daemon, one host" into "one pane for
humans **and** agents across a fleet." This is `spec.md` §10. We **design** it
now to keep AW0–AW4 forward-compatible; we **build** it later.

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

Full control-plane build-out, RBAC, multi-tenant isolation, cross-host session
migration, and agent-ownership arbitration are **future work**. This section is a
design contract, not a workstream with code DoD.

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

- [ ] The public `/v1` API exists, is documented (OpenAPI), and is **decoupled**
      from `remux_core::protocol` (AW0).
- [ ] `remuxd` remains Unix-socket-only; no workstream added a daemon network
      listener (AW1, AW2, AW4 all respect this).
- [ ] Humans reach remote hosts via `remux --host` / `host:session` over SSH,
      no exposed ports (AW1).
- [ ] Agents/services drive sessions over REST: CRUD, input, **screen-as-JSON**,
      wait, scrollback (AW2).
- [ ] Interactive consumers stream over **binary** WebSocket frames; OUTPUT bytes
      are verbatim, non-UTF-8 safe; a structured `/events` channel exists (AW3).
- [ ] v1 auth = TLS + static bearer (read/write scopes), deny-by-default; daemon
      local-only (AW4).
- [ ] A reference xterm.js consumer exists and is **not** the headline (AW5).
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
