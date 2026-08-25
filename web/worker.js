/* uapp browser build — the dedicated worker that hosts the wasm core.
 * Owns the in-memory SQLite database; the page talks JSON-RPC to it over
 * postMessage (same protocol the native WebSocket carries) — or, while an AI
 * run has this worker blocked, over the shared-memory inbox the glue drains. */

import init, { open_app, rpc_dispatch, ai_tick } from "./uapp_wasm.js";
import { glueConfigure, glueDrainInbox } from "./uapp_glue.js";

const ready = init();

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
      // RPCs that arrive through the shared-memory inbox while an AI run has
      // this worker blocked (see blockUntil in the glue).
      dispatch: (req) => postMessage({ type: "reply", id: req.id, ...dispatch(req) }),
      aiTick: (session) => {
        try {
          ai_tick(session);
        } catch (e) {
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
      const info = open_app(m.bytes ? new Uint8Array(m.bytes) : undefined, m.name || "New App", m.user || "");
      postMessage({ type: "opened", id: m.id, info: JSON.parse(info) });
    } catch (e) {
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
  try {
    return JSON.parse(rpc_dispatch(m.method || "", JSON.stringify(m.params || {})));
  } catch (e) {
    return { error: { message: "dispatch crashed: " + e } };
  }
}
