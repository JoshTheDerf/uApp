/* uapp browser build — the dedicated worker that hosts the wasm core.
 * Owns the in-memory SQLite database; the page talks JSON-RPC to it over
 * postMessage (same protocol the native WebSocket carries) — or, while an AI
 * run has this worker blocked, over the shared-memory inbox the glue drains. */

import init, { open_app, rpc_dispatch, ai_tick } from "./uapp_wasm.js";
import { glueConfigure, glueDrainInbox, gluePanicked } from "./uapp_glue.js";

const ready = init();

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
  if (gluePanicked() || e instanceof WebAssembly.RuntimeError) markDead(gluePanicked() || String(e));
}
const DEAD = () => ({ error: { message: "the engine stopped after an internal error (reload the page)" } });

self.onmessage = async (ev) => {
  const m = ev.data || {};
  try {
    await ready;
  } catch (e) {
    postMessage({ type: "fatal", error: "wasm failed to load: " + e });
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
      dispatch: (req) => postMessage({ type: "reply", id: req.id, ...dispatch(req) }),
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
    postMessage({ type: "ready" });
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
    postMessage({ type: "reply", id: m.id, ...dispatch(m) });
  }
};

function dispatch(m) {
  if (dead) return DEAD();
  try {
    return JSON.parse(rpc_dispatch(m.method || "", JSON.stringify(m.params || {})));
  } catch (e) {
    crashed(e);
    return { error: { message: "dispatch crashed: " + e } };
  }
}
