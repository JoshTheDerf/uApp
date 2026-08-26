/* uapp shell — core: the WebSocket JSON-RPC client, the event bus every
 * component subscribes to, and the small shared state (app info, mode). */

export const bus = new EventTarget();
export const emit = (type, detail) => bus.dispatchEvent(new CustomEvent(type, { detail }));
export const on = (type, fn) => bus.addEventListener(type, (e) => fn(e.detail));

export const state = {
  appId: "",
  mode: "auto",   // tool approval mode: auto | manual | plan (topbar owns the toggle)
  info: null,     // latest app.info snapshot
  shuttingDown: false,
};

// ---------- RPC ----------
let ws = null, nextId = 1;
const pending = new Map(); // rpc id -> {resolve, reject}
// Reject every in-flight rpc() when its socket dies — otherwise awaiting
// callers hang forever after a background/resume drop.
function failPending(why) {
  for (const [, p] of pending) { try { p.reject(new Error(why)); } catch {} }
  pending.clear();
}
let reconnectTimer = null;
// Browser (wasm) build: boot.js installs a worker-backed transport before the
// shell loads; when present it replaces the WebSocket entirely.
const wasmT = () => window.__uappTransport;
export function connect() {
  if (wasmT()) {
    wasmT().onMessage((m) => {
      if (m.method === "event") return handleEvent(m.params);
      const p = pending.get(m.id);
      if (!p) return;
      pending.delete(m.id);
      m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result);
    });
    refreshInfo();
    emit("ws-open");
    return;
  }
  clearTimeout(reconnectTimer);
  // A frozen-then-resumed webview can leave the old socket half-open; drop
  // it (silently — its onclose must not schedule another connect) so we
  // never end up with two live sockets.
  if (ws) { const old = ws; ws = null; old.onclose = null; try { old.close(); } catch {} failPending("reconnecting"); }
  ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`);
  ws.onopen = () => {
    refreshInfo();
    emit("ws-open");
  };
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event") return handleEvent(m.params);
    const p = pending.get(m.id);
    if (!p) return;
    pending.delete(m.id);
    m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result);
  };
  ws.onclose = () => {
    failPending("connection lost");
    if (!state.shuttingDown) reconnectTimer = setTimeout(connect, 1000);
  };
}
// Backgrounded webviews (Android especially) drop the socket AND freeze the
// reconnect setTimeout. When the page becomes visible again, reconnect at
// once if the socket is down — and if it merely looks open, probe it: a
// suspended webview often leaves a half-open connection that never fires
// onclose.
document.addEventListener("visibilitychange", () => {
  if (wasmT()) return; // worker transport never drops
  if (document.visibilityState !== "visible" || state.shuttingDown) return;
  if (!ws || ws.readyState !== 1) { connect(); return; }
  const id = nextId++;
  let alive = false;
  pending.set(id, { resolve: () => { alive = true; }, reject: () => { alive = true; } });
  try { ws.send(JSON.stringify({ id, method: "ping" })); } catch { connect(); return; }
  setTimeout(() => { if (!alive && ws && ws.readyState === 1) { try { ws.close(); } catch {} } }, 3000);
});
export function rpc(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    if (wasmT()) {
      pending.set(id, { resolve, reject });
      wasmT().send({ id, method, params });
      return;
    }
    if (!ws || ws.readyState !== 1) return reject(new Error("not connected"));
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });
}
export function closeSocket() {
  state.shuttingDown = true;
  if (wasmT()) return;
  try { ws.close(); } catch {}
}

// ---------- server events -> bus ----------
// Components subscribe to the slices they own; nothing here touches the DOM.
function handleEvent(p) {
  if (p.type === "changes") {
    emit("changes", p); // any write landed (Android write-back listens here)
    if (p.files) emit("files-changed");
    if (p.chat) emit("chat-changed");
    if (p.config) refreshInfo();
  } else if (p.type === "ai") {
    emit("ai", p);
  } else if (p.type === "present") {
    emit("present", p);
  } else if (p.type === "reload") {
    emit("reload", p); // app.reload RPC / the AI's reload_app tool
  } else if (p.type === "drop_hover" || p.type === "drop_leave" || p.type === "drop_files") {
    emit("native-drop", p);
  } else if (p.type === "renamed") {
    refreshInfo();
  } else if (p.type === "approval") {
    emit("approval", p.pending || []);
  } else if (p.type === "question") {
    emit("question", p.pending || []);
  } else if (p.type === "sync") {
    emit("sync", p);
  } else if (p.type === "scratch-load") {
    emit("scratch-load"); // a run_js scratchpad call is waiting on the frame
  }
}

// ---------- app info ----------
export async function refreshInfo() {
  try {
    const info = await rpc("app.info");
    state.info = info;
    state.appId = info.appId || "";
    // The approval mode is a per-device choice, remembered per app.
    const saved = state.appId && localStorage.getItem(`uapp.mode.${state.appId}`);
    if (saved && saved !== state.mode) state.mode = saved;
    if (info.name) document.title = `${info.name} — uapp`;
    emit("info", info);
  } catch {}
}
export function setMode(mode) {
  state.mode = mode;
  if (state.appId) localStorage.setItem(`uapp.mode.${state.appId}`, mode);
  emit("mode", mode);
}
setInterval(refreshInfo, 7000);
