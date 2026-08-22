/* uapp browser build — main-thread bootstrap and glue.
 *
 * Order of operations:
 *   1. Register the service worker (archive URLs + COOP/COEP for isolation).
 *   2. Start the wasm worker, hand it the SharedArrayBuffer bridge.
 *   3. Open the requested stored app, or the launcher — itself a .uapp with
 *      a grid of the OPFS app library and the samples (host.* API).
 *   4. Install window.__uappTransport and load the normal shell.
 *
 * At runtime this file is also: the router between app iframes (uapp.js over
 * postMessage) and the worker; the responder for service-worker archive
 * fetches; the main-thread end of the SAB bridge (run_js, app actions,
 * approvals, console); and the auto-saver.
 */

const enc = new TextEncoder();
const APPS_DIR = "apps";
const LEGACY_FILE = "app.uapp";
// The demo can be hosted under a subpath (e.g. /uapp/demo/) — everything we
// load or register is relative to where boot.js itself lives.
const BASE = new URL("./", import.meta.url);
window.__uappBase = BASE.pathname;

// sessionStorage throws (not returns null) when the browser blocks site data,
// and it is load-bearing here: it carries which app to open across the reload
// that switches documents, and guards the one-time cross-origin-isolation
// reload. Probe it once; ssOk === false means "don't count on a reload
// remembering anything".
const memSS = new Map();
let ssOk = true;
try {
  sessionStorage.setItem("uapp.probe", "1");
  sessionStorage.removeItem("uapp.probe");
} catch { ssOk = false; }
function ssGet(k) { try { return ssOk ? sessionStorage.getItem(k) : (memSS.has(k) ? memSS.get(k) : null); } catch { return null; } }
function ssSet(k, v) { try { ssOk ? sessionStorage.setItem(k, v) : memSS.set(k, v); } catch {} }
function ssDel(k) { try { ssOk ? sessionStorage.removeItem(k) : memSS.delete(k); } catch {} }

// ---- boot splash ------------------------------------------------------------
// index.html paints it on every load; we keep it up until the app frame is
// showing. Switching documents is a full reload and the samples are real
// downloads (the CAD one is 8 MB), so without this the page just sits there
// looking stalled after a click.

const splashEl = document.getElementById("boot-splash");
const splashStatus = document.getElementById("boot-status");
const splashDetail = document.getElementById("boot-detail");
const splashBar = document.getElementById("boot-bar");
let splashHidden = false;

const mb = (n) => (n / 1048576).toFixed(1) + " MB";

function splashShow(status, detail) {
  if (!splashEl) return;
  splashHidden = false;
  splashEl.style.display = "";
  splashEl.classList.remove("gone");
  if (status) splashStatus.textContent = status;
  splashBar.classList.remove("determinate");
  splashBar.style.width = "";
  splashDetail.textContent = detail || "";
}
// Detail line only — keeps whatever the status says (usually the app name).
function splashNote(detail) {
  if (splashEl) splashDetail.textContent = detail || "";
}
function splashProgress(loaded, total) {
  if (!splashEl) return;
  if (total && loaded <= total) {
    splashBar.classList.add("determinate");
    splashBar.style.width = Math.round((loaded / total) * 100) + "%";
    splashDetail.textContent = `${mb(loaded)} of ${mb(total)}`;
  } else {
    // No content-length (or a compressed transfer, where it undercounts):
    // back to the indeterminate bar and just report what has arrived.
    splashBar.classList.remove("determinate");
    splashBar.style.width = "";
    splashDetail.textContent = mb(loaded) + " downloaded";
  }
}
function splashHide() {
  if (!splashEl || splashHidden) return;
  splashHidden = true;
  splashEl.classList.add("gone");
  setTimeout(() => { if (splashHidden) splashEl.style.display = "none"; }, 300);
}

// A document switch is a reload, so name the app we are heading for instead of
// showing a generic "Starting" (the boot below consumes these keys).
{
  const pendingName = ssGet("uapp-open-name");
  if (pendingName) splashStatus.textContent = "Opening " + pendingName + "…";
}

// Download with a real progress bar when the server gives us a length.
async function fetchWithProgress(url, what) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`could not fetch ${what} (HTTP ${r.status})`);
  const total = Number(r.headers.get("content-length")) || 0;
  if (!r.body || !r.body.getReader) return new Uint8Array(await r.arrayBuffer());
  const reader = r.body.getReader();
  const chunks = [];
  let loaded = 0;
  splashProgress(0, total);
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.length;
    splashProgress(loaded, total);
  }
  const out = new Uint8Array(loaded);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.length; }
  return out;
}

let worker = null;
let sab = null;
// Resolved once the worker has an app open — every archive fetch and iframe
// RPC waits on it, so nothing races the open (or a document switch).
let appReadyResolve;
const appReady = new Promise((r) => { appReadyResolve = r; });
// True when another tab took over the demo (see the BroadcastChannel below):
// this tab stops answering service-worker fetches and stops saving.
let zombie = false;
let workerSeq = 1;
const workerPending = new Map(); // rpc id -> {resolve, reject}
let shellOnMessage = null;       // core.js's handler
const clientWindows = new Set(); // iframe windows that spoke to us
const ctxRegistry = [];          // [{ctx, win}] push-ordered
const iframeActions = new Map(); // win -> actions array
const invokes = new Map();       // invoke id -> resolve

// ---- tiny helpers -----------------------------------------------------------

function b64FromBytes(bytes) {
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}
function bytesFromB64(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}

const CTYPES = {
  html: "text/html; charset=utf-8", htm: "text/html; charset=utf-8",
  js: "text/javascript; charset=utf-8", mjs: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8", json: "application/json", svg: "image/svg+xml",
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", ico: "image/x-icon", avif: "image/avif", csv: "text/csv",
  txt: "text/plain; charset=utf-8", md: "text/markdown; charset=utf-8",
  markdown: "text/markdown; charset=utf-8", mp4: "video/mp4", webm: "video/webm",
  ogv: "video/ogg", mov: "video/quicktime", m4v: "video/x-m4v", mkv: "video/x-matroska",
  mp3: "audio/mpeg", wav: "audio/wav", m4a: "audio/mp4", flac: "audio/flac",
  ogg: "audio/ogg", woff2: "font/woff2", woff: "font/woff", ttf: "font/ttf",
  wasm: "application/wasm", map: "application/json", pdf: "application/pdf",
};
function contentTypeFor(name) {
  const ext = name.split(".").pop().toLowerCase();
  return CTYPES[ext] || "application/octet-stream";
}

// Same idea as the native server's inject_viewport: make sure app pages get a
// viewport, the wasm-mode flag, and uapp.js — flag BEFORE uapp.js.
function injectHtml(text) {
  const lower = text.toLowerCase();
  const hasVp = lower.includes('name="viewport"') || lower.includes("name='viewport'");
  const hasUapp = lower.includes("uapp.js");
  let inject = "\n<script>window.__uappWasm=1</script>";
  if (!hasVp) inject += '\n<meta name="viewport" content="width=device-width, initial-scale=1">';
  if (!hasUapp) inject += '\n<script src="/uapp.js"></script>';
  const pos = lower.indexOf("<head");
  if (pos >= 0) {
    const gt = text.indexOf(">", pos);
    if (gt >= 0) return text.slice(0, gt + 1) + inject + text.slice(gt + 1);
  }
  return inject + "\n" + text;
}

// ---- worker RPC -------------------------------------------------------------

function sendRpc(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = "b" + workerSeq++;
    workerPending.set(id, { resolve, reject });
    worker.postMessage({ type: "rpc", id, method, params });
  });
}

// ---- console buffer (main-thread copy of App's ring buffer) ------------------

const CONSOLE_CAP = 500;
const consoleBuf = [];
let consoleSeq = 0, consoleGen = 0, consoleAck = 0;
function consolePush(level, text) {
  if (level === "reset") { consoleGen++; return; }
  consoleBuf.push({ seq: ++consoleSeq, gen: consoleGen, level, text: String(text).slice(0, 8000), ts: Date.now() });
  while (consoleBuf.length > CONSOLE_CAP) consoleBuf.shift();
}
function consoleRead(onlyErrors, onlyLatest, limit) {
  let rows = consoleBuf.filter((e) =>
    (!onlyLatest || e.gen === consoleGen) &&
    (!onlyErrors || e.level === "error" || e.level === "warn"));
  const total = rows.length;
  if (rows.length > limit) rows = rows.slice(rows.length - limit);
  consoleAck = Math.max(consoleAck, consoleSeq);
  return {
    entries: rows.map((e) => ({ level: e.level, text: e.text, gen: e.gen, ts: e.ts })),
    dropped: total - rows.length,
    latest_gen: consoleGen,
  };
}
function consoleAlert() {
  let errs = 0, warns = 0, others = 0, top = consoleAck;
  for (const e of consoleBuf) {
    if (e.seq <= consoleAck) continue;
    if (e.level === "error") errs++; else if (e.level === "warn") warns++; else others++;
    top = Math.max(top, e.seq);
  }
  if (!errs && !warns && !others) return null;
  consoleAck = top;
  const s = (n) => (n === 1 ? "" : "s");
  const parts = [];
  if (errs) parts.push(`${errs} error${s(errs)}`);
  if (warns) parts.push(`${warns} warning${s(warns)}`);
  if (others) parts.push(`${others} log line${s(others)}`);
  return `\n\n[console] ${parts.join(", ")} in the live app since you last checked — call read_console to see the messages${errs ? " and stack traces" : ""}.`;
}

// ---- SAB bridge (worker blocked mid-AI-run asks us for things) ---------------

function bridgeReply(obj) {
  if (!sab) return;
  const i32 = new Int32Array(sab);
  let bytes = enc.encode(JSON.stringify(obj));
  const cap = sab.byteLength - 16;
  if (bytes.length > cap) {
    bytes = enc.encode(JSON.stringify({ error: "bridge reply too large (" + bytes.length + " bytes)" }));
  }
  new Uint8Array(sab, 16, bytes.length).set(bytes);
  Atomics.store(i32, 1, bytes.length);
  Atomics.store(i32, 0, 1);
  Atomics.notify(i32, 0);
}

function invokeInContext(context, msg, timeoutMs = 60000) {
  return new Promise((resolve) => {
    let win = null;
    for (let i = ctxRegistry.length - 1; i >= 0; i--) {
      if (ctxRegistry[i].ctx === context) { win = ctxRegistry[i].win; break; }
    }
    if (!win) {
      resolve({ error: `no '${context}' page is connected` });
      return;
    }
    const id = "inv" + workerSeq++;
    const timer = setTimeout(() => {
      if (invokes.has(id)) { invokes.delete(id); resolve({ error: "the page did not respond within " + timeoutMs / 1000 + "s" }); }
    }, timeoutMs);
    invokes.set(id, (params) => {
      clearTimeout(timer);
      if (params.error != null) resolve({ error: String(params.error) });
      else resolve({ result: params.result === undefined ? null : params.result });
    });
    msg.params.id = id;
    try { win.postMessage({ __uappMsg: true, m: msg }, "*"); } catch (e) {
      invokes.delete(id); clearTimeout(timer);
      resolve({ error: "the page is gone: " + e });
    }
  });
}

// The one in-flight bridge prompt (the worker is blocked, so there is never
// more than one). The shell answers it with its normal ai.approve/ai.answer
// RPCs, which the transport intercepts below.
let bridgePrompt = null; // {kind: "approval"|"question", id}

function emitShellEvent(type, extra) {
  const envelope = { method: "event", params: { type, ...extra } };
  if (shellOnMessage) shellOnMessage(envelope);
  for (const win of clientWindows) {
    try { win.postMessage({ __uappMsg: true, m: envelope }, "*"); } catch {}
  }
}

// Answer the pending bridge prompt (from an intercepted RPC or a stop).
function resolveBridgePrompt(reply) {
  if (!bridgePrompt) return false;
  const kind = bridgePrompt.kind;
  bridgePrompt = null;
  emitShellEvent(kind === "approval" ? "approval" : "question", { pending: [] });
  bridgeReply(reply);
  return true;
}

async function handleBridge(kind, payloadJson) {
  let p = {};
  try { p = JSON.parse(payloadJson); } catch {}
  if (kind === "console.read") {
    bridgeReply(consoleRead(!!p.only_errors, p.only_latest !== false, Math.min(Math.max(p.limit || 100, 1), 500)));
    return;
  }
  if (kind === "console.alert") {
    bridgeReply({ note: consoleAlert() || "" });
    return;
  }
  if (kind === "eval") {
    bridgeReply(await invokeInContext(p.context || "scratchpad", { method: "eval.invoke", params: { code: p.code || "" } }));
    return;
  }
  if (kind === "action") {
    bridgeReply(await invokeInContext("app", { method: "action.invoke", params: { name: p.name, input: p.input || {} } }));
    return;
  }
  if (kind === "approval") {
    // Show the shell's normal approval card; its ai.approve RPC is
    // intercepted by the transport and answered into the SAB.
    bridgePrompt = { kind: "approval", id: p.id };
    emitShellEvent("approval", { pending: [{
      id: p.id, name: p.name, input: p.input, note: p.note || null,
      session: p.session, created: Date.now(),
    }] });
    return;
  }
  if (kind === "ask_user") {
    // Same pattern for ask_user question cards (answered via ai.answer).
    bridgePrompt = { kind: "question", id: p.id };
    emitShellEvent("question", { pending: [{
      id: p.id, questions: p.questions, session: p.session, created: Date.now(),
    }] });
    return;
  }
  bridgeReply({ error: "unknown bridge request: " + kind });
}

// ---- service-worker archive requests -----------------------------------------

async function handleSwRequest(m) {
  if (zombie) return; // another tab owns the demo — let it answer
  const reply = (data) => navigator.serviceWorker.controller &&
    navigator.serviceWorker.controller.postMessage({ swReply: true, id: m.id, ...data });
  try {
    await appReady;
    const url = new URL(m.path, location.origin);
    const p = url.pathname;
    if (p === "/download.uapp") {
      const r = await sendRpc("app.export");
      const name = (appName() || "app").replace(/[^\w \-.]/g, "_");
      reply({ bodyB64: r.b64, contentType: "application/octet-stream",
              disposition: `attachment; filename="${name}.uapp"` });
      return;
    }
    if (p === "/template.uapp") {
      const r = await sendRpc("app.exportTemplate");
      const name = (r.name || "app").replace(/[^\w \-.]/g, "_");
      reply({ bodyB64: r.b64, contentType: "application/octet-stream",
              disposition: `attachment; filename="${name} template.uapp"` });
      return;
    }
    if (p === "/upload") {
      const name = url.searchParams.get("name") || "";
      const r = await sendRpc("files.write", { name, b64: m.bodyB64 || "" });
      reply({ bodyB64: b64FromBytes(enc.encode(JSON.stringify({ ok: true, name: r.name }))),
              contentType: "application/json" });
      return;
    }
    if (p === "/scratch/" || p === "/scratch") {
      const html = await (await fetch(new URL("scratch.html", BASE))).text();
      // The context flag must land before uapp.js connects.
      const patched = html.replace('window.__uappContext = "scratchpad";',
        'window.__uappContext = "scratchpad"; window.__uappWasm = 1;');
      reply({ bodyB64: b64FromBytes(enc.encode(patched)), contentType: "text/html; charset=utf-8" });
      return;
    }
    // Archive file. Like the native /app/* route, the /app/ URL prefix is a
    // ROUTE, not part of the archive name: /app/<ref> serves <ref>, where a
    // bare ref resolves under app/ (viewer URLs pass full archive names like
    // app/index.html or data/sales.csv after the prefix). Other paths
    // (/data/x, /vendor/x, root-absolute refs) are used as-is.
    let name;
    if (p === "/app" || p === "/app/") name = "app/index.html";
    else if (p.startsWith("/app/")) name = decodeURIComponent(p.slice(5));
    else name = decodeURIComponent(p.slice(1));
    const r = await sendRpc("files.read", { name });
    const ctype = contentTypeFor(r.name || name);
    let bodyB64 = r.b64;
    if (ctype.startsWith("text/html")) {
      bodyB64 = b64FromBytes(enc.encode(injectHtml(new TextDecoder().decode(bytesFromB64(r.b64)))));
    }
    reply({ bodyB64, contentType: ctype });
  } catch (e) {
    reply({ error: String((e && e.message) || e), status: 404 });
  }
}

// ---- iframe (uapp.js) routing -------------------------------------------------

function rebuildActionsSync() {
  const merged = [];
  for (const list of iframeActions.values()) merged.push(...list);
  sendRpc("actions.sync", { actions: merged }).catch(() => {});
}

window.addEventListener("message", (ev) => {
  const d = ev.data;
  if (!d || !d.__uappRPC || !d.m) return;
  const m = d.m;
  const win = ev.source;
  clientWindows.add(win);
  const replyTo = (obj) => { try { win.postMessage({ __uappMsg: true, m: obj }, "*"); } catch {} };
  if (m.method === "ctx.register") {
    const ctx = (m.params && m.params.context) || "";
    if (ctx && ctx.length <= 32) {
      for (let i = ctxRegistry.length - 1; i >= 0; i--) {
        if (ctxRegistry[i].win === win && ctxRegistry[i].ctx === ctx) ctxRegistry.splice(i, 1);
      }
      ctxRegistry.push({ ctx, win });
      if (ctx === "app") consolePush("reset", ""); // new page load, new generation
    }
    if (m.id != null) replyTo({ id: m.id, result: { ok: true } });
    return;
  }
  if (m.method === "actions.register") {
    iframeActions.set(win, (m.params && m.params.actions) || []);
    rebuildActionsSync();
    if (m.id != null) replyTo({ id: m.id, result: { ok: true } });
    return;
  }
  if (m.method === "log.write") {
    consolePush((m.params && m.params.level) || "log", (m.params && m.params.text) || "");
    return;
  }
  if (m.method.startsWith("host.")) {
    handleHostRpc(m).then(
      (result) => { if (m.id != null) replyTo({ id: m.id, result }); },
      (e) => { if (m.id != null) replyTo({ id: m.id, error: { message: String((e && e.message) || e) } }); },
    );
    return;
  }
  if (m.method === "actions.result" || m.method === "eval.result") {
    const cb = invokes.get(m.params && m.params.id);
    if (cb) { invokes.delete(m.params.id); cb(m.params); }
    if (m.id != null) replyTo({ id: m.id, result: { ok: true } });
    return;
  }
  // Everything else goes to the worker; route the reply back to this iframe
  // (waiting for the app to be open, so early ctx/app.info calls never race).
  appReady.then(() => sendRpc(m.method, m.params || {})).then(
    (result) => { if (m.id != null) replyTo({ id: m.id, result }); },
    (e) => { if (m.id != null) replyTo({ id: m.id, error: { message: String((e && e.message) || e) } }); },
  );
});

// ---- demo host API (used by the launcher .uapp) --------------------------------
// Lets an app manage the OPFS app library and switch documents. It's a demo:
// every app gets it (there is no cross-user data to protect here).

async function handleHostRpc(m) {
  const p = m.params || {};
  switch (m.method) {
    case "host.apps":
      return { current: currentAppId, apps: appsIndex().sort((a, b) => b.updated - a.updated) };
    case "host.open": {
      const e = appsIndex().find((a) => a.id === p.id);
      if (!e) throw new Error("no stored app with that id");
      splashShow("Opening " + e.name + "…");
      setTimeout(() => switchTo(e.id, e.name), 50); // reply first, then reload
      return { ok: true };
    }
    case "host.create": {
      const id = genId();
      const name = (p.name || "New App").slice(0, 80);
      splashShow("Creating " + name + "…");
      indexTouch(id, name);
      setTimeout(() => switchTo(id, p.name), 50);
      return { ok: true, id };
    }
    case "host.import": {
      const bytes = bytesFromB64(p.b64 || "");
      if (bytes.length < 16 || String.fromCharCode(...bytes.slice(0, 15)) !== "SQLite format 3") {
        throw new Error("not a plain .uapp file (encrypted apps can't be opened in the browser demo)");
      }
      const name = (p.name || "Imported app").replace(/\.uapp$/i, "").slice(0, 80);
      splashShow("Opening " + name + "…", "Saving…");
      const id = genId();
      // A failed write (storage blocked) must not leave the splash covering
      // the launcher the error message is about.
      try { await appWrite(id, bytes); } catch (e) { splashHide(); throw e; }
      indexTouch(id, name);
      setTimeout(() => switchTo(id, p.name), 50);
      return { ok: true, id };
    }
    case "host.sample": {
      const name = (p.name || "Sample").slice(0, 80);
      // Several megabytes for some samples: show the bar before the request
      // goes out, and put the splash away again if it fails.
      splashShow("Opening " + name + "…", "Downloading…");
      let bytes;
      try {
        bytes = await fetchWithProgress(
          new URL(String(p.url || "").replace(/^\//, ""), BASE), "the sample");
      } catch (e) {
        splashHide();
        throw e;
      }
      splashShow("Opening " + name + "…", "Saving…");
      const id = genId();
      // A failed write (storage blocked) must not leave the splash covering
      // the launcher the error message is about.
      try { await appWrite(id, bytes); } catch (e) { splashHide(); throw e; }
      indexTouch(id, name);
      setTimeout(() => switchTo(id, p.name), 50);
      return { ok: true, id };
    }
    case "host.delete":
      await appDelete(p.id);
      return { ok: true };
    case "host.export": {
      const bytes = await appRead(p.id);
      if (!bytes) throw new Error("no stored app with that id");
      const e = appsIndex().find((a) => a.id === p.id);
      return { b64: b64FromBytes(bytes), name: (e && e.name) || "app" };
    }
    default:
      throw new Error("unknown host method " + m.method);
  }
}

// ---- events from the worker ----------------------------------------------------

let lastInfoName = "";
function appName() { return lastInfoName; }

let saveTimer = null, saving = false, saveDirty = false;
async function appSave() {
  if (!currentAppId || zombie) return; // launcher is ephemeral; dormant tabs must not write
  if (saving) { saveDirty = true; return; }
  saving = true;
  try {
    const r = await sendRpc("app.export");
    await appWrite(currentAppId, bytesFromB64(r.b64));
    indexTouch(currentAppId, lastInfoName);
  } catch (e) {
    console.warn("uapp: auto-save failed:", e);
  } finally {
    saving = false;
    if (saveDirty) { saveDirty = false; scheduleSave(); }
  }
}
function scheduleSave() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(appSave, 1500);
}

function onWorkerEvent(envelope) {
  const params = envelope.params || {};
  // Closing the document (the shell's ✕ / app.quit) goes back to the landing
  // chooser: save one last time, then reload with the chooser forced on.
  if (params.type === "sync" && params.state === "closed") {
    (async () => {
      clearTimeout(saveTimer);
      try { await appSave(); } catch {}
      switchTo(null); // back to the launcher
    })();
    return; // don't let the shell paint its "closed" overlay first
  }
  if (params.type === "changes") scheduleSave();
  if (params.type === "renamed" && params.name) {
    lastInfoName = params.name;
    if (currentAppId) indexTouch(currentAppId, params.name);
  }
  if (params.type === "open_url" && params.url) {
    try { window.open(params.url, "_blank", "noopener"); } catch {}
  }
  if (shellOnMessage) shellOnMessage(envelope);
  for (const win of clientWindows) {
    try { win.postMessage({ __uappMsg: true, m: envelope }, "*"); } catch {}
  }
}

// ---- app library (many .uapp files) -----------------------------------------
// Index in localStorage: [{id, name, updated}]. Bytes in OPFS apps/<id>.uapp,
// or in IndexedDB where the browser won't give us OPFS (see pickBlobStore).
// currentAppId === null means the launcher (ephemeral, never saved).

let currentAppId = null;

// localStorage *throws* (rather than returning null) when the browser blocks
// site data for the origin, so everything goes through a shim that keeps the
// index in memory for the life of the tab instead.
const memKV = new Map();
function kvGet(key) {
  try { return localStorage.getItem(key); } catch { return memKV.has(key) ? memKV.get(key) : null; }
}
function kvSet(key, val) {
  try { localStorage.setItem(key, val); } catch { memKV.set(key, val); }
}

function appsIndex() {
  try { return JSON.parse(kvGet("uapp.demo.apps")) || []; } catch { return []; }
}
function saveIndex(list) {
  kvSet("uapp.demo.apps", JSON.stringify(list));
}
function indexTouch(id, name) {
  const list = appsIndex();
  const e = list.find((a) => a.id === id);
  if (e) { if (name) e.name = name; e.updated = Date.now(); }
  else list.push({ id, name: name || "App", updated: Date.now() });
  saveIndex(list);
}

// ---- where the .uapp bytes live ----------------------------------------------
// OPFS is the good backend, but it is not always on offer: Firefox rejects
// navigator.storage.getDirectory() with "Security error when calling
// GetDirectory" in private windows and whenever site data is blocked for the
// origin, and older Safari has no writable OPFS. So probe once and degrade:
// OPFS -> IndexedDB (which does work in a Firefox private window) -> nothing,
// where opening an app fails with a message the visitor can act on.

const errText = (e) => String((e && e.message) || e);

async function opfsDir() {
  const root = await navigator.storage.getDirectory();
  return root.getDirectoryHandle(APPS_DIR, { create: true });
}

const opfsStore = {
  name: "OPFS",
  persistent: true,
  async read(id) {
    const f = await (await (await opfsDir()).getFileHandle(id + ".uapp")).getFile();
    return f.size > 0 ? new Uint8Array(await f.arrayBuffer()) : null;
  },
  async write(id, bytes) {
    const fh = await (await opfsDir()).getFileHandle(id + ".uapp", { create: true });
    const w = await fh.createWritable();
    await w.write(bytes);
    await w.close();
  },
  async remove(id) { await (await opfsDir()).removeEntry(id + ".uapp"); },
};

const IDB_NAME = "uapp.demo", IDB_STORE = "apps";
function idbOpen() {
  return new Promise((resolve, reject) => {
    let req;
    try { req = indexedDB.open(IDB_NAME, 1); } catch (e) { reject(e); return; }
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(IDB_STORE)) db.createObjectStore(IDB_STORE);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error || new Error("indexedDB.open failed"));
    req.onblocked = () => reject(new Error("indexedDB.open blocked"));
  });
}
async function idbRun(mode, fn) {
  const db = await idbOpen();
  try {
    return await new Promise((resolve, reject) => {
      const tx = db.transaction(IDB_STORE, mode);
      const req = fn(tx.objectStore(IDB_STORE));
      tx.oncomplete = () => resolve(req ? req.result : undefined);
      tx.onabort = () => reject(tx.error || new Error("transaction aborted"));
      if (req) req.onerror = () => reject(req.error);
    });
  } finally { try { db.close(); } catch {} }
}

const idbStore = {
  name: "IndexedDB",
  persistent: true,
  async read(id) {
    const v = await idbRun("readonly", (s) => s.get(id));
    if (!v) return null;
    const u8 = v instanceof Uint8Array ? v : new Uint8Array(v);
    return u8.length > 0 ? u8 : null;
  },
  write(id, bytes) { return idbRun("readwrite", (s) => s.put(bytes.slice(), id)); },
  remove(id) { return idbRun("readwrite", (s) => s.delete(id)); },
};

// Nothing left to write to. Reads come back empty and writes say why, which
// beats "opening" an app whose bytes silently vanish on the next reload (every
// document switch here is a reload).
const BLOCKED_MSG =
  "this browser is blocking storage for the site — a private window, or " +
  "cookies/site data blocked for it. Allow site data (or use a normal window) " +
  "to open apps in the demo.";
const blockedStore = {
  name: "none",
  persistent: false,
  async read() { return null; },
  async write() { throw new Error(BLOCKED_MSG); },
  async remove() {},
};

let blobStoreP = null;
function blobStore() { return (blobStoreP = blobStoreP || pickBlobStore()); }

async function pickBlobStore() {
  try {
    // Probe a real write: a handle can be granted and the write still refused.
    const dir = await opfsDir();
    const fh = await dir.getFileHandle(".probe", { create: true });
    const w = await fh.createWritable();
    await w.write(new Uint8Array([0]));
    await w.close();
    try { await dir.removeEntry(".probe"); } catch {}
    return opfsStore;
  } catch (e) {
    console.warn("uapp: OPFS unavailable (" + errText(e) + ") — falling back to IndexedDB");
  }
  try {
    (await idbOpen()).close();
    return idbStore;
  } catch (e) {
    console.warn("uapp: IndexedDB unavailable (" + errText(e) + ") — apps will not be saved");
  }
  storageNotice();
  return blockedStore;
}

// Shown once when nothing can be persisted — the app-open failures below are
// otherwise a bare error message with no way to act on it.
let noticeShown = false;
function storageNotice() {
  if (noticeShown) return;
  noticeShown = true;
  const el = document.createElement("div");
  el.style.cssText = "position:fixed;left:12px;right:12px;bottom:12px;margin:0 auto;max-width:520px;z-index:999998;background:#2b3145;border:1px solid #3a4260;border-radius:12px;padding:14px 16px;color:#f2f4f8;font:13px/1.55 system-ui,sans-serif;box-shadow:0 10px 30px #0007";
  el.innerHTML = `<b>This browser is blocking storage for the site</b><br>
    Usually a private window, or cookies and site data blocked for it. The demo
    keeps a document per app in your browser's own storage, so opening one
    can't work here — allow site data for this site, or try a normal window.`;
  const btn = document.createElement("button");
  btn.textContent = "Got it";
  btn.style.cssText = "margin-top:10px;padding:7px 14px;border:none;border-radius:8px;background:#3f6de6;color:#fff;font:inherit;font-weight:600;cursor:pointer";
  btn.onclick = () => el.remove();
  el.appendChild(btn);
  document.body.appendChild(el);
}

async function appRead(id) {
  try { return await (await blobStore()).read(id); } catch { return null; }
}
async function appWrite(id, bytes) {
  return (await blobStore()).write(id, bytes);
}
async function appDelete(id) {
  try { await (await blobStore()).remove(id); } catch {}
  saveIndex(appsIndex().filter((a) => a.id !== id));
}
const genId = () => "a" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);

// One-time migration of the single-app storage from earlier demo builds.
async function migrateLegacy() {
  try {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(LEGACY_FILE);
    const f = await fh.getFile();
    if (f.size > 0) {
      const id = genId();
      await appWrite(id, new Uint8Array(await f.arrayBuffer()));
      indexTouch(id, kvGet("uapp.demo.name") || "App");
    }
    await root.removeEntry(LEGACY_FILE);
  } catch {}
}

// Switch documents by reloading the page — the cleanest reset of the worker,
// iframes and shell state. sessionStorage carries what to open next.
function switchTo(id, name) {
  if (id) {
    ssSet("uapp-open-app", id);
    if (name) ssSet("uapp-open-name", name);
  } else {
    ssDel("uapp-open-app");
    ssDel("uapp-open-name");
    splashShow("Returning to your apps…"); // covers the reload gap
  }
  location.reload();
}

function openApp(bytes, name) {
  return new Promise((resolve, reject) => {
    const id = "open" + workerSeq++;
    const handler = (ev) => {
      const m = ev.data || {};
      if (m.type === "opened" && m.id === id) {
        worker.removeEventListener("message", handler);
        m.error ? reject(new Error(m.error)) : resolve(m.info);
      }
    };
    worker.addEventListener("message", handler);
    worker.postMessage({ type: "open", id, bytes: bytes ? bytes.buffer : null, name },
      bytes ? [bytes.buffer] : []);
  });
}

function fatal(msg) {
  document.body.innerHTML = `<div style="max-width:520px;margin:80px auto;font:15px/1.6 system-ui;color:#eee;background:#2b3145;padding:24px;border-radius:12px"><b>uapp browser demo could not start</b><br>${msg}</div>`;
  document.body.style.background = "#23293a";
}

// ---- single active tab -------------------------------------------------------
// The service worker broadcasts archive fetches to every window of the demo;
// with two tabs open, both would answer and the faster (possibly WRONG) app
// would win — files from one app, scripts from another. So the demo is
// single-tab: the newest boot broadcasts a takeover and older tabs go dormant.

const BOOT_ID = Date.now() + "-" + Math.random().toString(36).slice(2);
// Constructing it throws where the browser blocks site data for the origin,
// and this is module top level: an unhandled throw here leaves the page on the
// splash forever, so the takeover check simply goes away instead.
let tabChannel = null;
try {
  if ("BroadcastChannel" in window) tabChannel = new BroadcastChannel("uapp-demo-tab");
} catch (e) {
  console.warn("uapp: BroadcastChannel unavailable (" + errText(e) + ")");
}

function goDormant() {
  if (zombie) return;
  zombie = true;
  clearTimeout(saveTimer);
  try { worker && worker.terminate(); } catch {}
  const div = document.createElement("div");
  div.style.cssText = "position:fixed;inset:0;z-index:999999;display:flex;align-items:center;justify-content:center;background:#23293acc;backdrop-filter:blur(3px);color:#f2f4f8;font:15px/1.5 system-ui,sans-serif";
  div.innerHTML = `<div style="background:#2b3145;border:1px solid #3a4260;border-radius:14px;padding:26px;max-width:360px;text-align:center">
    <div style="font-size:30px">👋</div>
    <h1 style="font-size:17px;margin:8px 0 4px">Demo opened in another tab</h1>
    <p style="color:#aeb6c8;font-size:13px;margin:0 0 16px">The uapp demo runs in one tab at a time (one process owns the file, like the desktop app).</p>
    <button style="padding:10px 18px;border:none;border-radius:9px;background:#3f6de6;color:#fff;font:inherit;font-weight:600;cursor:pointer" onclick="location.reload()">Use it here instead</button>
  </div>`;
  document.body.appendChild(div);
}

if (tabChannel) {
  tabChannel.onmessage = (ev) => {
    if (ev.data && ev.data.takeover && ev.data.bootId !== BOOT_ID) goDormant();
  };
}

// ---- boot ------------------------------------------------------------------------

(async () => {
  try {
    if (tabChannel) tabChannel.postMessage({ takeover: true, bootId: BOOT_ID });
    if (!("serviceWorker" in navigator)) {
      fatal("This browser has no service-worker support (or the page is not served over HTTPS/localhost).");
      return;
    }
    splashNote("Starting the service worker…");
    try {
      await navigator.serviceWorker.register(new URL("sw.js", BASE));
      await navigator.serviceWorker.ready;
    } catch (e) {
      // Blocking cookies/site data for the origin blocks this too, and without
      // the service worker there is no way to serve an app's files at all.
      fatal(e && e.name === "SecurityError"
        ? "This browser is blocking storage for the site, so the demo's service worker can't start. Allow cookies and site data for this site (or try a normal window) and reload."
        : "The demo's service worker could not start: " + errText(e));
      return;
    }
    if (!navigator.serviceWorker.controller || (!crossOriginIsolated && ssOk && !ssGet("uapp-coi-reload"))) {
      ssSet("uapp-coi-reload", "1");
      location.reload();
      return;
    }
    navigator.serviceWorker.addEventListener("message", (ev) => {
      if (ev.data && ev.data.swRequest) handleSwRequest(ev.data);
    });

    sab = crossOriginIsolated ? new SharedArrayBuffer(4 * 1024 * 1024) : null;
    if (!sab) console.warn("uapp: not cross-origin isolated — run_js/app-action tools are disabled");

    splashNote("Loading the runtime…"); // ~1.5 MB of wasm on a cold cache
    worker = new Worker(new URL("worker.js", BASE), { type: "module" });
    worker.addEventListener("message", (ev) => {
      const m = ev.data || {};
      if (m.type === "reply") {
        const p = workerPending.get(m.id);
        if (p) {
          workerPending.delete(m.id);
          m.error ? p.reject(new Error(m.error.message || "error")) : p.resolve(m.result);
        }
        return;
      }
      if (m.type === "event") {
        try { onWorkerEvent(JSON.parse(m.json)); } catch {}
        return;
      }
      if (m.type === "bridge") { handleBridge(m.kind, m.payload); return; }
      if (m.type === "fatal") fatal(m.error);
    });
    await new Promise((resolve, reject) => {
      const h = (ev) => {
        if (ev.data && ev.data.type === "ready") { worker.removeEventListener("message", h); resolve(); }
        if (ev.data && ev.data.type === "fatal") reject(new Error(ev.data.error));
      };
      worker.addEventListener("message", h);
      worker.postMessage({ type: "init", sab });
    });

    await migrateLegacy();

    // Open the requested stored app, or the launcher (itself a .uapp).
    let info = null;
    const wantId = ssGet("uapp-open-app");
    const wantName = ssGet("uapp-open-name") || "App";
    ssDel("uapp-open-app");
    ssDel("uapp-open-name");
    if (wantId) {
      splashShow("Opening " + wantName + "…", "Opening the document…");
      const bytes = await appRead(wantId); // null = brand-new app (host.create)
      try {
        info = await openApp(bytes, wantName);
        currentAppId = wantId;
        if (!bytes) scheduleSave(); // persist the fresh app right away
      } catch (e) {
        alert("Could not open the app: " + (e.message || e));
      }
    }
    if (!info) {
      currentAppId = null;
      splashShow("Starting uapp…", "Loading the app library…");
      const bytes = await fetchWithProgress(new URL("launcher.uapp", BASE), "the launcher app");
      info = await openApp(bytes, "Apps");
    }
    lastInfoName = info.name || "uapp";
    document.title = `${lastInfoName} — uapp`;
    appReadyResolve();

    // The shell's transport (core.js checks window.__uappTransport).
    window.__uappTransport = {
      send(obj) {
        const localReply = (result) =>
          setTimeout(() => shellOnMessage && shellOnMessage({ id: obj.id, result }), 0);
        // Prompts raised over the SAB bridge are answered HERE — the worker is
        // blocked inside the run, so these RPCs must not be queued behind it.
        if (bridgePrompt && obj.method === "ai.approve" && obj.params && obj.params.id === bridgePrompt.id) {
          resolveBridgePrompt({ allow: !!obj.params.allow, always: !!obj.params.always });
          return localReply({ ok: true });
        }
        if (bridgePrompt && obj.method === "ai.answer" && obj.params && obj.params.id === bridgePrompt.id) {
          resolveBridgePrompt({ answers: obj.params.answers || [] });
          return localReply({ ok: true });
        }
        if (obj.method === "ai.stop") {
          if (sab) Atomics.store(new Int32Array(sab), 2, 1); // reach a blocked run now
          resolveBridgePrompt(bridgePrompt && bridgePrompt.kind === "question"
            ? { stopped: true } : { allow: false, always: false });
        }
        sendRpc(obj.method, obj.params || {}).then(
          (result) => shellOnMessage && shellOnMessage({ id: obj.id, result }),
          (e) => shellOnMessage && shellOnMessage({ id: obj.id, error: { message: String((e && e.message) || e) } }),
        );
      },
      onMessage(fn) { shellOnMessage = fn; },
    };

    // The iframes in index.html start empty: their archive URLs only resolve
    // once the service worker controls the page and an app is open. Point them
    // at the real (base-relative) paths now.
    splashNote("Loading the app…");
    const appFrame = document.getElementById("appframe");
    // Hold the splash until the app has actually painted, with a cap in case
    // the frame never fires load.
    const framePainted = new Promise((resolve) => {
      if (!appFrame) return resolve();
      appFrame.addEventListener("load", resolve, { once: true });
      setTimeout(resolve, 8000);
    });
    for (const [id, path] of [["appframe", "app/"], ["scratchframe", "scratch/"]]) {
      const f = document.getElementById(id);
      // No cache-buster: these are served no-store, and a stable url lets
      // later reloads replace this history entry instead of pushing a new one
      // (see reloadAppFrame in shell/main.js).
      if (f) f.src = BASE.pathname + path;
    }

    await import(new URL("shell/main.js", BASE));
    await framePainted;
    splashHide();
  } catch (e) {
    console.error(e);
    fatal(String((e && e.message) || e));
  }
})();
