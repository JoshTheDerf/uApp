// reload_app waits for the reloaded page: the tool returns only once a NEW
// "app" connection has announced ctx.loaded — so run_js / read_console right
// after it see the new document, not the old one. The test plays the page:
// on the "reload" event it drops its socket and connects again like a fresh
// document would (ctx.register, then ctx.loaded on window load).
import { spawn } from "node:child_process";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-reload";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/r.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "devreload", UAPP_LINGER_SECS: "600" },
  stdio: ["ignore", "pipe", "pipe"],
});
const line = await new Promise((res) => { let b = ""; proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) res(b.slice(0, i)); }); });
const info = JSON.parse(line);
if (info.reused) { console.error("FAIL: a uapp-server from an earlier run still has the test file open (old code!) — pkill -x uapp-server"); process.exit(2); }
if (process.env.UAPP_DEBUG) proc.stderr.pipe(process.stderr);
const TOKEN = new URL(info.url).searchParams.get("t");

function client({ context = null, loadDelayMs = 0, onReload = null } = {}) {
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${TOKEN}`);
  const pending = new Map(); let id = 1;
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 30000);
  });
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event" && m.params?.type === "reload" && onReload) { onReload(); return; }
    const pr = pending.get(m.id); if (!pr) return; pending.delete(m.id);
    m.error ? pr.reject(new Error(m.error.message)) : pr.resolve(m.result);
  };
  const open = new Promise((r) => (ws.onopen = () => {
    if (context) {
      ws.send(JSON.stringify({ method: "ctx.register", params: { context } }));
      // Like a real page: `load` comes some time after the <head> connected.
      setTimeout(() => ws.send(JSON.stringify({ method: "ctx.loaded", params: { context } })), loadDelayMs);
    }
    r();
  }));
  return { ws, rpc, open, close: () => ws.close() };
}

console.log("[1] no app page connected: fails at once with a clear error");
{
  const shell = client(); await shell.open;
  const t0 = Date.now();
  const r = await shell.rpc("tools.call", { name: "reload_app", input: {} }).catch((e) => e.message);
  ok(/no app page is connected/.test(String(r)), "error names the cause", JSON.stringify(r));
  ok(Date.now() - t0 < 2000, "did not wait", `${Date.now() - t0}ms`);
  shell.close();
}

console.log("\n[2] app page reloads on the event: the tool returns after the NEW page loaded");
{
  let reloads = 0, page = null;
  const reloadLikeABrowser = () => {
    reloads++;
    page.close(); // the old document goes away...
    // ...and the new one connects, registers from its <head>, and reports
    // load 700ms later (scripts running). The tool must wait for that.
    setTimeout(() => { page = client({ context: "app", loadDelayMs: 700, onReload: reloadLikeABrowser }); }, 200);
  };
  page = client({ context: "app", onReload: reloadLikeABrowser }); await page.open;
  await sleep(200);
  const shell = client(); await shell.open;
  const t0 = Date.now();
  const r = await shell.rpc("tools.call", { name: "reload_app", input: {} });
  const ms = Date.now() - t0;
  ok(r.loaded === true, "loaded:true", JSON.stringify(r));
  ok(ms >= 850 && ms < 3000, "waited for ctx.loaded of the new page (~900ms)", `${ms}ms`);
  ok(reloads === 1, "the page was asked to reload once", `${reloads}`);
  // A page that comes back but never finishes loading: bounded wait.
  const stuck = () => { page.close(); setTimeout(() => { page = client({ context: "app", loadDelayMs: 60000 }); }, 100); };
  page.close(); page = client({ context: "app", onReload: stuck }); await page.open; await sleep(200);
  const t1 = Date.now();
  const r2 = await shell.rpc("tools.call", { name: "reload_app", input: {} });
  ok(r2.loaded === false && /did not finish/.test(r2.note || ""), "stuck page: loaded:false with a note", JSON.stringify(r2));
  ok(Date.now() - t1 >= 9500 && Date.now() - t1 < 14000, "gave up after ~10s", `${Date.now() - t1}ms`);
  page.close(); shell.close();
}

console.log("\n[3] app.reload RPC (topbar button) gives the same answer as the tool");
{
  const shell = client(); await shell.open;
  const e = await shell.rpc("app.reload", {}).catch((e) => e.message);
  ok(/no app page is connected/.test(String(e)), "no page: clear error", String(e));
  let page = client({ context: "app", onReload: () => { page.close(); setTimeout(() => { page = client({ context: "app", loadDelayMs: 100 }); }, 50); } });
  await page.open; await sleep(200);
  const r = await shell.rpc("app.reload", {});
  ok(r.ok === true && r.loaded === true, "with a page: waits for the new load", JSON.stringify(r));
  page.close(); shell.close();
}

console.log("\n[4] app__ action called before the page has registered it: waits, then runs");
{
  // Like a page whose bootstrap chain (uapp.js → build.js → …) registers its
  // actions a while after `load`. readonly: a not-yet-registered action counts
  // as gated (tools::is_gated), and this client never answers approval prompts.
  const page = client({ context: "app" }); await page.open;
  page.ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "action.invoke") {
      page.ws.send(JSON.stringify({ method: "actions.result", params: { id: m.params.id, result: { built: m.params.input.n * 2 } } }));
    }
  });
  await sleep(200);
  const shell = client(); await shell.open;
  setTimeout(() => page.ws.send(JSON.stringify({ method: "actions.register", params: { actions: [{ name: "build_site", description: "b", readonly: true, schema: { type: "object", properties: {} } }] } })), 1500);
  const t0 = Date.now();
  const r = await shell.rpc("tools.call", { name: "app__build_site", input: { n: 21 } });
  const ms = Date.now() - t0;
  ok(r && r.built === 42, "action ran once registered", JSON.stringify(r));
  ok(ms >= 1400 && ms < 5000, "waited for the registration (~1.5s)", `${ms}ms`);
  const t1 = Date.now();
  const e = await shell.rpc("tools.call", { name: "app__nope", input: {} }).catch((e) => e.message);
  ok(/no app action named 'nope'/.test(String(e)), "never-registered action: clear error", String(e));
  ok(Date.now() - t1 >= 7500 && Date.now() - t1 < 12000, "gave up after ~8s", `${Date.now() - t1}ms`);
  page.close(); shell.close();
}

console.log(`\n${passed} passed, ${failed} failed`);
// SIGTERM lets the server linger for other clients; make sure it is gone so the
// next run does not hand off to this (stale) instance.
proc.kill(); await sleep(300); proc.kill("SIGKILL");
process.exit(failed ? 1 : 0);
