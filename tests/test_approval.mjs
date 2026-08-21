// Approval-flow test: manual mode gates mutating tools; approve / always / deny;
// tool toggles; attach-from-archive refs.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-approval";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- mock provider: 1 sql_batch, 2 sql_batch, 3 write_file, 4 done ----
let calls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (o) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(o)); };
  if (req.url !== "/chat/completions") { res.writeHead(404); return res.end(); }
  calls.push(body);
  const n = calls.length;
  const tc = (id, name, args) => ({ choices: [{ message: { content: null, tool_calls: [
    { id, type: "function", function: { name, arguments: JSON.stringify(args) } } ] } }] });
  if (n === 1) return send(tc("g1", "sql_batch", { sql: "CREATE TABLE IF NOT EXISTS gated(x INTEGER);" }));
  if (n === 2) return send(tc("g2", "sql_batch", { sql: "INSERT INTO gated(x) VALUES(1);" }));
  if (n === 3) return send(tc("g3", "write_file", { name: "index.html", content: "<h1>nope</h1>" }));
  return send({ choices: [{ message: { content: "finished the workflow" } }] });
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

class Instance {
  async start(device, file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    this.ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${new URL(info.url).searchParams.get("t")}`);
    this.nextId = 1; this.pending = new Map(); this.approvalEvents = [];
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") { if (m.params.type === "approval") this.approvalEvents.push(m.params.pending); return; }
      const p = this.pending.get(m.id);
      if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
    };
  }
  rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++; this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("rpc timeout " + method)); } }, 20000);
    });
  }
  async stop() { this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}
const waitFor = async (fn, ms = 20000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(200); } return null; };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const A = new Instance(); await A.start("devappr", `${DIR}/appr.uapp`);
await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "t", model: "m", base_url: MOCK } });

console.log("\n[1] tools.list + disable");
{
  const tools = await A.rpc("tools.list");
  ok(tools.some((t) => t.name === "sql_batch" && t.gated), "registry lists gated sql_batch");
  ok(tools.some((t) => t.name === "sql_query" && !t.gated), "sql_query ungated");
  await A.rpc("config.set", { key: "tools_disabled", value: ["delete_file"] });
  const t2 = await A.rpc("tools.list");
  ok(t2.find((t) => t.name === "delete_file")?.enabled === false, "disabled tool reported off");
}

console.log("\n[2] manual mode: approve → always → deny");
{
  await A.rpc("chat.send", { text: "do gated things", mode: "manual" });
  // first gated call must pause
  const p1 = await waitFor(async () => {
    const p = await A.rpc("ai.pending");
    return p.length === 1 ? p : null;
  });
  ok(!!p1 && p1[0].name === "sql_batch", "gated sql_batch paused for approval");
  ok(A.approvalEvents.length >= 1, "approval event pushed to client");
  // approve with always=true
  await A.rpc("ai.approve", { id: p1[0].id, allow: true, always: true });
  // second sql_batch must run WITHOUT pausing; next pause should be write_file
  const p2 = await waitFor(async () => {
    const p = await A.rpc("ai.pending");
    return p.length === 1 && p[0].name === "write_file" ? p : null;
  });
  ok(!!p2, "always-allow skipped second sql_batch; write_file paused");
  const sawBatchPending2 = A.approvalEvents.some((e) => e.some((x) => x.id === "g2"));
  ok(!sawBatchPending2, "no approval prompt for always-allowed tool");
  // deny write_file
  await A.rpc("ai.approve", { id: p2[0].id, allow: false });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("finished the workflow")) ? r : null;
  });
  ok(!!done, "run completed after deny");
  const q = await A.rpc("sql.query", { sql: "SELECT count(*) FROM gated" });
  ok(q.rows[0][0] === 1, "approved writes executed (create + always-allowed insert)");
  const f = await A.rpc("files.read", { name: "index.html" });
  ok(!f.text.includes("nope"), "denied write_file did NOT run");
  const toolRows = done.rows.filter((r) => r[4] === "tool").map((r) => JSON.parse(r[5]));
  const deniedRes = toolRows.flatMap((c) => c.results).find((res) => res.denied);
  ok(!!deniedRes && deniedRes.content.includes("denied permission"), "denial recorded as tool result for the model");
  const modelSawDenial = calls[3].messages.some((m) => m.role === "tool" && String(m.content).includes("denied permission"));
  ok(modelSawDenial, "model received the denial and adapted");
  const sentTools = calls[0].tools.map((t) => t.function.name);
  ok(!sentTools.includes("delete_file"), "disabled tool omitted from provider request");
}

console.log("\n[3] auto mode: no pauses");
{
  calls = [];
  await A.rpc("sql.batch", { sql: "DROP TABLE IF EXISTS gated;" });
  await A.rpc("chat.clear");
  await A.rpc("chat.send", { text: "again", mode: "auto" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("finished the workflow")) ? r : null;
  });
  ok(!!done, "auto run completed");
  const everPending = A.approvalEvents.slice(-4).some((e) => e.length > 0 && Date.now() - e[0]?.created < 30000);
  const q = await A.rpc("sql.query", { sql: "SELECT count(*) FROM gated" });
  ok(q.rows[0][0] === 1, "all writes ran unprompted in auto mode");
  const f = await A.rpc("files.read", { name: "index.html" });
  ok(f.text.includes("nope"), "write_file ran in auto mode");
}

console.log("\n[4] attach-from-archive refs");
{
  await A.rpc("files.write", { name: "notes.txt", text: "hello ref" });
  await A.rpc("ai.stop");
  await A.rpc("chat.send", { text: "look at this", refs: ["notes.txt"], mode: "auto" }).catch(() => {});
  const r = await waitFor(async () => {
    const c = await A.rpc("chat.list");
    const users = c.rows.filter((x) => x[4] === "user");
    const last = users[users.length - 1];
    if (!last) return null;
    const content = JSON.parse(last[5]);
    return (content.attachments || []).some((a) => a.ref && a.name === "notes.txt") ? content : null;
  });
  ok(!!r, "ref attachment recorded without copying the file");
  let err = null;
  try { await A.rpc("chat.send", { text: "x", refs: ["missing.file"] }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("no such file"), "missing ref rejected");
}

await A.stop();
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
