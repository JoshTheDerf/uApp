/* uapp client library for embedded apps.
 * Usage:
 *   <script src="/uapp.js"></script>
 *   const r = await uapp.query("SELECT * FROM jobs WHERE status=?", ["open"]);
 *   await uapp.exec("INSERT INTO jobs(title, created) VALUES(?,?)", [t, Date.now()]);
 *   uapp.onChange(() => refresh());   // fires when this app's data changes
 *
 * ACTIONS — shared business logic (REQUIRED pattern for uapp apps):
 *   uapp.action("add_employee", {
 *     description: "Add an employee. Returns {id}.",
 *     params: { name: {type: "string"}, hired: {type: "string", description: "YYYY-MM-DD"} },
 *   }, async ({name, hired}) => {
 *     await uapp.exec("INSERT INTO employees(name, hired) VALUES(?,?)", [name, hired]);
 *     return { ok: true };
 *   });
 *   // UI code calls it directly:            uapp.call("add_employee", {...})
 *   // The AI assistant calls the SAME code as tool app__add_employee.
 *   // Mark read-only actions with readonly: true (skips approval prompts).
 *
 * TOOLS — the app can call everything the AI can:
 *   await uapp.tool("import_csv", { file: "data/x.csv" });
 *   await uapp.tool("mcp__crm__lookup", { q: "..." });
 *
 * Writes commit straight into the .uapp file, one transaction each.
 * Never use datetime('now')/random() in exec/batch SQL — compute values in JS
 * and pass them as params.
 */
(() => {
  // The server auto-injects <script src="/uapp.js"> into app pages that don't
  // already load it. If a page loads it BOTH ways, bail on the second run so we
  // don't open two sockets or register the "app" context twice.
  if (window.uapp) return;
  // Browser (wasm) demo build: no server — this page talks to the shell (its
  // parent window) over postMessage; the shell forwards to the wasm worker.
  // The flag is injected into served pages by the shell's service-worker path.
  // Browser build: the shell page (boot.js) flags itself with __uappWasmHost;
  // a page in one of its frames — at any depth — finds it by walking up, and
  // that window is where RPCs go. Nothing is injected into the page for this,
  // so a site that stores rendered HTML can't pick up a stale marker.
  const wasmHost = (() => {
    try {
      let w = window;
      while (w.parent !== w) { w = w.parent; if (w.__uappWasmHost) return w; }
    } catch {} // a cross-origin ancestor: not ours
    return null;
  })();
  const wasmMode = !!wasmHost;
  // A hosted site's top-level page (served by `uapp serve`, which injects
  // /chrome.js ahead of everything else): there is no WebSocket server behind
  // it, and the editing shell is about to take this document over anyway. Do
  // not open a socket — the retry loop would otherwise hammer /ws every second
  // for as long as the tab lives. The API stays defined so the page's own
  // `uapp.onChange(...)`-style calls do not throw; RPCs simply never settle.
  const hostedTop = !wasmMode && window.top === window
    && !!document.querySelector('script[src^="/chrome.js"]');
  let ws, nextId = 1, openp;
  // One raw-send for both transports; JSON objects in, transport framing here.
  const rawSend = (obj) => {
    if (wasmMode) { wasmHost.postMessage({ __uappRPC: true, m: obj }, "*"); return true; }
    if (ws && ws.readyState === 1) { ws.send(JSON.stringify(obj)); return true; }
    return false;
  };
  const pending = new Map();
  const changeCbs = [];
  const fileCbs = [];
  const presentCbs = [];
  const actions = new Map(); // name -> {opts, handler}
  let regTimer = null;
  // Which eval context this page is: the app iframe ("app") or the shell's
  // hidden scratch iframe ("scratchpad", set by /scratch/ before this loads).
  const context = window.__uappContext || "app";

  // ---- WebGL contexts: release them when the page goes away ----------------
  // Chromium caps how many live WebGL contexts one renderer may hold, and a
  // discarded document's context is only released when GC gets around to it.
  // Editing an app reloads this page over and over, so contexts belonging to
  // pages nobody can reach any more pile up against that cap until the renderer
  // starts dropping the LIVE one and the canvas blanks. Android WebView has the
  // smallest budget of the lot, so it hits this first. Release them
  // deterministically on the way out instead of waiting for the collector.
  //
  // Contexts are recorded as they're handed out rather than by sweeping the DOM
  // at teardown, for two reasons: asking a context-less <canvas> for "webgl"
  // would CREATE one — allocating against the very cap we're trying to relieve
  // — and canvases an app keeps off-DOM would be missed entirely. Only contexts
  // created after this script runs are tracked, which is all of them: it's
  // injected at the top of <head> (see inject_viewport in server.rs).
  const glContexts = [];
  const teardownCbs = [];
  // WeakRef so tracking never keeps a context the app itself has dropped.
  const Ref = typeof WeakRef === "function"
    ? WeakRef
    : class { constructor(v) { this.v = v; } deref() { return this.v; } };
  if (typeof HTMLCanvasElement !== "undefined") {
    const realGetContext = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function (type, ...rest) {
      const ctx = realGetContext.call(this, type, ...rest);
      if (ctx && /webgl/i.test(String(type))) glContexts.push(new Ref(ctx));
      return ctx;
    };
  }
  // pagehide, not beforeunload: it fires however the document goes away, and it
  // doesn't disqualify the page from the back/forward cache the way unload does.
  window.addEventListener("pagehide", () => {
    // App teardown first — a renderer that disposes itself (three.js and
    // friends) frees more than the context alone.
    for (const cb of teardownCbs.splice(0)) { try { cb(); } catch {} }
    for (const ref of glContexts.splice(0)) {
      const gl = ref.deref();
      if (!gl || gl.isContextLost()) continue;
      try { gl.getExtension("WEBGL_lose_context")?.loseContext(); } catch {}
    }
  });

  // ---- console + error forwarding (for the chat-tab AI's read_console) -------
  // Patched at load so early logs are caught, buffered until the socket opens,
  // then streamed over the SAME /ws connection (method "log.write") — no second
  // socket. The server buffers these for the AI, which reads them with the
  // read_console tool; nothing here changes app behaviour (the real console
  // methods still run). `reset` is sent once per page load to open a fresh
  // console "generation" server-side, so a reload after an edit starts clean.
  const logQueue = [];
  let logResetSent = false;
  function logFmt(args) {
    return Array.prototype.map.call(args, (x) => {
      if (typeof x === "string") return x;
      if (x instanceof Error) return x.stack || x.message || String(x);
      try { return JSON.stringify(x); } catch { return String(x); }
    }).join(" ");
  }
  // Only the live app iframe forwards. The hidden scratchpad page loads uapp.js
  // too, but its run_js output is captured separately (handleEval) — letting it
  // forward would pollute read_console and hijack the "latest reload" filter.
  const forwardLogs = context === "app";
  function logSend(m) {
    if (!forwardLogs) return;
    if (wasmMode || (ws && ws.readyState === 1)) {
      try { rawSend({ method: "log.write", params: m }); } catch {}
    } else if (logQueue.length < 500) {
      logQueue.push(m);
    }
  }
  // Called from ws.onopen. Reset goes first (once) so queued lines land in the
  // new generation; reconnects of the same page keep their generation.
  function logFlush() {
    if (!forwardLogs) return;
    if (!logResetSent) { logResetSent = true; logSend({ level: "reset" }); }
    for (const m of logQueue.splice(0)) logSend(m);
  }
  if (forwardLogs) ["log", "info", "warn", "error", "debug"].forEach((lvl) => {
    const orig = console[lvl] ? console[lvl].bind(console) : () => {};
    console[lvl] = function () {
      try { logSend({ level: lvl, text: logFmt(arguments) }); } catch {}
      return orig.apply(console, arguments);
    };
  });
  window.addEventListener("error", (e) => {
    const s = e.error && e.error.stack;
    logSend({ level: "error", text: s ||
      ((e.message || "Error") + " at " + (e.filename || "") + ":" + e.lineno + ":" + e.colno) });
  });
  window.addEventListener("unhandledrejection", (e) => {
    const r = e.reason;
    logSend({ level: "error", text: "Unhandled promise rejection: " +
      ((r && r.stack) || (r && r.message) || logFmt([r])) });
  });

  function schemaFor(opts) {
    if (opts.schema) return opts.schema;
    const props = opts.params || {};
    const required = Object.entries(props)
      .filter(([, v]) => !v || v.required !== false)
      .map(([k]) => k);
    const properties = {};
    for (const [k, v] of Object.entries(props)) {
      const { required: _r, ...rest } = v || {};
      properties[k] = Object.keys(rest).length ? rest : { type: "string" };
    }
    return { type: "object", properties, required };
  }

  function registerActions() {
    clearTimeout(regTimer);
    regTimer = setTimeout(() => {
      if ((!wasmMode && (!ws || ws.readyState !== 1)) || actions.size === 0) return;
      rawSend({
        method: "actions.register",
        params: {
          actions: [...actions].map(([name, { opts }]) => ({
            name,
            description: (opts && opts.description) || "",
            readonly: !!(opts && opts.readonly),
            schema: schemaFor(opts || {}),
          })),
        },
      });
    }, 20);
  }

  // Host invocations (action.invoke / eval.invoke) still running when this
  // document goes away get an answer NOW, so the caller learns "the page
  // navigated/reloaded under me" at once instead of waiting out its timeout.
  const inflight = new Map(); // invoke id -> result method
  window.addEventListener("pagehide", () => {
    for (const [id, method] of inflight) {
      rawSend({ method, params: { id, error: "the page unloaded (a navigation or reload) before this finished — if the code navigated on purpose that is expected; inspect the new page with a fresh run_js" } });
    }
    inflight.clear();
    // Browser build: tell the host this document is gone, so the core forgets
    // its actions/contexts now instead of finding out on the next call.
    if (wasmMode) rawSend({ method: "conn.bye" });
  });
  function replyOnce(method, id) {
    inflight.set(id, method);
    return (params) => { if (inflight.delete(id)) rawSend({ method, params }); };
  }

  async function handleInvoke(p) {
    const send = replyOnce("actions.result", p.id);
    const def = actions.get(p.name);
    if (!def) return send({ id: p.id, error: `unknown action ${p.name}` });
    try {
      const result = await def.handler(p.input || {});
      send({ id: p.id, result: result === undefined ? null : result });
    } catch (e) {
      send({ id: p.id, error: String((e && e.message) || e) });
    }
  }

  /// Load a script tag and wait for it (handy for vendor libs from run_js).
  /// Use root-absolute paths ("/vendor/xlsx.full.min.js") — they resolve the
  /// same from the app iframe and the /scratch/ page.
  const loadScript = (src) => new Promise((res, rej) => {
    const s = document.createElement("script");
    s.src = src;
    s.onload = () => res(true);
    s.onerror = () => rej(new Error("failed to load script " + src));
    document.head.appendChild(s);
  });

  // The AI's run_js tool: execute code in this page, reply with the
  // completion value + captured console output.
  //
  // Two harness guarantees the code can rely on:
  //  * Top-level declarations (`function render() {}`, `const x = ...`) are
  //    exposed as page globals after the run, so event handlers, the page's
  //    own scripts, and later run_js calls can reach them.
  //  * Error line numbers refer to the code as written (the synthetic
  //    async-function wrapper's offset is corrected away).
  const EVAL_WRAPPER_OFFSET = 3; // 2 header lines from Function ctor + our "try {" line

  // Names of top-level declarations in the code, so we can hoist them to the
  // page scope. Best-effort regex (no full parse): function declarations and
  // simple const/let/var assignments at the start of a line. const/let are
  // rewritten to var so they stay visible to the finally-hoist (var is
  // function-scoped, const/let would be trapped in the try block).
  function declaredNames(code) {
    const names = new Set();
    const re = /(?:^|\n)[ \t]*(?:async[ \t]+)?function[ \t]+([A-Za-z_$][\w$]*)[ \t]*\(|(?:^|\n)[ \t]*(?:const|let|var)[ \t]+([A-Za-z_$][\w$]*)[ \t]*=/g;
    let m;
    while ((m = re.exec(code))) names.add(m[1] || m[2]);
    return [...names];
  }
  function varifyTopLevel(code) {
    return code.replace(/((?:^|\n)[ \t]*)(?:const|let)([ \t]+[A-Za-z_$][\w$]*[ \t]*=)/g, "$1var$2");
  }

  // Compile-time SyntaxErrors from the Function constructor carry no location.
  // Locate the offending line by compiling growing prefixes. A truncated
  // prefix fails on its own, so we first append the missing closing brackets
  // (scanner skips strings/comments); a prefix that STILL fails to compile
  // contains the real syntax error.
  function closeSnippet(src) {
    const stack = [];
    let i = 0, n = src.length;
    while (i < n) {
      const c = src[i];
      if (c === '"' || c === "'" || c === "`") {
        const q = c; i++;
        while (i < n && src[i] !== q) { if (src[i] === "\\") i++; if (q === "`" && src[i] === "$" && src[i+1] === "{") { /* skip naive: treat as text */ } i++; }
        i++; continue;
      }
      if (c === "/" && src[i+1] === "/") { while (i < n && src[i] !== "\n") i++; continue; }
      if (c === "/" && src[i+1] === "*") { i += 2; while (i < n && !(src[i] === "*" && src[i+1] === "/")) i++; i += 2; continue; }
      if ("([{".includes(c)) stack.push(c);
      else if (")]}".includes(c)) {
        const open = { ")": "(", "]": "[", "}": "{" }[c];
        if (stack.pop() !== open) return null; // closer without opener: broken
      }
      i++;
    }
    const close = { "(": ")", "[": "]", "{": "}" };
    return src + stack.reverse().map((c) => close[c]).join("");
  }

  function findSyntaxLine(AsyncFunction, code) {
    const lines = code.split("\n");
    if (lines.length > 400) return 0;
    for (let k = 1; k <= lines.length; k++) {
      const closed = closeSnippet(lines.slice(0, k).join("\n") + "\n");
      if (closed == null) return k; // unmatched closer on this line
      let bad = false;
      try { new AsyncFunction("uapp", "loadScript", closed); } catch { bad = true; }
      if (bad) return k;
    }
    return 0;
  }

  // Rewrite stack frames like "<anonymous>:5:7" so the line number matches
  // the code as the caller wrote it.
  function fixStackLines(stack) {
    return stack.replace(/(anonymous[^:\n]*):(\d+):(\d+)/g,
      (_, pre, ln, col) => `${pre}:${Math.max(1, Number(ln) - EVAL_WRAPPER_OFFSET)}:${col}`);
  }

  async function handleEval(p) {
    const send = replyOnce("eval.result", p.id);
    const logs = [];
    const orig = { log: console.log, warn: console.warn, error: console.error };
    const cap = (lvl) => (...a) => {
      try {
        logs.push((lvl === "log" ? "" : lvl + ": ") +
          a.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(" "));
      } catch {}
      orig[lvl].apply(console, a);
    };
    console.log = cap("log"); console.warn = cap("warn"); console.error = cap("error");
    try {
      const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
      // Wrap in try/finally so the hoist epilogue runs even when the code
      // returns early or throws: page-level declarations become reachable
      // from the page (event handlers, later run_js calls).
      const names = declaredNames(p.code);
      const hoist = names.map((n) =>
        `try{if(typeof ${n}!=="undefined"&&${n}!=null)globalThis[${JSON.stringify(n)}]=${n}}catch(e){}`).join(";");
      const wrapped = "try {\n" + varifyTopLevel(p.code) + "\n} finally {\n" + hoist + "\n}";
      let fn;
      try {
        fn = new AsyncFunction("uapp", "loadScript", wrapped);
      } catch (e) {
        // Compile error: give the caller a real line number + excerpt.
        const line = findSyntaxLine(AsyncFunction, p.code);
        let msg = `SyntaxError: ${(e && e.message) || e}`;
        if (line) {
          msg += ` (near line ${line} of your code)`;
          const lines = p.code.split("\n");
          const from = Math.max(0, line - 3), to = Math.min(lines.length, line + 2);
          msg += "\n" + lines.slice(from, to).map((l, i) =>
            `${String(from + i + 1).padStart(4)}${from + i + 1 === line ? " >" : " |"} ${l}`).join("\n");
        }
        send({ id: p.id, error: msg });
        return;
      }
      let value = await fn(window.uapp, loadScript);
      if (value === undefined) value = null;
      let json;
      try { json = JSON.stringify(value); } catch (e) { json = JSON.stringify("[unserializable: " + e.message + "] " + String(value)); }
      if (json === undefined) json = JSON.stringify(String(value));
      if (json.length > 200000) json = JSON.stringify(json.slice(0, 200000) + "…[truncated]");
      send({ id: p.id, result: { value: JSON.parse(json), logs: logs.slice(0, 200), exposed: names } });
    } catch (e) {
      const stack = String((e && e.stack) || (e && e.message) || e);
      let msg = fixStackLines(stack);
      if (!/line \d|:\d+:\d+/.test(msg) && e && e.message) msg = e.message + " (no location)";
      send({ id: p.id, error: logs.length ? msg + "\nconsole: " + logs.slice(0, 50).join("\n") : msg });
    } finally {
      console.log = orig.log; console.warn = orig.warn; console.error = orig.error;
    }
  }

  let reconnectTimer = null;
  // Reject in-flight calls when their socket dies — otherwise `await
  // uapp.query(...)` hangs forever after a background/resume drop.
  function failPending(why) {
    for (const [, pr] of pending) { try { pr.reject(new Error(why)); } catch {} }
    pending.clear();
  }
  function onTransportMessage(m) {
    if (m.method === "action.invoke") { handleInvoke(m.params || {}); return; }
    if (m.method === "eval.invoke") { handleEval(m.params || {}); return; }
    if (m.method === "event") {
      const p = m.params || {};
      if (p.type === "changes" && p.data) changeCbs.forEach((cb) => { try { cb(p); } catch {} });
      if (p.type === "changes" && p.files) fileCbs.forEach((cb) => { try { cb(p); } catch {} });
      if (p.type === "present") presentCbs.forEach((cb) => { try { cb(p); } catch {} });
      return;
    }
    const pr = pending.get(m.id);
    if (!pr) return;
    pending.delete(m.id);
    m.error ? pr.reject(new Error(m.error.message)) : pr.resolve(m.result);
  }
  // "This page has finished loading" (window load, or right away on a
  // reconnect of a page that already had). ctx.register goes out from the
  // <head>, before the page's own scripts ran; the AI's reload_app waits for
  // THIS, so a run_js/read_console right after it sees the new page, not the
  // old one or a half-initialized new one.
  function announceLoaded() {
    const send = () => rawSend({ method: "ctx.loaded", params: { context } });
    if (document.readyState === "complete") send();
    else window.addEventListener("load", send, { once: true });
  }
  function connect() {
    if (hostedTop) {
      // The server's plain page (no editor around it): there is no host to
      // talk to, so every call fails at once — never a silent forever-pending.
      const e = new Error("uapp: this page is not running inside the uapp editor/shell, so uapp.* calls are unavailable here");
      e.name = "UappNoHost";
      openp = Promise.reject(e);
      openp.catch(() => {});
      return;
    }
    if (wasmMode) {
      window.addEventListener("message", (ev) => {
        const d = ev.data;
        if (d && d.__uappMsg && d.m) onTransportMessage(d.m);
      });
      rawSend({ method: "ctx.register", params: { context } });
      announceLoaded();
      registerActions();
      logFlush();
      openp = Promise.resolve();
      return;
    }
    clearTimeout(reconnectTimer);
    // Drop any half-open leftover socket without letting its onclose queue a
    // second connect (see the visibilitychange note below).
    if (ws) { const old = ws; ws = null; old.onclose = null; try { old.close(); } catch {} failPending("reconnecting"); }
    openp = new Promise((resolve) => {
      ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`);
      ws.onopen = () => {
        ws.send(JSON.stringify({ method: "ctx.register", params: { context } }));
        announceLoaded();
        registerActions();
        logFlush();
        resolve();
      };
      ws.onmessage = (ev) => onTransportMessage(JSON.parse(ev.data));
      ws.onclose = () => { failPending("connection lost"); reconnectTimer = setTimeout(connect, 1000); };
    });
  }
  connect();

  // A backgrounded webview (Android suspends the whole page) drops the socket
  // AND freezes the reconnect timer above. On resume, reconnect immediately if
  // down; if the socket merely looks open, probe it — suspended pages often
  // leave half-open connections that never fire onclose.
  document.addEventListener("visibilitychange", () => {
    if (wasmMode || hostedTop) return; // parent-window transport never drops; no socket at all
    if (document.visibilityState !== "visible") return;
    if (!ws || ws.readyState !== 1) { connect(); return; }
    const id = nextId++;
    let alive = false;
    pending.set(id, { resolve: () => { alive = true; }, reject: () => { alive = true; } });
    try { ws.send(JSON.stringify({ id, method: "ping" })); } catch { connect(); return; }
    setTimeout(() => { if (!alive && ws && ws.readyState === 1) { try { ws.close(); } catch {} } }, 3000);
  });

  // No call pends forever: a lost reply (a transport hiccup, a host that went
  // away) surfaces as a typed, retryable error naming the call instead of a
  // button stuck on "Saving…". `uapp.rpcTimeout` (ms) tunes it per page.
  async function rpc(method, params = {}) {
    await openp;
    return new Promise((resolve, reject) => {
      const id = nextId++;
      const ms = Number(window.uapp && window.uapp.rpcTimeout) || 60000;
      const timer = setTimeout(() => {
        if (!pending.has(id)) return;
        pending.delete(id);
        const e = new Error(`uapp: ${method} (call #${id}) got no reply within ${Math.round(ms / 1000)}s — the host may be busy or the reply was lost; retrying is safe for reads`);
        e.name = "UappTimeout"; e.method = method; e.callId = id;
        reject(e);
      }, ms);
      pending.set(id, {
        resolve: (v) => { clearTimeout(timer); resolve(v); },
        reject: (e) => { clearTimeout(timer); reject(e); },
      });
      rawSend({ id, method, params });
    });
  }

  // Rows come off the wire as arrays; every consumer in app code wants
  // objects keyed by column name. Map once, here, so no module ever sees the
  // array shape (the classic silent-undefined bug).
  function shapeRows(r) {
    if (r && Array.isArray(r.rows) && Array.isArray(r.columns)) {
      const cols = r.columns;
      r.rows = r.rows.map((arr) => {
        const o = {};
        cols.forEach((c, i) => { o[c] = arr[i]; });
        return o;
      });
    }
    return r;
  }

  const api = {
    rpc,
    query: async (sql, params = []) => shapeRows(await rpc("sql.query", { sql, params })),
    exec: async (sql, params = []) => {
      const r = shapeRows(await rpc("sql.exec", { sql, params }));
      // The server reports the rowid a write created (null when it didn't),
      // under both the wire name and the familiar JS casing.
      if (r && typeof r === "object" && "insert_id" in r) r.insertId = r.insert_id;
      return r;
    },
    batch: (sql) => rpc("sql.batch", { sql }),
    readFile: (name) => rpc("files.read", { name }),
    writeFile: (name, text) => rpc("files.write", { name, text }),
    listFiles: () => rpc("files.list"),
    importCsv: (opts) => rpc("import.csv", opts),
    fetchUrl: (url, name) => rpc("files.fetch", { url, name }),
    /// Register a named business-logic action (see header comment).
    action: (name, opts, handler) => {
      actions.set(name, { opts: opts || {}, handler });
      registerActions();
    },
    /// Call one of this app's own actions locally (same code the AI calls).
    call: async (name, input = {}) => {
      const def = actions.get(name);
      if (!def) throw new Error(`unknown action ${name}`);
      return def.handler(input);
    },
    /// Call any tool the AI has: builtins (sql_query, import_csv, download_lib…),
    /// MCP tools (mcp__server__tool), or app actions (app__name).
    tool: (name, input = {}) => rpc("tools.call", { name, input }),
    tools: () => rpc("tools.list"),
    onChange: (cb) => changeCbs.push(cb),
    /// Run `cb` just before this page goes away (an edit reloads it). Dispose
    /// renderers here — `renderer.dispose()` for three.js — so the GPU
    /// resources go with the page instead of outliving it; the WebGL context
    /// itself is released for you either way.
    onTeardown: (cb) => teardownCbs.push(cb),
    /// Run `cb` as soon as `el` is actually visible (right away if it already
    /// is). Use this before initializing grid/table/chart widgets: most of
    /// them (Tabulator included) compute sizes at init and silently render 0
    /// rows inside a display:none container.
    whenVisible: (el, cb) => {
      if (!el || typeof IntersectionObserver === "undefined") return void cb();
      if (el.offsetWidth || el.offsetHeight || el.getClientRects().length) return void cb();
      const io = new IntersectionObserver((entries) => {
        if (entries.some((e) => e.isIntersecting)) { io.disconnect(); cb(); }
      });
      io.observe(el);
    },
    onFilesChange: (cb) => fileCbs.push(cb),
    /// Ask every open shell to show a file from the archive (the shell opens its
    /// viewer). mode: "auto" | "raw" | "pretty" | "edit".
    presentFile: (name, mode) => rpc("files.present", { name, mode }),
    /// Observe presents — an app can react instead of leaving it to the shell.
    onPresent: (cb) => presentCbs.push(cb),
    user: null,
    device: null,
    ready: (async () => {
      const info = await rpc("app.info");
      api.user = info.user;
      api.device = info.device;
      return info;
    })(),
  };
  window.uapp = api;
})();
