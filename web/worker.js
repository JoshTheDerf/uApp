/* uapp browser build — the dedicated worker that hosts the wasm core.
 * Owns the in-memory SQLite database; the page talks JSON-RPC to it over
 * postMessage (same protocol the native WebSocket carries) — or, while an AI
 * run has this worker blocked, over the shared-memory inbox the glue drains. */

// The build this worker belongs to (build-web.sh stamps it). The runtime is
// fetched for exactly this build (?v=), so no cache or CDN can pair one
// deploy's glue with another's wasm, and each file says which build it is
// from: a server that has moved on (it ignores ?v= and serves what it has)
// is reported as an update instead of being run.
const BUILD = "__UAPP_BUILD__";
const V = BUILD.startsWith("__") ? "" : "?v=" + BUILD; // unstamped: as the generated glue imports it
let open_app, rpc_dispatch, ai_tick, export_bytes, glueConfigure, glueDrainInbox, gluePanicked;
const ready = (async () => {
  const [wasm, glue] = await Promise.all([import("./uapp_wasm.js" + V), import("./uapp_glue.js" + V)]);
  if (glue.GLUE_BUILD !== BUILD || (wasm.WASM_BUILD !== undefined && wasm.WASM_BUILD !== BUILD)) {
    throw Object.assign(new Error("the runtime on the server is from another build"), { updated: true });
  }
  ({ open_app, rpc_dispatch, ai_tick, export_bytes } = wasm);
  ({ glueConfigure, glueDrainInbox, gluePanicked } = glue);
  await wasm.default({ module_or_path: new URL("uapp_wasm_bg.wasm" + V, import.meta.url) });
})();

// After a panic (or any trap) the module's memory is whatever the crash left:
// locks held, a transaction half done. Nothing more runs in it, so a broken
// state can't be exported over the last good save. The page shows why.
let dead = "";
function markDead(why) {
  if (dead) return;
  dead = why || "the engine crashed";
  postMessage({ type: "dead", message: dead });
}
function crashed(e) {
  if (!gluePanicked) return; // the runtime never loaded
  if (gluePanicked() || e instanceof WebAssembly.RuntimeError) markDead(gluePanicked() || String(e));
}
const DEAD = () => ({ error: { message: "the engine stopped after an internal error (reload the page)" } });

self.onmessage = async (ev) => {
  const m = ev.data || {};
  try {
    await ready;
  } catch (e) {
    postMessage({ type: "fatal", error: "wasm failed to load: " + e, updated: !!(e && e.updated) });
    return;
  }
  if (m.type === "init") {
    glueConfigure({
      sab: m.sab || null,
      inbox: m.inbox || null,
      net: m.net || null,
      notify: (json) => postMessage({ type: "event", json }),
      onPanic: markDead,
      // RPCs that arrive through the shared-memory inbox while an AI run has
      // this worker blocked (see blockUntil in the glue).
      dispatch: (req) => reply(req.id, dispatch(req)),
      aiTick: (session) => {
        if (dead) return;
        try {
          ai_tick(session);
        } catch (e) {
          crashed(e);
          postMessage({ type: "event", json: JSON.stringify({
            method: "event",
            params: { type: "ai", state: "error", message: String(e), session },
          }) });
        }
      },
    });
    postMessage({ type: "ready", build: BUILD });
    return;
  }
  if (m.type === "open") {
    try {
      if (dead) throw new Error(dead);
      const info = open_app(m.bytes ? new Uint8Array(m.bytes) : undefined, m.name || "New App", m.user || "");
      postMessage({ type: "opened", id: m.id, info: JSON.parse(info) });
    } catch (e) {
      crashed(e);
      postMessage({ type: "opened", id: m.id, error: String(e) });
    }
    return;
  }
  if (m.type === "kick") {
    // The page wrote to the inbox; if we were blocked the wait loop already
    // took it, otherwise take it now.
    glueDrainInbox();
    return;
  }
  if (m.type === "rpc") {
    reply(m.id, dispatch(m));
  }
};

// The database bytes travel as a transfer, not a copy (see export_bytes).
function reply(id, out) {
  const bytes = out.result && out.result.bytes;
  postMessage({ type: "reply", id, ...out }, bytes instanceof Uint8Array ? [bytes.buffer] : []);
}

function dispatch(m) {
  if (dead) return DEAD();
  try {
    // The page's own saves and downloads (boot.js exportBytes).
    if (m.method === "app.exportBytes") return { result: { bytes: export_bytes() } };
    return JSON.parse(rpc_dispatch(m.method || "", JSON.stringify(m.params || {})));
  } catch (e) {
    crashed(e);
    return { error: { message: "dispatch crashed: " + e } };
  }
}
