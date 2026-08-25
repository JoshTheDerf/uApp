/* uapp browser build — worker-side glue imported by the wasm module.
 * Provides the platform pieces the Rust core can't have on wasm: wall-clock
 * time, HTTP, event forwarding to the page, AI-run scheduling, and the
 * SharedArrayBuffer bridge that lets the worker ask the main thread for things
 * mid-AI-run (run_js, app actions, approvals, console reads).
 *
 * The core's AI loop is synchronous: it blocks this worker for a whole turn.
 * Everything it blocks ON goes through `blockUntil`, which — instead of a bare
 * Atomics.wait — keeps servicing the page's RPCs from a shared-memory inbox
 * (re-entering `rpc_dispatch`) while it waits. So the app iframe, the shell
 * and the service worker keep working during a run: run_js code can query
 * the database, the frame can load its files, cards render as they happen.
 *
 * Shared buffers (all Int32Array-viewed, control ints first, payload from byte 32):
 *   sab   [0] bridge reply state (1 = ready)  [1] reply length
 *         [2] stop flag (shell's Stop button) [3] WAKE counter (any producer)
 *         [4] BLOCKED (1 while this worker is inside blockUntil)
 *   inbox [0] state (1 = an RPC is waiting)   [1] length   — page → worker
 *   net   [0] state (1 = a record is ready)   [1] length   [2] flags (1 eof, 2 error)
 */

const cfg = { sab: null, inbox: null, net: null, notify: () => {}, aiTick: null, dispatch: null };
export function glueConfigure(c) { Object.assign(cfg, c); }

const WAKE = 3, BLOCKED = 4;
const HDR = 32; // control ints live in the first 32 bytes; payloads follow

export function js_now_ms() { return Date.now(); }

export function js_notify(json) { try { cfg.notify(json); } catch {} }

// ---- inbox: page RPCs delivered through shared memory ----------------------

// Take the queued RPC (if any) out of the inbox and dispatch it. Called from
// the wait loop and from worker.js on a {kick} message. Re-entrant into wasm
// on purpose: the core holds no lock across a blocking call.
export function glueDrainInbox() {
  if (!cfg.inbox || !cfg.dispatch) return false;
  const ib = new Int32Array(cfg.inbox);
  let did = false;
  while (Atomics.load(ib, 0) === 1) {
    const len = Atomics.load(ib, 1);
    const bytes = new Uint8Array(cfg.inbox, HDR, len).slice();
    Atomics.store(ib, 0, 0); // slot free — the page may write the next one
    did = true;
    let m = null;
    try { m = JSON.parse(new TextDecoder().decode(bytes)); } catch { continue; }
    try { cfg.dispatch(m); } catch (e) { console.error("inbox dispatch failed:", e); }
  }
  return did;
}

let depth = 0;
// Wait until pred() holds (or timeoutMs passes → false), servicing the inbox
// the whole time. Producers bump sab[WAKE] and notify after writing anything
// we might be waiting for, so a single wait cell covers all of them.
function blockUntil(pred, timeoutMs) {
  const ctrl = new Int32Array(cfg.sab);
  depth++;
  Atomics.store(ctrl, BLOCKED, 1);
  const deadline = Date.now() + timeoutMs;
  try {
    for (;;) {
      glueDrainInbox();
      const seen = Atomics.load(ctrl, WAKE);
      if (pred()) return true;
      if (cfg.inbox && Atomics.load(new Int32Array(cfg.inbox), 0) === 1) continue;
      const left = deadline - Date.now();
      if (left <= 0) return false;
      Atomics.wait(ctrl, WAKE, seen, Math.min(left, 1000));
    }
  } finally {
    if (--depth === 0) Atomics.store(ctrl, BLOCKED, 0);
    // A request written between the last drain and BLOCKED=0 is picked up by
    // the {kick} message the page sends alongside every inbox write.
  }
}

// ---- bridge: ask the main thread (run_js, actions, prompts, console) --------

let bridgeBusy = false;
export function js_bridge_call(kind, payloadJson) {
  if (!cfg.sab) {
    return JSON.stringify({ error: "bridge unavailable (page is not cross-origin isolated — run_js/app actions need COOP/COEP headers)" });
  }
  if (bridgeBusy) return JSON.stringify({ error: "bridge busy (nested page call from inside a page call)" });
  bridgeBusy = true;
  try {
    const i32 = new Int32Array(cfg.sab);
    Atomics.store(i32, 0, 0);
    postMessage({ type: "bridge", kind, payload: payloadJson });
    if (!blockUntil(() => Atomics.load(i32, 0) === 1, 300000)) {
      return JSON.stringify({ error: "bridge timeout (no reply from the page within 5 minutes)" });
    }
    const len = Atomics.load(i32, 1);
    const cap = cfg.sab.byteLength - HDR;
    const bytes = new Uint8Array(cfg.sab, HDR, Math.min(len, cap)).slice();
    return new TextDecoder().decode(bytes);
  } finally {
    bridgeBusy = false;
  }
}

// ---- HTTP: fetch on the main thread, body streamed through `net` -----------

let netSeq = 1;
let netBusy = 0;           // id of the request whose record we're waiting for
const xhrBodies = new Map(); // fallback mode: id -> whole body (Uint8Array)

function netTake() {
  const n = new Int32Array(cfg.net);
  const len = Atomics.load(n, 1), flags = Atomics.load(n, 2);
  const bytes = new Uint8Array(cfg.net, HDR, Math.min(len, cfg.net.byteLength - HDR)).slice();
  Atomics.store(n, 0, 0);
  return { bytes, flags };
}

// Wait for the main thread to fill the net slot for `id`.
function netAwait(id, timeoutMs) {
  const n = new Int32Array(cfg.net);
  if (!blockUntil(() => Atomics.load(n, 0) === 1, timeoutMs)) {
    postMessage({ type: "http.close", id });
    return null;
  }
  return netTake();
}

export function js_http_open(method, url, headersJson, body, connectTimeoutMs) {
  if (!cfg.sab || !cfg.net) return xhrOpen(method, url, headersJson, body);
  if (netBusy) return JSON.stringify({ error: "an HTTP request is already in flight on this worker" });
  const id = netSeq++;
  let headers = [];
  try { headers = JSON.parse(headersJson); } catch {}
  Atomics.store(new Int32Array(cfg.net), 0, 0);
  netBusy = id;
  postMessage({ type: "http.open", id, method, url, headers, body: body == null ? null : body });
  const rec = netAwait(id, Math.max(connectTimeoutMs || 0, 1000));
  if (!rec) { netBusy = 0; return JSON.stringify({ error: "connect timeout" }); }
  let r = {};
  try { r = JSON.parse(new TextDecoder().decode(rec.bytes)); } catch { r = { error: "bad open record" }; }
  if (r.error) { netBusy = 0; return JSON.stringify(r); }
  r.id = id;
  return JSON.stringify(r);
}

export function js_http_read(id, readTimeoutMs) {
  if (xhrBodies.has(id)) {
    const b = xhrBodies.get(id);
    xhrBodies.set(id, new Uint8Array(0));
    return b;
  }
  if (netBusy !== id) return new Uint8Array(0); // closed or foreign: EOF
  postMessage({ type: "http.read", id });
  const rec = netAwait(id, Math.max(readTimeoutMs || 0, 1000));
  if (!rec) { netBusy = 0; throw "read timeout (no data from the provider)"; }
  if (rec.flags & 2) { netBusy = 0; throw new TextDecoder().decode(rec.bytes) || "network failure"; }
  if (rec.flags & 1) { netBusy = 0; return rec.bytes; } // last chunk (may be empty)
  return rec.bytes;
}

export function js_http_close(id) {
  xhrBodies.delete(id);
  if (netBusy === id) netBusy = 0;
  postMessage({ type: "http.close", id });
}

// No SharedArrayBuffer (page not cross-origin isolated): sync XHR, whole body
// at once — the pre-isolation behaviour, still correct, just not incremental.
function xhrOpen(method, url, headersJson, body) {
  try {
    const xhr = new XMLHttpRequest();
    xhr.open(method, url, false);
    xhr.overrideMimeType("text/plain; charset=x-user-defined");
    for (const [k, v] of JSON.parse(headersJson)) {
      try { xhr.setRequestHeader(k, v); } catch {}
    }
    xhr.send(body == null ? null : body);
    if (xhr.status === 0) return JSON.stringify({ error: "network/CORS failure (status 0)" });
    const headers = xhr.getAllResponseHeaders().trim().split(/\r?\n/)
      .map((l) => { const i = l.indexOf(":"); return i > 0 ? [l.slice(0, i).trim(), l.slice(i + 1).trim()] : null; })
      .filter(Boolean);
    const s = xhr.responseText || "";
    const bytes = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) bytes[i] = s.charCodeAt(i) & 0xff;
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return JSON.stringify({
      id: 0,
      status: xhr.status,
      contentType: xhr.getResponseHeader("content-type") || "",
      headers,
      bodyB64: btoa(bin),
    });
  } catch (e) {
    return JSON.stringify({ error: String((e && e.message) || e) });
  }
}

// ---- scheduling / stop -----------------------------------------------------

export function js_schedule_ai(session) {
  // A fresh macrotask, so the RPC that triggered the run replies first.
  setTimeout(() => { try { cfg.aiTick && cfg.aiTick(session); } catch (e) { console.error("ai_tick failed:", e); } }, 0);
}

export function js_stop_requested() {
  return cfg.sab ? Atomics.load(new Int32Array(cfg.sab), 2) === 1 : false;
}

export function js_clear_stop() {
  if (cfg.sab) Atomics.store(new Int32Array(cfg.sab), 2, 0);
}
