// Minimal CDP driver: open a page in the already-running headless chromium,
// evaluate expressions in it, screenshot.
export async function connect(debugPort, url) {
  const list = async () => (await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json());
  // Open a fresh target for our URL.
  const t = await (await fetch(`http://127.0.0.1:${debugPort}/json/new?${encodeURIComponent(url)}`, { method: "PUT" })).json();
  const ws = new WebSocket(t.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0; const pending = new Map(); const events = [];
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { const { res, rej } = pending.get(m.id); pending.delete(m.id); m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result); }
    else if (m.method) events.push(m);
  };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
  await send("Runtime.enable");
  await send("Page.enable");
  await send("Log.enable");
  const evalJs = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error("eval failed: " + (r.exceptionDetails.exception?.description || r.exceptionDetails.text) + "\n  in: " + expr.slice(0, 200));
    return r.result.value;
  };
  const waitFor = async (expr, ms = 60000, every = 300) => {
    const t0 = Date.now();
    for (;;) {
      let v = null;
      try { v = await evalJs(expr); } catch (e) { v = null; }
      if (v) return v;
      if (Date.now() - t0 > ms) throw new Error("timeout waiting for: " + expr.slice(0, 160));
      await new Promise((r) => setTimeout(r, every));
    }
  };
  const shot = async (file) => {
    const r = await send("Page.captureScreenshot", { format: "png" });
    (await import("node:fs")).writeFileSync(file, Buffer.from(r.data, "base64"));
  };
  const close = async () => { try { await fetch(`http://127.0.0.1:${debugPort}/json/close/${t.id}`); } catch {} ws.close(); };
  return { send, evalJs, waitFor, shot, close, events, list, ws };
}
