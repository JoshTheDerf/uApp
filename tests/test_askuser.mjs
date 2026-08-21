// ask_user tool: question events, structured multi-question answers,
// timeout, and ai.stop resolving open prompts.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-askuser";
const FILE = `${DIR}/askuser.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---------- mock provider ----------
// First call per conversation returns an ask_user tool_use; once the answer's
// tool_result is in the transcript, echo it back as text so assertions can
// read exactly what the model received.
let calls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const answerResult = (body) => {
  for (const m of body.messages || []) {
    if (!Array.isArray(m.content)) continue;
    for (const b of m.content) {
      if (b.type === "tool_result" && String(b.content).includes("answered")) return String(b.content);
    }
  }
  return null;
};
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  calls.push({ url: req.url, body });
  const send = (obj) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify(obj)); };
  const got = answerResult(body);
  if (got) return send({ stop_reason: "end_turn", content: [{ type: "text", text: `GOT ${got}` }] });
  return send({ stop_reason: "tool_use", content: [{
    type: "tool_use", id: "q1", name: "ask_user",
    input: { questions: [
      { question: "Which database layout?", header: "Layout",
        options: [{ label: "One table (Recommended)", description: "simplest" },
                  { label: "Normalized", description: "3 tables" }] },
      { question: "Which columns matter?", header: "Columns", multiSelect: true,
        options: [{ label: "name" }, { label: "email" }, { label: "phone" }] },
    ] } }] });
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

// ---------- instance helper ----------
class Instance {
  async start(device, file, env = {}) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device, ...env }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    this.port = info.port; this.token = new URL(info.url).searchParams.get("t");
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1; this.pending = new Map(); this.questionEvents = [];
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") { if (m.params.type === "question") this.questionEvents.push(m.params); return; }
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
const lastAssistantText = async (inst, session) => {
  const r = await inst.rpc("chat.list", { session });
  const rows = r.rows.filter((x) => x[4] === "assistant");
  if (!rows.length) return "";
  const c = JSON.parse(rows[rows.length - 1][5]);
  return (c.blocks || []).filter((b) => b.type === "text").map((b) => b.text).join("\n");
};

rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });

console.log("ask_user prompts");
{
  const a = new Instance();
  await a.start("asker", FILE, { UAPP_LINGER_SECS: "never" });
  await a.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "k", base_url: `${MOCK}`, stream: false } });

  // tools.list carries ask_user, ungated.
  const tools = await a.rpc("tools.list");
  const t = tools.find((x) => x.name === "ask_user");
  ok(!!t, "ask_user is in tools.list");
  ok(t && t.gated === false, "ask_user is ungated");

  // 1) A run raises a question: event fans out with the normalized questions.
  await a.rpc("chat.send", { text: "build me a tracker" });
  const q = await waitFor(async () => {
    const list = await a.rpc("ai.questions");
    return list.length ? list[0] : null;
  });
  ok(!!q, "open question appears in ai.questions");
  ok(q && q.questions.length === 2, "both questions arrive", JSON.stringify(q));
  ok(q && q.questions[0].header === "Layout", "header chip preserved");
  ok(q && q.questions[0].options[0].label.includes("Recommended"), "option labels preserved");
  ok(q && q.questions[0].options[0].description === "simplest", "option descriptions preserved");
  ok(q && q.questions[1].multiSelect === true, "multiSelect preserved");
  ok(a.questionEvents.some((e) => (e.pending || []).length > 0), "question event was broadcast");
  ok(q && q.session === "main", "question is stamped with the root session");

  // 2) Answer: single choice + multi choices + free-form other.
  await a.rpc("ai.answer", { id: q.id, answers: [
    { choices: ["One table (Recommended)"] },
    { choices: ["name", "email"], other: "also a notes column" },
  ] });
  const final = await waitFor(async () => {
    const t2 = await lastAssistantText(a, "main");
    return t2.startsWith("GOT") ? t2 : null;
  });
  ok(!!final, "run resumed after the answer");
  ok(final && final.includes('"answered":true'), "result marks answered");
  ok(final && final.includes("One table (Recommended)"), "single choice delivered");
  ok(final && final.includes('"name"') && final.includes('"email"'), "multi choices delivered");
  ok(final && final.includes("also a notes column"), "free-form other delivered");
  ok(final && final.includes("Which database layout?"), "answers are paired with question text");
  const after = await a.rpc("ai.questions");
  ok(after.length === 0, "question is cleared after the answer");

  // 3) Answering a gone question errors cleanly.
  const gone = await a.rpc("ai.answer", { id: q.id, answers: [] }).then(() => null, (e) => e.message);
  ok(gone && gone.includes("no open question"), "answering a closed question errors");

  await a.stop();
}

console.log("timeout and stop");
{
  rmSync(DIR, { recursive: true, force: true });
  mkdirSync(DIR, { recursive: true });
  const a = new Instance();
  await a.start("asker2", FILE, { UAPP_LINGER_SECS: "never", UAPP_ASK_TIMEOUT_MS: "800" });
  await a.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "k", base_url: `${MOCK}`, stream: false } });

  // 4) Timeout: the tool resolves as unanswered and the run continues.
  await a.rpc("chat.send", { text: "quick one" });
  const timed = await waitFor(async () => {
    const t2 = await lastAssistantText(a, "main");
    return t2.includes('"answered":false') ? t2 : null;
  });
  ok(!!timed, "timeout resolves as answered:false");
  ok(timed && timed.includes("no answer within the wait window"), "timeout carries the proceed note");
  await a.stop();
}
{
  rmSync(DIR, { recursive: true, force: true });
  mkdirSync(DIR, { recursive: true });
  const a = new Instance();
  await a.start("asker3", FILE, { UAPP_LINGER_SECS: "never" });
  await a.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "k", base_url: `${MOCK}`, stream: false } });

  // 5) ai.stop resolves the open prompt (as stopped) and clears the list.
  await a.rpc("chat.send", { text: "another" });
  const q = await waitFor(async () => {
    const list = await a.rpc("ai.questions");
    return list.length ? list[0] : null;
  });
  ok(!!q, "prompt open before stop");
  await a.rpc("ai.stop", { session: "main" });
  const cleared = await waitFor(async () => (await a.rpc("ai.questions")).length === 0);
  ok(!!cleared, "ai.stop clears open questions");
  await a.stop();
}

mock.close();
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
