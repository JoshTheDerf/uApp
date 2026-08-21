// Multi-chat sessions, compaction, sub-agents and per-session runs.
// Mock provider (Anthropic wire shape, non-streaming) like tests/test_ai.mjs.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-sessions";
const FILE = `${DIR}/sessions.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---------- mock provider ----------
// Routes (each becomes an ai.base_url): plain replies, sub-agent flow,
// summarizer, and a stalling endpoint for the concurrency check.
let plainCalls = [], agentCalls = [], sumCalls = [], stallCalls = [];
let releaseStall = null;
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const lastUserText = (body) => {
  for (const m of [...(body.messages || [])].reverse()) {
    if (m.role !== "user" || !Array.isArray(m.content)) continue;
    const t = m.content.filter((b) => b.type === "text").map((b) => b.text).join(" ");
    if (t) return t;
  }
  return "";
};
const hasToolResultFor = (body, name) =>
  (body.messages || []).some((m) =>
    Array.isArray(m.content) && m.content.some((b) => b.type === "tool_result" && String(b.content).includes(name)));

const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (obj) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(obj)); };
  const text = (t) => send({ stop_reason: "end_turn", content: [{ type: "text", text: t }] });

  if (req.url === "/v1/messages") {
    plainCalls.push(body);
    return text(`reply to: ${lastUserText(body)}`);
  }
  if (req.url === "/agent/v1/messages") {
    agentCalls.push(body);
    const names = (body.tools || []).map((t) => t.name);
    if (!names.includes("agent_run")) {
      // Inner (sub-agent) loop: the agent tools are absent from its toolset.
      return text("AGENT REPORT: checked every table, all clean.");
    }
    if (hasToolResultFor(body, "AGENT REPORT")) return text("outer done, agent said it is clean");
    return send({ stop_reason: "tool_use", content: [{
      type: "tool_use", id: "ag1", name: "agent_run",
      input: { description: "audit tables", prompt: "Audit every table and report." } }] });
  }
  if (req.url === "/sum/v1/messages") {
    if (String(body.system || "").startsWith("You are a summarizer")) {
      sumCalls.push(body);
      return text("SUMMARY: user asked four things; app unchanged; nothing open.");
    }
    return text(`reply to: ${lastUserText(body)}`);
  }
  if (req.url === "/stall/v1/messages") {
    stallCalls.push(body);
    if (lastUserText(body).includes("STALL")) {
      await new Promise((r) => { releaseStall = r; setTimeout(r, 15000); });
      return text("stalled answer");
    }
    if (releaseStall) releaseStall(); // the other session was served while this one waited
    return text("quick answer");
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

// ---------- instance helper ----------
class Instance {
  async start(device, file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    await this.connect(info.port, new URL(info.url).searchParams.get("t"));
  }
  // A second shell client on an already-running server (multi-tab case).
  async connect(port, token) {
    this.port = port; this.token = token;
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1; this.pending = new Map(); this.aiEvents = [];
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") { if (m.params.type === "ai") this.aiEvents.push(m.params); return; }
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
  async stop() { try { this.ws.close(); } catch {} if (this.proc) { this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); } }
}
const waitFor = async (fn, ms = 25000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(200); } return null; };
// Send and wait for that session's ASSISTANT answer to land. (Only assistant
// rows count: build_messages prefixes later user turns with "[user]: ", so the
// echo the mock sends back is not byte-identical to the question.)
async function sendAndWait(inst, session, text, needle) {
  await inst.rpc("chat.send", { text, session });
  return waitFor(async () => {
    const r = await inst.rpc("chat.list", { session });
    return r.rows.some((row) => row[4] === "assistant" && row[5].includes(needle)) ? r : null;
  });
}

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const A = new Instance(); await A.start("devA", FILE);
await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "test", model: "m", base_url: MOCK } });

console.log("\n[1] sessions: create / rename / list / isolation");
let work;
{
  const created = await A.rpc("sessions.create", { title: "Work" });
  work = created.id;
  ok(/^s-\d+-\d+$/.test(work), `session id shape (${work})`);
  let list = await A.rpc("sessions.list");
  ok(list.some((s) => s.id === "main" && s.title === "Main chat"), "implicit main session listed");
  ok(list.some((s) => s.id === work && s.title === "Work" && s.kind === "chat"), "created session listed");
  await A.rpc("sessions.rename", { id: work, title: "Work stuff" });
  list = await A.rpc("sessions.list");
  ok(list.find((s) => s.id === work).title === "Work stuff", "sessions.rename");

  ok(!!(await sendAndWait(A, "main", "hello main", "reply to: hello main")), "main session answered");
  ok(!!(await sendAndWait(A, work, "hello work", "reply to: hello work")), "second session answered");
  const mainRows = (await A.rpc("chat.list", { session: "main" })).rows;
  const workRows = (await A.rpc("chat.list", { session: work })).rows;
  ok(mainRows.length === 2 && workRows.length === 2, `histories isolated (${mainRows.length}/${workRows.length})`);
  ok(mainRows.every((r) => r[6] === "main") && workRows.every((r) => r[6] === work), "session column returned by chat.list");
  ok(!mainRows.some((r) => r[5].includes("hello work")), "no cross-talk between sessions");
  ok((await A.rpc("chat.list")).rows.length === 2, "chat.list defaults to main");
  const wl = (await A.rpc("sessions.list")).find((s) => s.id === work);
  ok(wl.messages === 2 && wl.last_ts > 0, `message counts in sessions.list (${wl.messages})`);
  // The provider only ever saw one conversation at a time.
  ok(plainCalls.every((c) => c.messages.length === 1), "each session's run sends only its own history");
}

console.log("\n[2] a second shell client (multi-tab) sees the sessions");
const B = new Instance(); await B.connect(A.port, A.token);
{
  const seen = await waitFor(async () => {
    const l = await B.rpc("sessions.list");
    return l.some((s) => s.id === work && s.title === "Work stuff") ? l : null;
  });
  ok(!!seen, "session metadata visible to a second client");
  const rows = await waitFor(async () => {
    const r = await B.rpc("chat.list", { session: work });
    return r.rows.length === 2 ? r : null;
  });
  ok(!!rows, "session messages visible with their session");
}

console.log("\n[3] chat.compact");
await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "test", model: "m", base_url: `${MOCK}/sum` } });
let comp;
{
  const s = (await A.rpc("sessions.create", { title: "Long" })).id;
  for (const n of ["one", "two", "three", "four"]) {
    ok(!!(await sendAndWait(A, s, `question ${n}`, `question ${n}`)), `seeded exchange ${n}`);
  }
  const before = (await A.rpc("chat.list", { session: s })).rows;
  ok(before.length === 8, `8 rows before compaction (${before.length})`);
  comp = await A.rpc("chat.compact", { session: s });
  ok(comp.ok && comp.kept === 4 && comp.summarized_bytes > 0, `compact result ${JSON.stringify(comp)}`);
  ok(sumCalls.length === 1 && sumCalls[0].system.startsWith("You are a summarizer"), "summarizer prompt used");
  ok(String(sumCalls[0].messages[0].content[0].text).includes("question one"), "old turns went into the summarizer");
  const after = (await A.rpc("chat.list", { session: s })).rows;
  ok(after.length === 5, `summary + kept tail (${after.length} rows)`);
  ok(after[0][4] === "summary" && JSON.parse(after[0][5]).text.startsWith("SUMMARY:"), "summary row inserted");
  ok(after[0][0].startsWith("sum-"), "summary mid marks it as a compaction");
  ok(!after.some((r) => r[5].includes("question one")), "compacted turns removed");
  ok(after.some((r) => r[5].includes("question four")), "last exchanges kept");
  const conv = await waitFor(async () => {
    const r = await B.rpc("chat.list", { session: s });
    return r.rows.length === 5 && r.rows[0][4] === "summary" ? r : null;
  });
  ok(!!conv, "second client sees the compacted history");
  // The summary is the model's text, carried in the op — not recomputed.
  ok(conv && JSON.parse(conv.rows[0][5]).text === JSON.parse(after[0][5]).text, "identical summary text on both clients");
  ok(sumCalls.length === 1, "no second summarizer call");
  // A follow-up run now starts from the summary.
  ok(!!(await sendAndWait(A, s, "and five", "and five")), "conversation continues after compaction");
}

console.log("\n[4] sub-agent: agent_run");
await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "test", model: "m", base_url: `${MOCK}/agent` } });
{
  const tools = await A.rpc("tools.list");
  ok(tools.some((t) => t.name === "agent_run") && tools.some((t) => t.name === "agent_send"), "agent tools registered");
  const done = await sendAndWait(A, "main", "please audit the app", "outer done");
  ok(!!done, "outer loop consumed the agent's report");
  const toolRow = done.rows.find((r) => r[4] === "tool" && r[5].includes("AGENT REPORT"));
  ok(!!toolRow, "tool result carries the sub-agent report");
  const result = JSON.parse(toolRow[5]).results[0];
  const payload = JSON.parse(result.content);
  ok(payload.session.startsWith("ag-") && payload.steps >= 1 && payload.report.includes("AGENT REPORT"),
     `agent tool result shape ${JSON.stringify(payload).slice(0, 120)}`);
  const list = await A.rpc("sessions.list");
  const ag = list.find((s) => s.id === payload.session);
  ok(ag && ag.kind === "agent" && ag.parent === "main" && ag.title === "audit tables",
     `agent session recorded ${JSON.stringify(ag)}`);
  const inner = (await A.rpc("chat.list", { session: payload.session })).rows;
  ok(inner.length >= 2 && inner[0][4] === "user" && inner[0][5].includes("Audit every table"),
     "agent conversation seeded with the prompt");
  // The inner request is the one whose toolset lacks the agent tools.
  const innerReq = agentCalls.find((c) => !(c.tools || []).some((t) => t.name === "agent_run"));
  ok(!!innerReq, "sub-agent ran with its own toolset");
  ok(!(innerReq.tools || []).some((t) => t.name === "agent_send"), "agent_send also excluded inside an agent");
  ok((innerReq.tools || []).some((t) => t.name === "sql_query"), "sub-agent keeps the ordinary tools");
  ok(String(innerReq.system).includes("autonomous sub-agent"), "sub-agent system prompt suffix");
  ok(!list.some((s) => s.id === payload.session && s.kind === "chat"), "agent session not a normal chat");
}

console.log("\n[5] per-session runs: one stalled provider does not block another chat");
await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "test", model: "m", base_url: `${MOCK}/stall` } });
{
  const slow = (await A.rpc("sessions.create", { title: "Slow" })).id;
  const fast = (await A.rpc("sessions.create", { title: "Fast" })).id;
  await A.rpc("chat.send", { text: "STALL please", session: slow });
  const busy = await waitFor(async () => {
    const i = await A.rpc("app.info");
    return (i.aiSessions || []).includes(slow) ? i : null;
  }, 8000);
  ok(!!busy && busy.aiBusy, "app.info reports the live session (aiBusy kept for compat)");
  const quick = await sendAndWait(A, fast, "answer now", "quick answer");
  ok(!!quick, "second session answered while the first was stalled");
  const slowDone = await waitFor(async () => {
    const r = await A.rpc("chat.list", { session: slow });
    return r.rows.some((row) => row[5].includes("stalled answer")) ? r : null;
  });
  ok(!!slowDone, "stalled session finished too");
  const idle = await waitFor(async () => {
    const i = await A.rpc("app.info");
    return (i.aiSessions || []).length === 0 ? i : null;
  });
  ok(!!idle && !idle.aiBusy, "runs deregister when finished");
  ok(A.aiEvents.every((e) => typeof e.session === "string"), "every ai event carries its session");
}

console.log("\n[6] scoped clear + delete");
{
  await A.rpc("chat.clear", { session: "main" });
  ok((await A.rpc("chat.list", { session: "main" })).rows.length === 0, "chat.clear only clears its session");
  ok((await A.rpc("chat.list", { session: work })).rows.length === 2, "other sessions untouched by clear");
  await A.rpc("sessions.archive", { id: work, archived: true });
  ok((await A.rpc("sessions.list")).find((s) => s.id === work).archived === true, "sessions.archive");
  await A.rpc("sessions.delete", { id: work });
  const gone = await A.rpc("sessions.list");
  ok(!gone.some((s) => s.id === work), "sessions.delete removes the session");
  ok((await A.rpc("chat.list", { session: work })).rows.length === 0, "its messages went with it");
  const conv = await waitFor(async () => {
    const l = await B.rpc("sessions.list");
    return !l.some((s) => s.id === work) ? l : null;
  });
  ok(!!conv, "deletion replicated to device B");
  let bad = null;
  try { await A.rpc("chat.list", { session: "../etc" }); } catch (e) { bad = e.message; }
  ok(!!bad && bad.includes("bad session id"), "session ids are validated");
}

await A.stop(); await B.stop();
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
