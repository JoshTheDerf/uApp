// run_js / eval.run: code execution routed to registered page contexts
// ("scratchpad" = hidden shell iframe, "app" = live app page). The test
// plays the browser side: it registers contexts over WS and actually
// evaluates the code Node-side with the same AsyncFunction pattern uapp.js
// uses, including a mini uapp API backed by real RPCs.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-eval";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- mock AI provider: drives run_js through the real tool loop ----
let aiCalls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (o) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(o)); };
  aiCalls.push(body);
  const tc = (id, name, args) => ({ choices: [{ message: { content: null, tool_calls: [
    { id, type: "function", function: { name, arguments: JSON.stringify(args) } } ] } }] });
  if (aiCalls.length === 1) return send(tc("e1", "run_js", {
    code: "const r = await uapp.query('SELECT count(*) FROM inventory'); console.log('counted'); return {n: r.rows[0][0]};",
  }));
  return send({ choices: [{ message: { content: "there are rows" } }] });
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/e.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "deveval", UAPP_LINGER_SECS: "600", UAPP_EVAL_TIMEOUT_MS: "4000" },
  stdio: ["ignore", "pipe", "pipe"],
});
const line = await new Promise((res) => { let b = ""; proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) res(b.slice(0, i)); }); });
const info = JSON.parse(line);
if (info.reused) { console.error("FAIL: a uapp-server from an earlier run still has the test file open (old code!) — pkill -x uapp-server"); process.exit(2); }
const TOKEN = new URL(info.url).searchParams.get("t");

// One WS client that can register itself as an eval context and actually
// execute incoming eval.invoke payloads (like uapp.js does in the browser).
function client(contexts = []) {
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${TOKEN}`);
  const pending = new Map(); let id = 1;
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 30000);
  });
  const uapp = {
    query: (sql, params = []) => rpc("sql.query", { sql, params }),
    exec: (sql, params = []) => rpc("sql.exec", { sql, params }),
    readFile: (name) => rpc("files.read", { name }),
    tool: (name, input = {}) => rpc("tools.call", { name, input }),
  };
  const marker = contexts.join("+") || "none";
  ws.onmessage = async (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "eval.invoke") {
      const p = m.params;
      const send = (params) => ws.send(JSON.stringify({ method: "eval.result", params }));
      const logs = [];
      const console2 = { log: (...a) => logs.push(a.join(" ")) };
      try {
        const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
        const fn = new AsyncFunction("uapp", "loadScript", "console", "MARKER", p.code);
        let value = await fn(uapp, async () => true, console2, marker);
        if (value === undefined) value = null;
        send({ id: p.id, result: { value, logs } });
      } catch (e) {
        send({ id: p.id, error: String(e && e.message || e) });
      }
      return;
    }
    if (m.method === "event") return;
    const pr = pending.get(m.id);
    if (pr) { pending.delete(m.id); m.error ? pr.reject(new Error(m.error.message)) : pr.resolve(m.result); }
  };
  const ready = new Promise((r) => (ws.onopen = async () => {
    for (const c of contexts) ws.send(JSON.stringify({ method: "ctx.register", params: { context: c } }));
    r();
  }));
  return { ws, rpc, ready };
}

const shell = client([]); // plain client, no contexts (like the shell itself)
await shell.ready;
const rpc = shell.rpc;

console.log("\n[1] /scratch/ route");
{
  const r = await fetch(`http://127.0.0.1:${info.port}/scratch/?t=${TOKEN}`);
  const t = await r.text();
  ok(r.status === 200 && t.includes('__uappContext = "scratchpad"'), "scratch page served, sets context");
  ok((r.headers.get("cache-control") || "").includes("no-store"), "scratch page no-store");
  const bad = await fetch(`http://127.0.0.1:${info.port}/scratch/`);
  ok(bad.status === 403, "scratch page requires auth");
}

console.log("\n[2] no context connected -> clear error");
{
  let err = null;
  try { await rpc("eval.run", { code: "return 1" }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("no 'scratchpad' page is connected"), `helpful error (${err && err.message})`);
  let err2 = null;
  try { await rpc("eval.run", { code: "return 1", context: "bogus" }); } catch (e) { err2 = e; }
  ok(!!err2 && err2.message.includes("context must be"), "bad context rejected");
}

console.log("\n[3] scratchpad execution end-to-end");
const scratch = client(["scratchpad"]);
await scratch.ready;
await sleep(200);
{
  const r = await rpc("eval.run", { code: "return 6 * 7" });
  ok(r.value === 42, `return value round-trips (${JSON.stringify(r)})`);
  await rpc("sql.batch", { sql: "CREATE TABLE inventory(item TEXT, qty INT); INSERT INTO inventory VALUES('widget', 12), ('gizmo', 3)" });
  const q = await rpc("eval.run", { code: "const r = await uapp.query('SELECT sum(qty) FROM inventory'); return r.rows[0][0]" });
  ok(q.value === 15, "eval'd code reaches the real database through uapp API");
  const lg = await rpc("eval.run", { code: "console.log('step', 1); console.log('step', 2); return null" });
  ok(lg.logs.length === 2 && lg.logs[1] === "step 2", "console output captured");
  let err = null;
  try { await rpc("eval.run", { code: "throw new Error('kaboom')" }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("kaboom") && err.message.includes("scratchpad"), "thrown error surfaces with context");
  await rpc("eval.run", { code: "globalThis.__stash = 'persisted'; return true" });
  const st = await rpc("eval.run", { code: "return globalThis.__stash" });
  ok(st.value === "persisted", "scratchpad state persists between calls");
}

console.log("\n[4] app context routes to the app page");
const appPage = client(["app"]);
await appPage.ready;
await sleep(200);
{
  const r = await rpc("eval.run", { code: "return MARKER", context: "app" });
  ok(r.value === "app", `routed to the app page (${r.value})`);
  const s = await rpc("eval.run", { code: "return MARKER", context: "scratchpad" });
  ok(s.value === "scratchpad", "scratchpad still routed separately");
}

console.log("\n[5] newest registrant wins; falls back when it disconnects");
{
  const scratch2 = client(["scratchpad"]);
  await scratch2.ready;
  await sleep(200);
  const r = await rpc("eval.run", { code: "return MARKER" });
  ok(r.value === "scratchpad", "newest scratchpad page answers");
  scratch2.ws.close();
  await sleep(400);
  const r2 = await rpc("eval.run", { code: "return 'still ' + MARKER" });
  ok(r2.value === "still scratchpad", "older page takes over after newest disconnects");
}

console.log("\n[6] run_js is a gated tool");
{
  const tools = await rpc("tools.list");
  const t = tools.find((t) => t.name === "run_js");
  ok(!!t && t.gated === true, "run_js listed and gated");
  // App-triggered gated tools now require approval (no longer bypass the gate)
  // The app can call run_js, but only with user approval or "always allow"
}

console.log("\n[7] AI tool loop executes run_js in the scratchpad");
{
  await rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "t", model: "m", base_url: MOCK } });
  await rpc("chat.send", { text: "how many inventory rows?", mode: "auto" });
  const deadline = Date.now() + 20000; let done = null;
  while (Date.now() < deadline && !done) {
    const r = await rpc("chat.list");
    if (r.rows.some((row) => row[5].includes("there are rows"))) done = r;
    else await sleep(250);
  }
  ok(!!done, "AI run completed");
  const toolMsgs = (done ? done.rows : []).filter((r) => r[4] === "tool").map((r) => JSON.parse(r[5]));
  const ev = toolMsgs.flatMap((c) => c.results || []).find((r) => r.name === "run_js");
  ok(!!ev && ev.content.includes('"n":2') && ev.content.includes("counted"), `AI got value + logs (${ev && ev.content.slice(0, 80)})`);
}

console.log("\n[8] eval timeout produces a clear error");
{
  let err = null;
  try { await rpc("eval.run", { code: "await new Promise(() => {})" }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("did not finish within"), `timeout error (${err && err.message.slice(0, 60)})`);
}

proc.kill("SIGINT"); await sleep(300); proc.kill("SIGKILL");
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
