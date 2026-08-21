/* uapp browser build — the dedicated worker that hosts the wasm core.
 * Owns the in-memory SQLite database; the page talks JSON-RPC to it over
 * postMessage (same protocol the native WebSocket carries). */

import init, { open_app, rpc_dispatch, ai_tick } from "./uapp_wasm.js";
import { glueConfigure } from "./uapp_glue.js";

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
      notify: (json) => postMessage({ type: "event", json }),
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
  if (m.type === "rpc") {
    let out;
    try {
      out = JSON.parse(rpc_dispatch(m.method || "", JSON.stringify(m.params || {})));
    } catch (e) {
      out = { error: { message: "dispatch crashed: " + e } };
    }
    postMessage({ type: "reply", id: m.id, ...out });
  }
};
