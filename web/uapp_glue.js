/* uapp browser build — worker-side glue imported by the wasm module.
 * Provides the platform pieces the Rust core can't have on wasm: wall-clock
 * time, synchronous HTTP (sync XHR — legal in a worker), event forwarding to
 * the page, AI-run scheduling, and the SharedArrayBuffer bridge that lets the
 * blocked worker ask the main thread for things mid-AI-run (run_js, app
 * actions, approvals, console reads). */

const cfg = { sab: null, notify: () => {}, aiTick: null };
export function glueConfigure(c) { Object.assign(cfg, c); }

export function js_now_ms() { return Date.now(); }

export function js_notify(json) { try { cfg.notify(json); } catch {} }

// Base64 for binary-safe sync-XHR bodies (x-user-defined keeps raw bytes as
// charCode & 0xFF).
function b64FromBinaryString(s) {
  let out = "";
  const bytes = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) bytes[i] = s.charCodeAt(i) & 0xff;
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    out += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(out);
}

export function js_http_request(method, url, headersJson, body) {
  try {
    const xhr = new XMLHttpRequest();
    xhr.open(method, url, false); // synchronous: the worker may block
    xhr.overrideMimeType("text/plain; charset=x-user-defined");
    for (const [k, v] of JSON.parse(headersJson)) {
      try { xhr.setRequestHeader(k, v); } catch {}
    }
    xhr.send(body == null ? null : body);
    if (xhr.status === 0) {
      return JSON.stringify({ error: "network/CORS failure (status 0)" });
    }
    const headers = xhr.getAllResponseHeaders().trim().split(/\r?\n/)
      .map((l) => { const i = l.indexOf(":"); return i > 0 ? [l.slice(0, i).trim(), l.slice(i + 1).trim()] : null; })
      .filter(Boolean);
    return JSON.stringify({
      status: xhr.status,
      contentType: xhr.getResponseHeader("content-type") || "",
      headers,
      bodyB64: b64FromBinaryString(xhr.responseText || ""),
    });
  } catch (e) {
    return JSON.stringify({ error: String((e && e.message) || e) });
  }
}

/* SharedArrayBuffer layout (Int32Array view):
 *   [0] reply state: 0 = waiting, 1 = ready
 *   [1] reply byte length
 *   [2] stop flag (set by the shell's Stop button while a run is live)
 *   bytes 16.. : UTF-8 reply JSON */
export function js_bridge_call(kind, payloadJson) {
  if (!cfg.sab) {
    return JSON.stringify({ error: "bridge unavailable (page is not cross-origin isolated — run_js/app actions need COOP/COEP headers)" });
  }
  const i32 = new Int32Array(cfg.sab);
  Atomics.store(i32, 0, 0);
  postMessage({ type: "bridge", kind, payload: payloadJson });
  const r = Atomics.wait(i32, 0, 0, 300000);
  if (r === "timed-out" && Atomics.load(i32, 0) === 0) {
    return JSON.stringify({ error: "bridge timeout (no reply from the page within 5 minutes)" });
  }
  const len = Atomics.load(i32, 1);
  const cap = cfg.sab.byteLength - 16;
  const bytes = new Uint8Array(cfg.sab, 16, Math.min(len, cap)).slice();
  return new TextDecoder().decode(bytes);
}

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
