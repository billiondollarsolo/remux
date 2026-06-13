# Remux Robustness & Usability Plan

> **Status:** Implemented (WS0–WS6). This document is the source of truth for the
> work taking `remux` from "working prototype" to "robust, daily-usable terminal
> multiplexer" on par with — and beyond — [`coder/boo`](https://github.com/coder/boo).
>
> All seven workstreams have landed on `master`, including the formerly deferred
> **WS5 / T5.2** (the global-lock → per-session-lock refactor) — see §7.
> Resolved product decisions (see §11): detach uses a **`Ctrl-a` prefix**
> (default, configurable); the TUI is exposed as **`remux ui`** (the `remux-tui`
> binary remains); boo-style **command aliases** are shipped.
>
> Nothing in this plan touches the long-term `spec.md` roadmap (gateway, web,
> fleet). It is exclusively about making the **local runtime** correct,
> faithful, scriptable, and trustworthy first. Everything else builds on that
> foundation.

---

## 0. Context & Motivation

### 0.1 What we are comparing against

`coder/boo` is a screen/tmux-style session manager written in Zig on top of
**libghostty-vt**. It shares remux's core architecture (a daemon owns the PTY;
a thin client attaches over a Unix socket; sessions outlive clients). Its
notable strengths:

| Boo capability | Why it matters |
| --- | --- |
| Reattach **reconstructs the screen from parsed VT state**, not raw byte replay | TUIs (`vim`, `htop`, `less`) come back correctly, with no escape-sequence corruption |
| **Binary-safe input** (`send --text`) and raw passthrough | No fidelity loss, no quoting hell, works for AI agents |
| **Automation primitives**: `send`, `peek`, `wait`, JSON output | Headless scripting / agent control without a TTY |
| **Meaningful exit codes** (`0` ok, `3` no session, `4` timeout) | Scripts can branch on outcome |
| Detached **terminal-query answering** | A backgrounded TUI that queries the terminal doesn't hang |
| Single-attach steal model | Simple, predictable ownership |

### 0.2 Where remux stands today (verified against the code)

Remux already has a clean, well-factored architecture and a good protocol. But
several of its most important subsystems are **built but not wired up**, and the
input/rendering path is **lossy**. Concrete findings:

| # | Finding | Evidence |
| --- | --- | --- |
| F1 | **The VT snapshot is computed and shipped but never rendered by the client.** Reattach only replays plain-text scrollback. | `crates/remux-daemon/src/session_manager.rs:363` builds `vt_snapshot`; `crates/remux-cli/src/cmd/attach.rs:59-68` writes only `bootstrap.scrollback`; `attach.rs:162` ignores `Event::StateSnapshot`. |
| F2 | **Color is discarded in snapshots.** Named and RGB colors collapse to `None`; only indexed (palette) colors survive, and only as `Option<u8>`. | `crates/remux-daemon/src/vt.rs:90-96`; `crates/remux-core/src/terminal.rs:5-12`. |
| F3 | **Input is decoded then re-encoded by hand, losing fidelity and with outright bugs.** | `crates/remux-cli/src/cmd/attach.rs:247-297`. F-key table emits wrong sequences for F5/F11/F12 (`attach.rs:279-289`); no Alt/Meta, no bracketed paste, no mouse, no shift+arrow. |
| F4 | **No headless automation surface.** `SendInput` exists in the protocol but no CLI `send`, `peek`, or `wait` command exposes it. | `crates/remux-cli/src/main.rs:27-79` (Commands enum). |
| F5 | **Persistence is dead code.** `load_sessions` is `#[allow(dead_code)]` and never called; `persist_scrollback` config knob is unimplemented. | `crates/remux-daemon/src/persistence.rs:35-36`; `crates/remux-core/src/config.rs:61`. |
| F6 | **No CI and no integration tests.** A capable testkit (`DaemonHarness`, `TestClient`) exists but nothing exercises it; there is no `.github/`. | No `.github/` dir; no `tests/` dirs; `crates/remux-testkit/src/lib.rs`. |
| F7 | **`KillSession` broadcasts a premature `SessionExited{None}`** before the real exit, producing a duplicate/incorrect event. | `crates/remux-daemon/src/daemon.rs:289-311`. |
| F8 | **Attach resizes the PTY but not the VT.** VT only resizes via `ResizeSession`. Snapshot dimensions can drift from the real terminal. | `session_manager.rs:346-357` vs `session_manager.rs:418-454`. |
| F9 | **Configured `detach_key` is ignored**; Ctrl-Q is hardcoded. | `config.rs:103` defines it; `attach.rs:228-231` hardcodes it. |
| F10 | **VT scrollback depth is hardcoded** to 10 000, ignoring `max_scrollback_lines`. | `vt.rs:24`. |
| F11 | **Coarse global lock**: a single `Mutex<SessionManager>` is taken on every PTY chunk and every disconnect iterates all sessions. | `daemon.rs:17`, `daemon.rs:374-384`, `daemon.rs:170-187`. |

### 0.3 Guiding principles for this plan

1. **Transparency first.** A multiplexer's job is to be an invisible pipe.
   Prefer raw byte passthrough over interpret-and-reconstruct wherever possible.
2. **Reconstruct, don't replay.** On reattach, paint the screen from parsed VT
   state (boo's core insight). Raw replay is a fallback, not the primary path.
3. **Everything scriptable.** Every interactive action has a headless,
   JSON-capable, exit-code-bearing CLI equivalent.
4. **No dead code in `main`.** If a subsystem ships, it is wired and tested. If
   it isn't ready, the config knob and the code are both removed.
5. **Tested reliability.** The thing whose entire value proposition is "it won't
   lose my session" must have integration tests proving it.

---

## 1. Workstream Overview

Work is organized into seven workstreams (WS). The **critical path** is
WS0 → WS1 → WS2; WS3 unlocks the agent story; WS4–WS6 harden and polish.

| WS | Title | Closes | Priority | Est. size |
| --- | --- | --- | --- | --- |
| WS0 | Test & CI foundation | F6 | P0 (do first) | M |
| WS1 | Faithful reattach (snapshot render + color fidelity) | F1, F2, F8, F10 | P0 | L |
| WS2 | Raw input passthrough | F3, F9 | P0 | M |
| WS3 | Automation surface (`send`/`peek`/`wait`/JSON/exit codes) | F4 | P1 | L |
| WS4 | Honest persistence (scrollback + recovery, or removal) | F5 | P1 | M |
| WS5 | Concurrency & correctness hardening | F7, F11 | P2 | M |
| WS6 | Usability polish | — | P2 | S–M |

Recommended execution order: **WS0, then WS1, then WS2**, landing each as its own
PR with green CI. WS3 follows. WS4–WS6 are independent and can interleave.

---

## 2. WS0 — Test & CI Foundation

**Goal:** Before changing behavior, make behavior observable and regression-proof.
This is the highest-ROI work because every subsequent workstream depends on it to
prove correctness.

### 2.1 Reasoning

The testkit (`DaemonHarness` + `TestClient`) is already capable but unused
(`crates/remux-testkit/src/lib.rs`). There is no `.github/`. We cannot claim
"robust" without a CI gate and an end-to-end test that a session survives a
detach/reattach cycle.

### 2.2 Tasks

#### T0.1 — GitHub Actions CI

Create `.github/workflows/ci.yml` with jobs:

```yaml
name: ci
on:
  push:
    branches: [master]
  pull_request:
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo build --workspace --locked
      # integration tests need the daemon binary built first
      - run: cargo test --workspace --locked
```

- Add a matrix entry for `macos-latest` (remux targets Linux + macOS per
  `packaging/`).
- `--locked` enforces `Cargo.lock` is authoritative.

#### T0.2 — Integration test crate wiring

Add `crates/remux-testkit/tests/` (or a dedicated `crates/remux-tests/`)
exercising `DaemonHarness`. Note `DaemonHarness::start()` shells out to the
`remuxd` binary (`lib.rs:127`), so tests must run after `cargo build`. Document
this ordering in CI (the `cargo test` step builds bins first by default, but
add an explicit `cargo build -p remux-daemon` to be safe).

#### T0.3 — Core roundtrip integration test

```
create → ls (assert present) → attach (assert bootstrap) →
send "echo hello\n" → wait/peek (assert "hello" visible) →
kill → ls (assert gone/Exited)
```

This single test guards F1–F4 simultaneously once they land.

#### T0.4 — `cargo fmt` baseline

Run `cargo fmt --all` once, commit the result, so the `--check` gate is green
from day one.

### 2.3 Tests / verification

- CI passes on the branch.
- `cargo test --workspace` runs ≥1 integration test that starts a real daemon.

### 2.4 Definition of Done

- [ ] `.github/workflows/ci.yml` green on PR.
- [ ] `cargo fmt --check`, `clippy -D warnings`, `build --locked`, `test` all gated.
- [ ] At least one end-to-end test using `DaemonHarness` that creates, sends to,
      reads from, and kills a session.
- [ ] macOS job present (may be allowed-to-fail initially if runners are flaky,
      but must build).

---

## 3. WS1 — Faithful Reattach

**Goal:** When you reattach, the screen looks *exactly* as it did, including TUIs
on the alternate screen, with full color. This is the single most visible
robustness gap and the headline feature parity item vs boo.

### 3.1 Reasoning

Today the daemon builds a `TerminalSnapshot` (`vt.rs:45`) and ships it in
`AttachBootstrap.vt_snapshot` (`session_manager.rs:363-369`), but the client
throws it away and replays line-based scrollback bytes instead
(`attach.rs:59-68`). Consequences:

- `vim`/`htop`/`less` (alternate-screen apps) reattach to a blank or corrupted
  screen — the live UI is gone.
- Cursor position, in-progress prompts, and screen-relative layout are lost.
- Even for plain shells, scrollback replay strips `\r` and splits on `\n`
  (`scrollback.rs:27-41`), so any cursor-addressed output is mangled.

Boo's insight: **don't replay bytes, repaint from parsed state.** Remux has the
parsing (`alacritty_terminal`); it just needs to (a) make the snapshot lossless
enough and (b) render it on the client.

### 3.2 Design

#### Two-phase reattach payload

On attach the client should receive, and render in order:

1. **History** — scrollback *above* the current screen, for scroll-up. Replayed
   as raw bytes (acceptable: it's history, not live).
2. **Screen snapshot** — the visible grid + cursor + modes, painted
   deterministically by emitting SGR + cursor-position sequences cell-run by
   cell-run.

This mirrors how `tmux` and boo rebuild a pane.

#### Richer cell model (fixes F2)

`crates/remux-core/src/terminal.rs` must carry full color and attributes.
Replace the lossy `Option<u8>` with an explicit color enum:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CellColor {
    Default,
    /// 16-color / 256-color palette index.
    Indexed(u8),
    /// 24-bit truecolor.
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellData {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
    // (optional) blink, hidden — add if cheap
}
```

Update `vt.rs` `color_to_option` → `convert_color`:

```rust
fn convert_color(color: alacritty_terminal::vte::ansi::Color) -> CellColor {
    use alacritty_terminal::vte::ansi::Color;
    match color {
        Color::Named(named) => /* map NamedColor -> Indexed(0..=15) or Default */,
        Color::Indexed(i) => CellColor::Indexed(i),
        Color::Spec(rgb) => CellColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}
```

Pull the additional flags in `vt.rs:59-67` from `cell.flags` (`Flags::DIM`,
`Flags::INVERSE`, `Flags::STRIKEOUT`, etc.).

> **Protocol note:** `terminal.rs` types are serialized in `AttachBootstrap` and
> `Event::StateSnapshot`. Changing `CellData` is a wire-format break. Since the
> README marks the project **pre-alpha** ("the protocol ... is still evolving"),
> a clean break is acceptable now. Add a `const PROTOCOL_VERSION` to
> `remux-core` and have the client send it in `AttachSession` so future breaks
> fail loud instead of corrupting. (See WS5/T5.4.)

#### Snapshot → escape sequences (the renderer)

Add `crates/remux-cli/src/render_snapshot.rs` that turns a `TerminalSnapshot`
into a byte stream the local terminal can consume:

```rust
/// Produce a byte sequence that repaints `snap` onto the user's terminal.
pub fn paint_snapshot(snap: &TerminalSnapshot) -> Vec<u8> {
    let mut out = Vec::new();
    // 1. If alt-screen, enter it (CSI ? 1049 h) so we don't pollute scrollback.
    // 2. Clear screen + home cursor (CSI 2 J, CSI H).
    // 3. For each row, walk cells, coalescing runs with identical SGR state;
    //    emit one SGR per run then the run's chars. Reset SGR at row end.
    // 4. Position the cursor (CSI <row+1> ; <col+1> H).
    // 5. Restore cursor visibility / other modes captured in the snapshot.
    out
}
```

Key correctness points:
- **Coalesce runs** with identical attributes to keep the payload small.
- **SGR reset** (`CSI 0 m`) between state changes; never assume terminal state.
- **Alt-screen flag** (`snap.alternate_screen`, already captured at `vt.rs:85`)
  decides whether we enter `1049h`. For the normal screen, repaint in place.
- Skip trailing blank cells per line to avoid emitting full-width padding.

#### Wire it into the client (fixes F1)

In `crates/remux-cli/src/cmd/attach.rs`:
- After entering raw mode, if `bootstrap.vt_snapshot` is `Some`, write
  `paint_snapshot(&snap)` instead of (or after) the history bytes.
- Handle `Event::StateSnapshot { snapshot, .. }` (currently dropped at
  `attach.rs:162`) by repainting — needed when control is regained or the daemon
  pushes a resync.

#### Resize the VT on attach (fixes F8)

In `session_manager.rs::attach_session`, when `mode == Control` and the size
changed, also call `vt.resize(size)` (today only the PTY master is resized at
`session_manager.rs:346-357`). Otherwise the snapshot grid dimensions disagree
with the client's terminal.

#### Honor configured scrollback depth (fixes F10)

`VtState::new` hardcodes `scrolling_history: 10000` (`vt.rs:24`). Thread
`config.daemon.max_scrollback_lines` through to it.

### 3.3 Examples

**Before:** `remux attach editor` where `editor` is running `vim` → blank screen
or raw scrollback dump; vim's UI is gone.

**After:** `remux attach editor` → vim's full-screen UI is repainted, cursor in
the right cell, colors intact, status line present.

### 3.4 Tests

- **Unit (render_snapshot):** Construct a small `TerminalSnapshot` (e.g. 3×2 with
  one red bold cell) and assert `paint_snapshot` emits the expected SGR + chars +
  cursor-position bytes. Golden-bytes test.
- **Unit (vt color):** Feed `\x1b[38;2;255;0;0mX` through `VtState::process`,
  snapshot, assert the cell's `fg == CellColor::Rgb(255,0,0)`.
- **Unit (alt screen):** Feed `\x1b[?1049h`, assert `snapshot().alternate_screen`.
- **Integration:** Create a session running a tiny program that draws a known
  full-screen pattern (e.g. positions cursor and writes "OK" at row 5 col 10 on
  alt screen). Attach via `TestClient`, capture the bootstrap, render in a
  headless `alacritty_terminal` instance, assert the cell at (5,10) reads "OK".
  (We can parse our own `paint_snapshot` output with `alacritty_terminal` to
  verify round-trip fidelity without a real TTY.)

### 3.5 Definition of Done

- [ ] `CellData` carries full color (Default/Indexed/Rgb) + reverse/dim/strike.
- [ ] `vt.rs` no longer discards named or RGB color.
- [ ] `paint_snapshot` exists, unit-tested with golden bytes.
- [ ] `attach.rs` repaints from `vt_snapshot` on attach and on `StateSnapshot`.
- [ ] VT is resized on controlling attach; `max_scrollback_lines` is honored.
- [ ] Integration test proves an alt-screen TUI reattaches with correct content
      and color via snapshot round-trip.
- [ ] Manual verification (documented in PR): `vim`, `htop`, `less` survive
      detach/reattach visually intact.

---

## 4. WS2 — Raw Input Passthrough

**Goal:** Keystrokes reach the PTY byte-for-byte. No interpret-and-rebuild. This
fixes broken keys today and makes paste/mouse/modifier handling "just work."

### 4.1 Reasoning

`attach.rs` reads `crossterm` `KeyEvent`s and rebuilds bytes via
`encode_key_event` (`attach.rs:247-297`). That path:
- Loses Alt/Meta-prefixed keys, Shift+Arrow, Ctrl+special, and any sequence
  crossterm doesn't model.
- Has bugs: the F-key table (`attach.rs:279-289`) emits `ESC[20~` for F5
  (should be `ESC[15~`) and wrong codes for F11/F12.
- Cannot forward **bracketed paste**, **mouse reporting**, or **focus events**.

A terminal multiplexer should not parse input it's only going to forward. The
correct model: put the local terminal in raw mode and **copy stdin bytes
straight to `SendInput`**, scanning only for the detach key.

### 4.2 Design

- Replace the `crossterm::event::read()` loop with a raw byte reader on stdin
  (e.g. a `spawn_blocking` loop doing `io::stdin().read(&mut buf)`), and forward
  `buf[..n]` verbatim as `Request::SendInput`.
- **Detach key detection on the raw stream:** scan the incoming bytes for the
  configured detach sequence (default Ctrl-Q = `0x11`). Support a two-key prefix
  (boo uses `Ctrl-A d`) so the detach key isn't stolen from apps that need it.
  Make this configurable via `config.client.detach_key` (fixes F9). Recommended
  default: a prefix key (`Ctrl-A`) + `d`, matching screen/boo muscle memory, with
  the prefix itself sent through on double-press (`Ctrl-A Ctrl-A` → literal
  `Ctrl-A`).
- **Resize** still comes from a separate signal path: keep using
  `crossterm::Event::Resize` *or* install a `SIGWINCH` handler; resize is not
  part of the byte stream, so it's fine to handle it out of band. Continue
  sending `Request::ResizeSession`.
- **Enable the right modes on attach** so apps behave: optionally enable
  bracketed paste forwarding and pass mouse sequences through untouched (we just
  copy bytes, so mouse "just works" as long as we don't swallow them).

### 4.3 Migration note

`encode_key_event` and `handle_input_event` (`attach.rs:212-297`) are deleted.
The TUI (`remux-tui`) keeps using crossterm for its own UI — only the
**attach passthrough** changes. Keep the two concerns separate.

### 4.4 Examples

- **Before:** Pressing F5 in a session sends the wrong escape; Alt+b does nothing.
- **After:** Every key, paste, and mouse action reaches the app exactly as if you
  were typing into it directly.

### 4.5 Tests

- **Unit (detach scanner):** Feed byte streams to the detach-detection function;
  assert it (a) triggers on the configured sequence, (b) forwards a doubled
  prefix as a literal, (c) forwards all other bytes unchanged including bytes
  that *contain* the prefix mid-paste.
- **Integration:** Attach (control), `send` a multi-byte sequence including an
  ESC-prefixed key and a UTF-8 multibyte char, then `peek` the session's input
  echo to confirm byte-exact delivery. (Run a `cat`-like child and read back.)
- **Property-ish:** Random byte buffers (excluding the detach sequence) must pass
  through unchanged.

### 4.6 Definition of Done

- [ ] Attach forwards raw stdin bytes; `encode_key_event` removed.
- [ ] Detach sequence configurable via `detach_key`; default documented; prefix
      double-press sends a literal.
- [ ] Resize handled out-of-band (SIGWINCH or crossterm resize event).
- [ ] F-keys, Alt/Meta, Shift+Arrow, mouse, and bracketed paste verified working
      (integration + manual notes in PR).
- [ ] No regression: Ctrl-C, Ctrl-D, Ctrl-Z reach the app.

---

## 5. WS3 — Automation Surface

**Goal:** Everything you can do interactively, you can do headlessly and in a
script — the capability that makes remux usable by CI and AI agents, which
`spec.md` positions as a core audience.

### 5.1 Reasoning

`spec.md` repeatedly cites "humans and AI agents share the same runtime," but the
CLI exposes no non-interactive control. Boo's `send`/`peek`/`wait` + JSON + exit
codes are precisely what makes it agent-friendly. The protocol already has
`SendInput` and `ReadScrollback`; we need new CLI verbs and one new request
(`CaptureScreen`).

### 5.2 New CLI commands

Add to `Commands` (`crates/remux-cli/src/main.rs:27`):

#### `remux send`

```
remux send <session> --text "ls -la\n"     # binary-safe, no shell quoting
remux send <session> --bytes-hex 1b5b41     # raw bytes (e.g. ESC [ A)
remux send <session> --key Enter            # named keys for convenience
echo -n "data" | remux send <session> --stdin
```

- Maps to `Request::SendInput`. **Does not attach** — fire-and-forget.
- `--text` does NOT interpret shell escapes beyond explicit `\n`, `\t`, `\\`
  (document precisely). Binary safety is the whole point.

#### `remux peek` / `remux capture`

```
remux peek <session>                 # render current screen as plain text
remux peek <session> --json          # structured: cells, cursor, size, modes
remux peek <session> --ansi          # screen with SGR colors preserved
```

- New request `Request::CaptureScreen { session }` →
  `Response::Screen(TerminalSnapshot)`, served from the live `VtState`
  (`session_manager` already holds it). Reuse WS1's snapshot.
- `--json` serializes the `TerminalSnapshot` (already `Serialize`).
- Plain-text mode flattens cells to chars per row, trimming trailing blanks.

#### `remux wait`

```
remux wait <session> --idle 500ms          # block until no output for 500ms
remux wait <session> --for-regex 'PASS|FAIL' --timeout 30s
remux wait <session> --exit                # block until the session exits
```

- Implemented client-side by subscribing to `Event::Output` (attach as
  **Observer**, so it doesn't steal control) and applying the predicate.
- `--idle` resets a timer on each output chunk.
- `--for-regex` matches against a rolling decoded buffer.
- `--exit` returns the child's exit code as the process exit code.

### 5.3 Exit-code taxonomy (matches/extends boo)

Define once in `remux-cli` and apply uniformly:

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Generic/usage error |
| 3 | Session not found |
| 4 | Timeout (`wait`) |
| 5 | Permission denied (not controlling client) |
| 6 | Daemon unreachable |
| 124 | (alias for timeout, optional, matches coreutils `timeout`) |

Map `RemuxError` variants (`crates/remux-core/src/error.rs`) to these in a single
`fn exit_code_for(&RemuxError) -> i32`. Replace the blanket `process::exit(1)` in
`main.rs:131/139/158`.

### 5.4 JSON everywhere

`ls --json` and `inspect --json` already exist. Ensure `peek --json`, and add
`--json` to `new` (emit the created `SessionDetails`) and `wait` (emit
`{ "result": "matched"|"idle"|"exited"|"timeout", "exit_code": N }`). Standardize
on serializing the existing serde types so output is stable.

### 5.5 Examples (agent workflow)

```sh
id=$(remux new --json --name build -- cargo build | jq -r .id)
remux send "$id" --text "\n"
remux wait "$id" --idle 2s --timeout 300s || echo "build stuck"
remux peek "$id" --ansi | tail -20
remux kill "$id"
```

### 5.6 Tests

- **Integration `send`:** create → `send --text "echo marker\n"` → `wait --idle`
  → `peek` contains "marker".
- **Integration `peek --json`:** assert valid JSON, correct `cols/rows`, cursor
  present.
- **Integration `wait --for-regex`:** start a child that prints "READY" after a
  delay; `wait --for-regex READY --timeout 5s` returns 0; a non-matching regex
  with short timeout returns 4.
- **Exit codes:** `remux peek nonexistent` exits 3; `wait ... --timeout 1ms`
  exits 4; sending as a non-controller exits 5.

### 5.7 Definition of Done

- [ ] `send`, `peek` (text/ansi/json), `wait` (idle/regex/exit) implemented.
- [ ] `Request::CaptureScreen` / `Response::Screen` added and served from VT.
- [ ] `--json` available on `new`, `ls`, `inspect`, `peek`, `wait`.
- [ ] Centralized `exit_code_for`; documented exit-code table in README.
- [ ] Integration tests for each verb incl. timeout and not-found paths.

---

## 6. WS4 — Honest Persistence

**Goal:** Either implement real recovery or stop advertising it. No dead code,
no lying config knobs.

### 6.1 Reasoning

`persistence::load_sessions` is `#[allow(dead_code)]` and never called
(`persistence.rs:35-36`); `persist_scrollback` is a config field with no
implementation (`config.rs:61`). Today a daemon restart silently loses all
session metadata. A multiplexer that claims persistence must not surprise users.

### 6.2 Important constraint

When `remuxd` exits, the PTYs it owns die with it (they are child processes). So
"persistence across daemon restart" can only restore **metadata and scrollback
history**, not live processes — unless we re-exec the original command. Be
explicit about the chosen semantics:

**Option A (recommended, scoped):** *Crash-resilient scrollback + metadata.*
- Persist session metadata (already structured in `PersistedSession`) and,
  if `persist_scrollback = true`, periodically flush scrollback to disk.
- On daemon start, `load_sessions` and present prior sessions as `Exited`
  (clearly marked "ended: daemon restart") so users can read their scrollback
  via `logs`/`peek` even though the process is gone. Optionally offer
  `remux new --from <old-id>` to relaunch the same command+cwd.

**Option B (larger, later):** *Process re-parenting* via a supervisor or
`systemd`/`launchd` socket activation so the daemon can restart without killing
sessions. This is a bigger effort; defer to a future phase, but design Option A
so it doesn't preclude B.

### 6.3 Tasks

- Call `load_sessions` on daemon startup (`crates/remux-daemon/src/main.rs`),
  reconstructing read-only `Exited` handles for prior sessions.
- Implement `persist_scrollback`: on a timer / on session exit, write the ring
  buffer to `<data.dir>/sessions/<id>.scrollback`. Cap by
  `max_scrollback_lines`. Load it back so `logs`/`peek` work post-restart.
- Implement `cleanup_exited_after_hours` (`config.rs:63`) — currently also unused
  — to prune old persisted sessions on startup.
- If we decide *not* to ship persistence now: delete `persist_scrollback`,
  `cleanup_exited_after_hours`, and the dead `load_sessions`/`remove_session`,
  and update the README so it doesn't promise persistence.

### 6.4 Tests

- **Integration restart:** start daemon (via harness) with a temp data dir,
  create a session, write known output, stop the daemon, start a new daemon on
  the same data dir, assert the session appears as `Exited` and its scrollback is
  readable via `logs`.
- **Cleanup:** persist a session with an old `created_at`, start daemon, assert
  it's pruned per `cleanup_exited_after_hours`.

### 6.5 Definition of Done

- [ ] A clear, documented persistence semantic (Option A) — or explicit removal.
- [ ] No `#[allow(dead_code)]` left on persistence functions.
- [ ] `persist_scrollback` and `cleanup_exited_after_hours` either work or are gone.
- [ ] Restart integration test passes.
- [ ] README persistence section matches reality.

---

## 7. WS5 — Concurrency & Correctness Hardening

**Goal:** Remove correctness foot-guns and the coarse locking that will bite
under many concurrent sessions.

### 7.1 Tasks

#### T5.1 — Fix premature exit broadcast (F7)

`KillSession` broadcasts `Event::SessionExited { exit_code: None }` immediately
(`daemon.rs:289-311`), then the PTY pump broadcasts the *real* exit later. Remove
the eager broadcast; rely on the pump's authoritative `SessionExited` with the
true code. If clients need instant feedback, send a distinct
`Event::SessionTerminating` instead of a fake exit.

#### T5.2 — Per-session locking (F11) — **DONE**

> **Done:** The registry now stores per-session handles behind their own locks
> (`sessions: HashMap<SessionId, Arc<tokio::sync::Mutex<SessionHandle>>>`). The
> outer `Mutex<SessionManager>` still guards the map + `name_index`, but lookups
> hold it only long enough to clone the `Arc<Mutex<SessionHandle>>`; the PTY
> output pump clones its handle ONCE at startup and thereafter locks only that
> handle per chunk — never the registry — so independent sessions no longer
> contend. Lock ordering is strictly `registry -> handle`, never two handles at
> once, and the hot path never takes the registry lock while holding a handle,
> so there is no lock-order inversion.

The single `Mutex<SessionManager>` (`daemon.rs:17`) was locked on **every** PTY
output chunk and the whole map was iterated on every client disconnect (the
latter already fixed by T5.3). Implemented as:
- Outer `Mutex` over the registry (`sessions` map + `name_index`) for
  lookup/insert/remove only.
- Each `SessionHandle` behind its own `Arc<Mutex<..>>`; all per-session work
  (`append_to_scrollback`, `broadcast_event`, `attach`/`detach`/`resize`/
  `send_input`/`capture_screen`/`kill`/`mark_exited`) is a method on the handle.
- The PTY pump receives its handle clone from `create_session` and locks only
  that handle on the hot path; the registry is touched only on the exit path
  (for `Config`-driven scrollback persistence), never while holding the handle.

Internal refactor only — no protocol/CLI/event change. Covered by the existing
WS0 integration tests (which start a real daemon and exercise concurrency) plus
a new daemon unit test (`per_session_locking_concurrent_output_no_deadlock`)
that drives output + broadcast across several sessions concurrently, each task
holding only its own handle lock, asserting no deadlock and per-session output
integrity.

#### T5.3 — Track per-client attachments

`detach_client_from_all_sessions` (`daemon.rs:170`) lists *all* sessions and
attempts detach on each. Maintain a `HashSet<SessionId>` per connected client so
disconnect is O(attached), not O(total).

#### T5.4 — Protocol version handshake

Add `const PROTOCOL_VERSION: u32` to `remux-core`. Client includes it in the
first request (or a new `Hello`); daemon rejects mismatches with a clear error.
Prevents silent corruption after the WS1 wire-format change.

#### T5.5 — Backpressure policy review

`broadcast_event` drops events when a subscriber channel is full
(`session_manager.rs:560-575`, capacity 256). For a slow client this silently
loses output → corrupted screen. Decide policy: either (a) coalesce to a
`StateSnapshot` resync when a client falls behind (preferred — leverages WS1), or
(b) apply real backpressure. Document the choice.

### 7.2 Tests

- **T5.1:** Integration — kill a session; assert exactly one `SessionExited` is
  observed and its `exit_code` reflects the signal/real exit, not `None`.
- **T5.2/T5.3:** Stress test — spin up many sessions, attach/detach rapidly from
  multiple clients, assert no deadlock/panic and all detach cleanly.
- **T5.4:** A client sending a bad version gets a clean protocol error, not a
  deserialize panic.
- **T5.5:** Simulate a slow consumer; assert the screen self-heals via resync
  rather than staying corrupted.

### 7.3 Definition of Done

- [ ] No duplicate/fake `SessionExited`.
- [x] PTY pump no longer contends on a global lock for steady-state output (T5.2 — per-session locking).
- [ ] Disconnect cleanup is O(attached sessions).
- [ ] Protocol version handshake in place.
- [ ] Documented, tested backpressure/resync policy.

---

## 8. WS6 — Usability Polish

**Goal:** The rough edges that make the difference between "works" and "pleasant."

### 8.1 Tasks

- **T6.1 Scrollback paging in attach. — DONE.** Attach now has an in-client
  copy/scroll mode (prefix + `[`, also prefix + PageUp). It maintains a bounded
  local line buffer (seeded from `bootstrap.scrollback`, kept current by every
  `Event::Output` chunk, capped at ~10 000 lines), enters the alternate screen
  while scrolling, and renders a window of the buffer with a reverse-video status
  line. Navigation: PageUp/Ctrl-b/k/Up (older), PageDown/Ctrl-f/j/Down (newer),
  Home (oldest), End/G (newest), q/Esc (exit, repaints the live screen from the
  last snapshot). Read-only attach supports it too. Purely client-local — no new
  protocol messages. Windowing math (`visible_window`) and line splitting are
  factored into pure, unit-tested helpers. Implemented in
  `crates/remux-cli/src/cmd/attach.rs`.
- **T6.2 Status / message line.** Brief on-attach hint ("[detach: Ctrl-A d]") and
  transient messages (control stolen, resize) rendered without corrupting the app
  (use a saved-cursor + restore, or the bottom line only).
- **T6.3 `remux attach --read-only`.** Expose the existing `AttachMode::Observer`
  (protocol already supports it) as a CLI flag for safe shoulder-surfing / agent
  monitoring.
- **T6.4 Detached terminal-query answering.** Like boo: when no client is
  attached, the daemon should answer common terminal queries (DA, cursor-position
  report, DSR) from `VtState` so a backgrounded TUI doesn't block waiting for a
  reply. Implement in the PTY pump by detecting query sequences and writing canned
  responses back to the PTY.
- **T6.5 Better `ls`.** Show attached/exited status, last-activity, and (with
  `--preview`, already a flag at `main.rs:42`) a scrollback preview line.
- **T6.6 Signal handling for `remuxd`.** Graceful shutdown on SIGTERM (flush
  persistence, notify clients) — ties into WS4.
- **T6.7 Man pages / `--help` examples / shell completions** (`clap_complete`).

### 8.2 Definition of Done

- [ ] Read-only attach flag works.
- [ ] Detach hint shown; messages don't corrupt the app screen.
- [ ] Detached query answering prevents TUI hangs (tested with a program that
      issues a cursor-position report while detached).
- [ ] Shell completions generated for bash/zsh/fish.

---

## 9. Cross-Cutting: Definition of Done (Global)

The project is "robust and usable" when **all** of the following hold:

- [ ] CI is green on every PR (fmt, clippy `-D warnings`, build `--locked`, tests)
      on Linux and macOS.
- [ ] `vim`/`htop`/`less` survive detach → reattach with pixel-faithful screen and
      full color (WS1).
- [ ] Every key, paste, mouse event, and modifier reaches the app byte-exact (WS2).
- [ ] `send`/`peek`/`wait` enable a full headless create→drive→inspect→kill loop
      with meaningful exit codes (WS3).
- [ ] Persistence behavior is documented and matches reality; no dead code (WS4).
- [ ] No global-lock contention on the output hot path; no fake exit events (WS5).
- [ ] Read-only attach, detach hints, and detached query answering work (WS6).
- [ ] An end-to-end integration test covers the headline flow and runs in CI.
- [ ] README reflects the actual, shipped behavior.

---

## 10. Suggested PR Sequencing

| PR | Scope | Depends on |
| --- | --- | --- |
| PR1 | WS0: CI + fmt baseline + first integration test | — |
| PR2 | WS1a: richer `CellData` + lossless VT color + protocol version const | PR1 |
| PR3 | WS1b: `paint_snapshot` + client repaint + VT resize-on-attach | PR2 |
| PR4 | WS2: raw input passthrough + configurable detach key | PR1 |
| PR5 | WS3a: `send` + exit-code taxonomy + `--json` standardization | PR1 |
| PR6 | WS3b: `peek`/`CaptureScreen` + `wait` | PR3, PR5 |
| PR7 | WS5: locking refactor + exit-event fix + handshake | PR3 |
| PR8 | WS4: persistence (Option A) | PR1 |
| PR9 | WS6: polish (read-only, query answering, completions) | PR3 |

Each PR ships with its own tests and keeps CI green. PR2/PR3 carry the
wire-format break together so `main` is never left half-migrated.

---

## 11. Risks & Open Questions

| Risk / Question | Notes / proposed resolution |
| --- | --- |
| Wire-format break for `CellData` | Acceptable pre-alpha; gate with `PROTOCOL_VERSION` handshake (T5.4) so old clients fail loud. |
| `alacritty_terminal` API churn / private types | `Flags`, `Color`, `NamedColor` mappings may need care; pin the version in `Cargo.lock` (already locked). |
| Snapshot repaint of huge alt-screens | Coalesce runs, trim trailing blanks; payload is one screen, bounded by `cols×rows`. |
| Detach prefix collides with apps using Ctrl-A (Emacs/readline) | Use double-press passthrough; make fully configurable. Default could remain single Ctrl-Q to avoid surprise — decide before PR4. |
| macOS PTY/signal differences | `portable-pty` abstracts most; ensure CI macOS job builds and run smoke tests. |
| Persistence semantics (Option A vs B) | Confirm A is acceptable for now (no live-process recovery). |
| Backpressure vs resync (T5.5) | Prefer resync-on-lag using WS1 snapshot; needs the snapshot path landed first. |

---

## 12. References

- Boo (design inspiration): https://github.com/coder/boo
- Existing remux protocol: `crates/remux-core/src/protocol.rs`
- VT/snapshot: `crates/remux-daemon/src/vt.rs`, `crates/remux-core/src/terminal.rs`
- Attach client (input/render): `crates/remux-cli/src/cmd/attach.rs`
- Session lifecycle: `crates/remux-daemon/src/session_manager.rs`
- Daemon server/pump: `crates/remux-daemon/src/daemon.rs`
- Persistence: `crates/remux-daemon/src/persistence.rs`
- Config: `crates/remux-core/src/config.rs`
- Testkit: `crates/remux-testkit/src/{lib.rs,client.rs}`
- Long-term product vision: `spec.md`
- xterm control sequences (for snapshot repaint & key encoding):
  https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
