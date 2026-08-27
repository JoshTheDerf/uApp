// Plan mode: read-only toolset offered to the model, write calls blocked at
// runtime, and the system prompt announces the mode.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-planmode";
const FILE = `${DIR}/plan.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// Mock provider: first call answers with a (hallucinated) write_file tool_use,
// second call — which carries its error tool_result — ends the turn.
let calls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  calls.push(body);
  const send = (obj) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(obj)); };
  const sawError = (body.messages || []).some((m) => Array.isArray(m.content) &&
    m.content.some((b) => b.type === "tool_result" && String(b.content).includes("read-only")));
  if (sawError) return send({ stop_reason: "end_turn", content: [{ type: "text", text: "understood, proposing instead" }] });
  return send({ stop_reason: "tool_use", content: [{
    type: "tool_use", id: "w1", name: "write_file",
    input: { name: "index.html", content: "<h1>nope</h1>" } }] });
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

class Instance {
  async start(device, file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: "never" }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
if (info.reused) { console.error("FAIL: a uapp-server from an earlier run still has the test file open (old code!) — pkill -x uapp-server"); process.exit(2); }
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
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("rpc timeout " + method)); } }, 40000);
    });
  }
  async stop() { try { this.ws.close(); } catch {} this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}
const waitFor = async (fn, ms = 25000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(200); } return null; };

rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });

console.log("plan mode");
{
  const a = new Instance();
  await a.start("planner", FILE);
  await a.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "k", base_url: `${MOCK}`, stream: false } });
  const before = await a.rpc("files.read", { name: "index.html" }).then((r) => r.text, () => null);

  await a.rpc("chat.send", { text: "change the title", mode: "plan" });
  const done = await waitFor(async () => {
    const r = await a.rpc("chat.list", {});
    const rows = r.rows.filter((x) => x[4] === "assistant");
    if (!rows.length) return null;
    const c = JSON.parse(rows[rows.length - 1][5]);
    const t = (c.blocks || []).filter((b) => b.type === "text").map((b) => b.text).join("");
    return t.includes("proposing") ? r : null;
  });
  ok(!!done, "run completed after the blocked write");

  // 1) The offered toolset was read-only.
  const names = (calls[0].tools || []).map((t) => t.name);
  ok(names.includes("sql_query") && names.includes("read_file"), "read tools offered");
  for (const w of ["write_file", "sql_exec", "sql_batch", "delete_file", "run_js", "import_csv", "agent_run", "agent_send"]) {
    ok(!names.includes(w), `${w} withheld in plan mode`);
  }
  // 2) The system prompt announces the mode.
  const sys0 = calls[0].system;
  const sysText = Array.isArray(sys0) ? sys0.map((b) => b.text).join("") : String(sys0 || "");
  ok(sysText.includes("PLAN MODE"), "system prompt carries PLAN MODE");
  // 3) The hallucinated write was refused at runtime and nothing changed.
  const toolRows = done.rows.filter((x) => x[4] === "tool");
  const resTxt = JSON.stringify(toolRows.map((x) => x[5]));
  ok(resTxt.includes("read-only"), "runtime backstop refused the write");
  const after = await a.rpc("files.read", { name: "index.html" }).then((r) => r.text, () => null);
  ok(after === before, "file content unchanged");

  // 4) Back to auto: the write toolset returns.
  await a.rpc("ai.stop", {});
  await a.rpc("chat.send", { text: "now do it", mode: "auto" });
  await waitFor(async () => calls.length >= 3);
  const lastNames = (calls[calls.length - 1].tools || []).map((t) => t.name);
  ok(lastNames.includes("write_file"), "auto mode restores write tools");

  await a.stop();
}

mock.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
