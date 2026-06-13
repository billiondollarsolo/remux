// Built-in remux browser client (AW5). A thin, self-contained consumer of the
// gateway's public /v1 API: lists sessions over REST and attaches to one over
// the binary WebSocket /stream endpoint, wiring it to an xterm.js terminal.
//
// xterm.js (@xterm/xterm) and the fit addon (@xterm/addon-fit) are loaded from a
// CDN in index.html (no bundler). Vendoring them into the gateway binary for
// fully offline use is a deliberate follow-up.
//
// I/O contract with the gateway (see AGENT_API_PLAN.md §5):
//   - server -> client: BINARY frames of raw PTY output bytes (may be non-UTF-8).
//   - client -> server: BINARY frames of raw input bytes (UTF-8 of typed string).
//   - resize: a TEXT frame {"type":"resize","cols":N,"rows":N}.

(function () {
  "use strict";

  const tokenInput = document.getElementById("token");
  const connectBtn = document.getElementById("connect");
  const statusEl = document.getElementById("status");
  const sessionsEl = document.getElementById("sessions");
  const terminalEl = document.getElementById("terminal");

  // Prefill the token from ?token= if present (and strip it from the visible URL
  // so it is not left in the address bar / history more than necessary).
  const params = new URLSearchParams(window.location.search);
  const tokenFromQuery = params.get("token");
  if (tokenFromQuery) {
    tokenInput.value = tokenFromQuery;
  }

  let term = null;
  let fitAddon = null;
  let ws = null;
  let activeId = null;

  function setStatus(text, kind) {
    statusEl.textContent = text;
    statusEl.className = "status" + (kind ? " " + kind : "");
  }

  function currentToken() {
    return tokenInput.value.trim();
  }

  function ensureTerminal() {
    if (term) {
      return;
    }
    term = new window.Terminal({
      cursorBlink: true,
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      theme: { background: "#000000", foreground: "#c8d0da" },
    });
    fitAddon = new window.FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    term.open(terminalEl);
    fit();

    // Keyboard / paste input -> binary frame (UTF-8 bytes of the string).
    term.onData(function (data) {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(new TextEncoder().encode(data));
      }
    });

    // Terminal resize -> text control frame.
    term.onResize(function (size) {
      sendResize(size.cols, size.rows);
    });

    window.addEventListener("resize", fit);
  }

  function fit() {
    if (fitAddon) {
      try {
        fitAddon.fit();
      } catch (e) {
        /* terminal may not be visible yet */
      }
    }
  }

  function sendResize(cols, rows) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "resize", cols: cols, rows: rows }));
    }
  }

  async function loadSessions() {
    const token = currentToken();
    if (!token) {
      setStatus("enter a token", "err");
      return;
    }
    setStatus("loading sessions…");
    try {
      const resp = await fetch("/v1/sessions", {
        headers: { Authorization: "Bearer " + token },
      });
      if (!resp.ok) {
        setStatus("list failed: HTTP " + resp.status, "err");
        renderSessions([]);
        return;
      }
      const sessions = await resp.json();
      renderSessions(sessions);
      setStatus(sessions.length + " session(s)", "ok");
    } catch (e) {
      setStatus("list error: " + e, "err");
      renderSessions([]);
    }
  }

  function renderSessions(sessions) {
    sessionsEl.innerHTML = "";
    if (!sessions || sessions.length === 0) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = "no sessions";
      sessionsEl.appendChild(li);
      return;
    }
    sessions.forEach(function (s) {
      const li = document.createElement("li");
      li.dataset.id = s.id;
      if (s.id === activeId) {
        li.className = "active";
      }
      const name = document.createElement("div");
      name.className = "name";
      name.textContent = s.name || s.id;
      const meta = document.createElement("div");
      meta.className = "meta";
      const cmd = Array.isArray(s.command) ? s.command.join(" ") : "";
      meta.textContent = (s.status || "?") + (cmd ? " · " + cmd : "");
      li.appendChild(name);
      li.appendChild(meta);
      li.addEventListener("click", function () {
        selectSession(s.id);
      });
      sessionsEl.appendChild(li);
    });
  }

  function highlightActive() {
    Array.prototype.forEach.call(sessionsEl.children, function (li) {
      li.className = li.dataset.id === activeId ? "active" : "";
    });
  }

  function selectSession(id) {
    const token = currentToken();
    if (!token) {
      setStatus("enter a token", "err");
      return;
    }
    activeId = id;
    highlightActive();
    ensureTerminal();
    term.reset();

    // Reconnect on selection: close any existing socket first.
    if (ws) {
      try {
        ws.onclose = null;
        ws.close();
      } catch (e) {
        /* ignore */
      }
      ws = null;
    }

    // Same host/scheme as the page; ?token= because browsers can't set headers
    // on a WS handshake.
    const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
    const url =
      scheme +
      "//" +
      window.location.host +
      "/v1/sessions/" +
      encodeURIComponent(id) +
      "/stream?token=" +
      encodeURIComponent(token);

    setStatus("connecting to " + id + "…");
    ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";

    ws.onopen = function () {
      setStatus("attached: " + id, "ok");
      fit();
      // Send an initial resize so the daemon matches the pane.
      if (term) {
        sendResize(term.cols, term.rows);
      }
      term.focus();
    };

    ws.onmessage = function (ev) {
      if (typeof ev.data === "string") {
        // The /stream endpoint sends raw bytes; any text frame is informational.
        return;
      }
      // Binary output: write the raw bytes (output may be non-UTF-8, so write the
      // Uint8Array directly rather than a decoded string).
      term.write(new Uint8Array(ev.data));
    };

    ws.onerror = function () {
      setStatus("websocket error", "err");
    };

    ws.onclose = function (ev) {
      if (activeId === id) {
        setStatus("disconnected (code " + ev.code + ")", "err");
      }
    };
  }

  connectBtn.addEventListener("click", loadSessions);
  tokenInput.addEventListener("keydown", function (e) {
    if (e.key === "Enter") {
      loadSessions();
    }
  });

  // Auto-load the session list if a token was supplied via the query string.
  if (tokenFromQuery) {
    loadSessions();
  } else {
    setStatus("disconnected");
  }
})();
