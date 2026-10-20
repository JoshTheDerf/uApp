/* uapp browser build — the main-thread end of the wasm worker: starting it,
 * RPCs to it (postMessage, or the shared-memory inbox while an AI run has it
 * blocked), provider HTTP on its behalf, the approval/question prompts it
 * raises over the SharedArrayBuffer bridge, and the shell's transport. */

import { BASE, BUILD, enc, updatedError } from "./env.js?v=__UAPP_BUILD__";
import { broadcastToFrames, deliverToConn } from "./frame-router.js?v=__UAPP_BUILD__";
import { splashNote } from "./splash.js?v=__UAPP_BUILD__";

let worker = null;
let sab = null;    // bridge control + reply (see uapp_glue.js for the layout)
let inbox = null;      // page → worker RPCs while the worker is blocked in an AI run
let net = null;        // provider HTTP records, main-thread fetch → worker
const WAKE = 3, BRIDGE_CALL = 5, HDR = 32; // same layout as uapp_glue.js
const RUNTIME_TIMEOUT_MS = 90000;
function wake() {
  const c = new Int32Array(sab);
  Atomics.add(c, WAKE, 1);
  Atomics.notify(c, WAKE);
}
// Resolved once the worker has an app open — every archive fetch and iframe
// RPC waits on it, so nothing races the open (or a document switch).
let appReadyResolve, appReadyReject;
export const appReady = new Promise((res, rej) => { appReadyResolve = res; appReadyReject = rej; });
appReady.catch(() => {}); // rejected only when boot fails; dependents get their own rejection
export let appIsReady = false;
export function markAppReady() { appIsReady = true; appReadyResolve(); }
export function failAppReady(e) { appReadyReject(e instanceof Error ? e : new Error(String(e))); }
// True once this page no longer owns its document (another tab took it
// over, the hosted-site chrome stood down, a reset is under way): it stops
// answering service-worker fetches and stops saving.
export let zombie = false;
export function markZombie() { zombie = true; }
let workerSeq = 1;
const workerPending = new Map(); // rpc id -> {resolve, reject}
let shellOnMessage = null;       // core.js's handler

// Start the worker and wait until it is ready for an app. `onEvent` gets the
// core's events, `onDead` a crash after boot (see worker.js markDead).
export async function startWorker({ onEvent, onDead }) {
  sab = crossOriginIsolated ? new SharedArrayBuffer(4 * 1024 * 1024) : null;
  if (!sab) console.warn("uapp: not cross-origin isolated — run_js/app-action tools are disabled");
  inbox = sab ? new SharedArrayBuffer(4 * 1024 * 1024) : null;
  net = sab ? new SharedArrayBuffer(1024 * 1024 + HDR) : null;

  splashNote("Loading the runtime…"); // ~1.5 MB of wasm on a cold cache
  worker = new Worker(new URL("worker.js" + (BUILD.startsWith("__") ? "" : "?v=" + BUILD), BASE), { type: "module" });
  // A module worker that fails to load (a 404, a syntax error, a network
  // drop mid-download) says so only here, and never posts "ready".
  let workerFailed = null;
  const workerFailure = new Promise((_, reject) => { workerFailed = reject; });
  workerFailure.catch(() => {});
  worker.addEventListener("error", (ev) => {
    const why = (ev && ev.message) || "the worker script failed to load";
    if (!appIsReady) workerFailed(new Error("The runtime could not load (" + why + ")."));
    else console.error("uapp: worker error:", why);
  });
  worker.addEventListener("message", (ev) => {
    const m = ev.data || {};
    if (m.type === "reply") {
      const p = workerPending.get(m.id);
      if (p) {
        workerPending.delete(m.id);
        m.error ? p.reject(new Error(m.error.message || "error")) : p.resolve(m.result);
      }
      if (inbox && inboxQueue.length) flushInbox(); // the worker freed the slot
      return;
    }
    if (netHandle(m)) return;
    if (m.type === "event") {
      try { onEvent(JSON.parse(m.json)); } catch {}
      return;
    }
    if (m.type === "bridge") { handleBridge(m.kind, m.payload, m.call); return; }
    if (m.type === "bridge.cancel") { cancelBridgePrompt(m.call); return; }
    if (m.type === "conn.send") { deliverToConn(m.conn, m.msg); return; }
    if (m.type === "fatal" && !appIsReady) workerFailed(m.updated ? updatedError() : new Error(m.error));
    if (m.type === "dead") {
      if (appIsReady) onDead(m.message);
      else workerFailed(new Error("The engine crashed while starting: " + m.message));
    }
  });
  await Promise.race([
    new Promise((resolve, reject) => {
      const h = (ev) => {
        const d = ev.data || {};
        if (d.type === "ready") {
          worker.removeEventListener("message", h);
          // A worker.js from another deploy than this boot.js: they
          // would talk past each other.
          if (d.build && d.build !== BUILD && !BUILD.startsWith("__")) reject(updatedError());
          else resolve();
        }
      };
      worker.addEventListener("message", h);
      worker.postMessage({ type: "init", sab, inbox, net });
    }),
    workerFailure,
    // ~1.5 MB of wasm: a slow connection fits in this, a hung one does not.
    new Promise((_, reject) => setTimeout(() => reject(new Error(
      "Loading the runtime took too long. Check your connection and try again.")), RUNTIME_TIMEOUT_MS)),
  ]);
}

// Stop the engine for good (dormant tab, editor stood down): no more
// fetches answered from it, no more saves, and nobody left waiting.
export function stopWorker(why) {
  zombie = true;
  try { worker && worker.terminate(); } catch {}
  failWorkerPending(why);
}

// ---- worker RPC -------------------------------------------------------------

export function sendRpc(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = "b" + workerSeq++;
    workerPending.set(id, { resolve, reject });
    const msg = { type: "rpc", id, method, params };
    // ONE path whenever shared memory exists: the inbox. A worker blocked in
    // an AI run drains it from its wait loop; an idle worker drains it on the
    // {kick}; a worker that is running synchronously (between two waits)
    // finds it at its next wait or kick. Sampling "is it blocked right now"
    // and using postMessage otherwise stranded replies behind the whole run
    // and could reorder messages between the two paths.
    if (inbox) {
      inboxQueue.push(msg);
      flushInbox();
      return;
    }
    worker.postMessage(msg);
  });
}

// Engine gone (dormant tab, editor stood down, fatal): fail every waiting
// caller now instead of leaving promises that can never settle.
function failWorkerPending(why) {
  for (const [, p] of workerPending) { try { p.reject(new Error(why)); } catch {} }
  workerPending.clear();
}

const inboxQueue = [];
function flushInbox() {
  const ib = new Int32Array(inbox);
  while (inboxQueue.length && Atomics.load(ib, 0) === 0) {
    const msg = inboxQueue.shift();
    const bytes = enc.encode(JSON.stringify(msg));
    if (bytes.length > inbox.byteLength - HDR) { worker.postMessage(msg); continue; } // too big: queue behind the run
    new Uint8Array(inbox, HDR, bytes.length).set(bytes);
    Atomics.store(ib, 1, bytes.length);
    Atomics.store(ib, 0, 1);
    wake();
    worker.postMessage({ type: "kick" });
  }
}

// ---- provider HTTP on behalf of the worker ---------------------------------
// The worker can't fetch asynchronously while its AI loop is blocked, so it
// posts {http.open}/{http.read} and waits on the `net` slot; we fetch here and
// hand over the headers, then the body one chunk at a time (a live stream, so
// SSE deltas render as they arrive).

const netStreams = new Map(); // id -> {chunks: [], done, error, reader, wanted}
function netWrite(bytes, flags) {
  const n = new Int32Array(net);
  new Uint8Array(net, HDR, bytes.length).set(bytes);
  Atomics.store(n, 1, bytes.length);
  Atomics.store(n, 2, flags);
  Atomics.store(n, 0, 1);
  wake();
}
function netServe(id) {
  const s = netStreams.get(id);
  if (!s || !s.wanted) return;
  const cap = net.byteLength - HDR;
  if (s.chunks.length) {
    let c = s.chunks[0];
    if (c.length > cap) { s.chunks[0] = c.subarray(cap); c = c.subarray(0, cap); } else s.chunks.shift();
    s.wanted = false;
    netWrite(c, s.chunks.length === 0 && s.done && !s.error ? 1 : 0);
    if (s.done && !s.chunks.length) netStreams.delete(id);
    return;
  }
  if (s.error) { s.wanted = false; netWrite(enc.encode(String(s.error)), 2); netStreams.delete(id); return; }
  if (s.done) { s.wanted = false; netWrite(new Uint8Array(0), 1); netStreams.delete(id); }
}
async function netOpen(m) {
  const s = { chunks: [], done: false, error: null, reader: null, wanted: false, ctrl: new AbortController() };
  netStreams.set(m.id, s);
  let resp;
  try {
    const headers = {};
    for (const [k, v] of m.headers || []) headers[k] = v;
    resp = await fetch(m.url, { method: m.method, headers, body: m.body == null ? undefined : m.body, signal: s.ctrl.signal, cache: "no-store" });
  } catch (e) {
    netStreams.delete(m.id);
    netWrite(enc.encode(JSON.stringify({ error: "network/CORS failure: " + (e && e.message || e) })), 0);
    return;
  }
  const hdrs = [];
  resp.headers.forEach((v, k) => hdrs.push([k, v]));
  netWrite(enc.encode(JSON.stringify({ status: resp.status, contentType: resp.headers.get("content-type") || "", headers: hdrs })), 0);
  if (!resp.body) { s.done = true; netServe(m.id); return; }
  s.reader = resp.body.getReader();
  try {
    for (;;) {
      const { value, done } = await s.reader.read();
      if (done) break;
      if (value && value.length) s.chunks.push(value);
      netServe(m.id);
    }
    s.done = true;
  } catch (e) {
    if (!s.ctrl.signal.aborted) s.error = "read failed: " + (e && e.message || e);
    s.done = true;
  }
  netServe(m.id);
}
function netHandle(m) {
  if (m.type === "http.open") { netOpen(m); return true; }
  if (m.type === "http.read") { const s = netStreams.get(m.id); if (s) { s.wanted = true; netServe(m.id); } else netWrite(new Uint8Array(0), 1); return true; }
  if (m.type === "http.close") { const s = netStreams.get(m.id); if (s) { try { s.ctrl.abort(); } catch {} netStreams.delete(m.id); } return true; }
  return false;
}

// ---- bridge prompts -------------------------------------------------------------

// `call` is the id the worker gave the request; it only reads a reply that
// carries it (see js_bridge_call in uapp_glue.js).
function bridgeReply(obj, call) {
  if (!sab) return;
  const i32 = new Int32Array(sab);
  let bytes = enc.encode(JSON.stringify(obj));
  const cap = sab.byteLength - HDR;
  if (bytes.length > cap) {
    bytes = enc.encode(JSON.stringify({ error: "bridge reply too large (" + bytes.length + " bytes)" }));
  }
  new Uint8Array(sab, HDR, bytes.length).set(bytes);
  Atomics.store(i32, 1, bytes.length);
  Atomics.store(i32, BRIDGE_CALL, call | 0);
  Atomics.store(i32, 0, 1);
  wake(); // the worker waits on the shared WAKE cell (see blockUntil)
}

// The scratch frame starts empty and is loaded on first use (same reasoning as
// the native shell: an idle second document costs memory a phone needs for the
// app). The core asks for it with a "scratch-load" event, which the shell
// (main.js) answers — nothing to do here.

// The one in-flight bridge prompt (the worker is blocked, so there is never
// more than one). The shell answers it with its normal ai.approve/ai.answer
// RPCs, which the transport intercepts below.
let bridgePrompt = null; // {kind: "approval"|"question", id, call}

function emitShellEvent(type, extra) {
  deliverEvent({ method: "event", params: { type, ...extra } });
}

// Answer the pending bridge prompt (from an intercepted RPC or a stop).
function resolveBridgePrompt(reply) {
  if (!bridgePrompt) return false;
  const { kind, call } = bridgePrompt;
  bridgePrompt = null;
  emitShellEvent(kind === "approval" ? "approval" : "question", { pending: [] });
  bridgeReply(reply, call);
  return true;
}

// The worker gave up waiting (js_bridge_call timed out): take the card down
// so nobody answers a question that is no longer being asked.
function cancelBridgePrompt(call) {
  if (!bridgePrompt || bridgePrompt.call !== call) return;
  const kind = bridgePrompt.kind;
  bridgePrompt = null;
  emitShellEvent(kind === "approval" ? "approval" : "question", { pending: [] });
}

async function handleBridge(kind, payloadJson, call) {
  let p = {};
  try { p = JSON.parse(payloadJson); } catch {}
  if (kind === "approval") {
    // Show the shell's normal approval card; its ai.approve RPC is
    // intercepted by the transport and answered into the SAB.
    bridgePrompt = { kind: "approval", id: p.id, call };
    emitShellEvent("approval", { pending: [{
      id: p.id, name: p.name, input: p.input, note: p.note || null,
      session: p.session, created: Date.now(),
    }] });
    return;
  }
  if (kind === "ask_user") {
    // Same pattern for ask_user question cards (answered via ai.answer).
    bridgePrompt = { kind: "question", id: p.id, call };
    emitShellEvent("question", { pending: [{
      id: p.id, questions: p.questions, session: p.session, created: Date.now(),
    }] });
    return;
  }
  bridgeReply({ error: "unknown bridge request: " + kind }, call);
}

// ---- the app's bytes ------------------------------------------------------------

// The database as bytes, transferred out of the worker rather than encoded
// (app.export is base64 inside JSON, ~5 copies of the file per save).
// `shareable` is for a copy that leaves this device (a download, a
// publish): without the API key, MCP headers and tool approvals, which on
// native never live in the file at all. Saves keep everything, since the
// OPFS copy is this device's own.
export async function exportBytes(shareable = false) {
  return (await sendRpc(shareable ? "app.exportShareable" : "app.exportBytes")).bytes;
}

export function openApp(bytes, name) {
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

// ---- the shell's transport --------------------------------------------------------
// core.js checks window.__uappTransport and talks through it instead of a
// WebSocket.
export function installTransport() {
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
}
// An event for the shell and every page in our frames.
export function deliverEvent(envelope) {
  if (shellOnMessage) shellOnMessage(envelope);
  broadcastToFrames(envelope);
}
