// Vendoring test: files.fetch downloads a library into the archive as one
// replicated op; served same-origin with correct MIME (incl. application/wasm).
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-vendor";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// mock CDN: a JS lib and a 150KB wasm blob (valid magic header)
const wasmBytes = Buffer.concat([Buffer.from([0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0]), Buffer.alloc(150 * 1024, 7)]);
const cdn = createServer((req, res) => {
  if (req.url.startsWith("/pkg/lib.min.js")) {
    res.writeHead(200, { "content-type": "application/javascript" });
    return res.end("window.__vendored = 42;");
  }
  if (req.url.startsWith("/pkg/engine.wasm")) {
    res.writeHead(200, { "content-type": "application/wasm" });
    return res.end(wasmBytes);
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => cdn.listen(0, "127.0.0.1", r));
const CDN = `http://127.0.0.1:${cdn.address().port}`;

class Instance {
  async start(device, file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: "600", UAPP_ALLOW_LOCAL_FETCH: "1" }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    this.port = info.port; this.token = new URL(info.url).searchParams.get("t");
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1; this.pending = new Map();
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") return;
      const p = this.pending.get(m.id);
      if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
    };
  }
  rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++; this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("rpc timeout " + method)); } }, 30000);
    });
  }
  async http(path) {
    const r = await fetch(`http://127.0.0.1:${this.port}${path}`, { headers: { Cookie: `uapp_t_${this.port}=${this.token}` } });
    return { status: r.status, ct: r.headers.get("content-type"), buf: Buffer.from(await r.arrayBuffer()) };
  }
  async stop() { this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}
const waitFor = async (fn, ms = 15000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(300); } return null; };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const A = new Instance(); await A.start("devva", `${DIR}/v.uapp`);

console.log("\n[1] vendor a JS lib (default vendor/ path)");
{
  const r = await A.rpc("files.fetch", { url: `${CDN}/pkg/lib.min.js` });
  ok(r.name === "app/vendor/lib.min.js" && r.bytes > 0, `stored as ${r.name} (${r.bytes}b)`);
  const served = await A.http("/app/vendor/lib.min.js");
  ok(served.status === 200 && served.ct.includes("javascript"), "served same-origin as javascript");
  ok(served.buf.toString().includes("__vendored"), "content intact");
}

console.log("\n[2] vendor a WASM module — MIME must allow instantiateStreaming");
{
  const r = await A.rpc("files.fetch", { url: `${CDN}/pkg/engine.wasm`, name: "vendor/engine.wasm" });
  ok(r.bytes === wasmBytes.length, `150KB wasm stored (${r.bytes}b)`);
  const served = await A.http("/app/vendor/engine.wasm");
  ok(served.ct === "application/wasm", `served as application/wasm (got ${served.ct})`);
  ok(served.buf.equals(wasmBytes), "wasm bytes identical after log round-trip");
}

console.log("\n[3] a second open reuses the running server, same bytes");
{
  const B = new Instance(); await B.start("devvb", `${DIR}/v.uapp`);
  const got = await waitFor(async () => {
    const f = await B.rpc("files.list");
    return f.rows.some((r) => r[0] === "app/vendor/engine.wasm") ? f : null;
  });
  ok(!!got, "second open sees the vendored wasm");
  const served = await B.http("/app/vendor/engine.wasm");
  ok(served.buf.equals(wasmBytes), "identical bytes over the reused server");
  await B.stop();
}

console.log("\n[3b] root-absolute references (/styles.css, /js/app.js) serve too");
{
  await A.rpc("files.write", { name: "styles.css", text: "body{margin:0}" });
  await A.rpc("files.write", { name: "js/core.js", text: "window.core=1;" });
  await A.rpc("files.write", { name: "shell.css", text: "/* app file that shadows nothing */" });
  const css = await A.http("/styles.css");
  ok(css.status === 200 && css.ct.includes("css"), "/styles.css served from archive at root");
  const js = await A.http("/js/core.js");
  ok(js.status === 200 && js.buf.toString().includes("core=1"), "/js/core.js served from archive at root");
  const wasmRoot = await A.http("/vendor/engine.wasm");
  ok(wasmRoot.ct === "application/wasm", "root-absolute wasm keeps its MIME");
  const shell = await A.http("/shell.css");
  ok(!shell.buf.toString().includes("shadows nothing"), "shell routes still win over same-named app files");
  const miss = await A.http("/definitely-missing.js");
  ok(miss.status === 404, "missing files still 404");
  const noauth = await fetch(`http://127.0.0.1:${A.port}/js/core.js`);
  ok(noauth.status === 403, "root fallback still requires auth");
}

console.log("\n[4] guardrails + tool registration");
{
  let err = null;
  try { await A.rpc("files.fetch", { url: "ftp://nope/x" }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("http"), "non-http URL rejected");
  err = null;
  try { await A.rpc("files.fetch", { url: `${CDN}/pkg/lib.min.js`, name: "../escape.js" }); } catch (e) { err = e; }
  ok(!!err, "path traversal in target name rejected");
  err = null;
  try { await A.rpc("files.fetch", { url: `${CDN}/missing.js` }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("404"), "upstream 404 surfaced");
  const tools = await A.rpc("tools.list");
  const t = tools.find((x) => x.name === "download_lib");
  ok(!!t && t.gated, "download_lib registered as a gated tool");
}

await A.stop();
cdn.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
