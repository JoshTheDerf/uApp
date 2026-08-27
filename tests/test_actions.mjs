// App-action tests: business logic registered by the page (uapp.action) is
// callable by the AI as app__<name> tools, and the app can call every tool
// the AI has (tools.call). Covers gating metadata, timeout, disconnect.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-actions";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- mock provider: drives the AI to call app actions ----
let calls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (o) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(o)); };
  calls.push(body);
  const tc = (id, name, args) => ({ choices: [{ message: { content: null, tool_calls: [
    { id, type: "function", function: { name, arguments: JSON.stringify(args) } } ] } }] });
  const n = calls.length;
  if (n === 1) return send(tc("x1", "app__add_employee", { name: "Karen", hired: "2024-01-02" }));
  if (n === 2) return send(tc("x2", "app__list_employees", {}));
  if (n === 3) return send({ choices: [{ message: { content: "employee added and listed" } }] });
  if (n === 4) return send(tc("x4", "app__slow", {}));
  return send({ choices: [{ message: { content: "gave up on slow" } }] });
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

class Client {
  constructor(port, token) { this.port = port; this.token = token; }
  async connect() {
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1; this.pending = new Map(); this.invokes = [];
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "action.invoke") { this.onInvoke && this.onInvoke(m.params); return; }
      if (m.method === "event") return;
      const p = this.pending.get(m.id);
      if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
    };
    return this;
  }
  rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++; this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("rpc timeout " + method)); } }, 20000);
    });
  }
  raw(method, params) { this.ws.send(JSON.stringify({ method, params })); }
}

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/a.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "devact", UAPP_LINGER_SECS: "600", UAPP_ACTION_TIMEOUT_MS: "1500" },
  stdio: ["ignore", "pipe", "pipe"],
});
const line = await new Promise((res) => { let b = ""; proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) res(b.slice(0, i)); }); });
const info = JSON.parse(line);
if (info.reused) { console.error("FAIL: a uapp-server from an earlier run still has the test file open (old code!) — pkill -x uapp-server"); process.exit(2); }
const token = new URL(info.url).searchParams.get("t");

const shell = await new Client(info.port, token).connect(); // plays the shell
const page = await new Client(info.port, token).connect();  // plays the app page

await shell.rpc("sql.batch", { sql: "CREATE TABLE employees(id INTEGER PRIMARY KEY, name TEXT, hired TEXT);" });
await shell.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "t", model: "m", base_url: MOCK } });

// the "page" registers its actions and serves invocations like uapp.js does
page.onInvoke = async (p) => {
  const reply = (params) => page.raw("actions.result", params);
  try {
    if (p.name === "add_employee") {
      if (!p.input.name) throw new Error("name required");
      await page.rpc("sql.exec", { sql: "INSERT INTO employees(name, hired) VALUES(?,?)", params: [p.input.name, p.input.hired] });
      page.invokes.push(p);
      return reply({ id: p.id, result: { ok: true } });
    }
    if (p.name === "list_employees") {
      const r = await page.rpc("sql.query", { sql: "SELECT name, hired FROM employees" });
      return reply({ id: p.id, result: r.rows });
    }
    // "slow": never reply — exercises the timeout
  } catch (e) { reply({ id: p.id, error: e.message }); }
};
page.raw("actions.register", { actions: [
  { name: "add_employee", description: "Add an employee", schema: { type: "object", properties: { name: { type: "string" }, hired: { type: "string" } }, required: ["name"] } },
  { name: "list_employees", description: "List employees", readonly: true, schema: { type: "object", properties: {} } },
  { name: "slow", description: "never responds", schema: { type: "object", properties: {} } },
]});
await sleep(300);

const waitFor = async (fn, ms = 20000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(250); } return null; };

console.log("\n[1] registration + tools.list metadata");
{
  const tools = await shell.rpc("tools.list");
  const add = tools.find((t) => t.name === "app__add_employee");
  const list = tools.find((t) => t.name === "app__list_employees");
  ok(!!add && add.kind === "app" && add.gated === true, "write action listed, gated");
  ok(!!list && list.gated === false, "readonly action listed, ungated");
}

console.log("\n[2] AI calls app actions (same code as the UI)");
{
  await shell.rpc("chat.send", { text: "add karen then list everyone", mode: "auto" });
  const done = await waitFor(async () => {
    const r = await shell.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("employee added and listed")) ? r : null;
  });
  ok(!!done, "AI run completed via app actions");
  ok(page.invokes.length === 1 && page.invokes[0].input.name === "Karen", "page handler received the AI's input");
  const q = await shell.rpc("sql.query", { sql: "SELECT name FROM employees" });
  ok(q.rows.length === 1 && q.rows[0][0] === "Karen", "action's write landed in the db (replicated op)");
  const toolMsgs = done.rows.filter((r) => r[4] === "tool").map((r) => JSON.parse(r[5]));
  const listResult = toolMsgs.flatMap((c) => c.results).find((r) => r.name === "app__list_employees");
  ok(!!listResult && listResult.content.includes("Karen"), "action return value became the tool result");
  ok(calls[0].tools.some((t) => t.function.name === "app__add_employee"), "actions advertised as tools to the provider");
}

console.log("\n[3] unresponsive action times out without hanging the run");
{
  await shell.rpc("chat.send", { text: "call the slow one", mode: "auto" });
  const done = await waitFor(async () => {
    const r = await shell.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("gave up on slow")) ? r : null;
  });
  ok(!!done, "run completed despite unresponsive action");
  const toolMsgs = done.rows.filter((r) => r[4] === "tool").map((r) => JSON.parse(r[5]));
  const slow = toolMsgs.flatMap((c) => c.results).find((r) => r.name === "app__slow");
  ok(!!slow && slow.is_error && slow.content.includes("did not respond"), "timeout surfaced as a tool error");
}

console.log("\n[4] symmetry: the app calls the AI's tools (and its own actions)");
{
  const q = await page.rpc("tools.call", { name: "sql_query", input: { sql: "SELECT count(*) FROM employees" } });
  ok(q.rows[0][0] === 1, "app called builtin tool sql_query via tools.call");
  const viaAction = await page.rpc("tools.call", { name: "app__list_employees", input: {} });
  ok(Array.isArray(viaAction) && viaAction[0][0] === "Karen", "tools.call routes app__ actions back into the page");
  await shell.rpc("config.set", { key: "tools_disabled", value: ["app__add_employee"] });
  let err = null;
  try { await page.rpc("tools.call", { name: "app__add_employee", input: { name: "X" } }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("disabled"), "disabled tools rejected for the app too");
  await shell.rpc("config.set", { key: "tools_disabled", value: [] });
}

console.log("\n[5] actions die with their page");
{
  page.ws.close();
  const gone = await waitFor(async () => {
    const tools = await shell.rpc("tools.list");
    return tools.every((t) => !t.name.startsWith("app__")) ? true : null;
  }, 8000);
  ok(!!gone, "actions unregistered when the page disconnected");
}

proc.kill("SIGINT"); await sleep(300); proc.kill("SIGKILL");
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
