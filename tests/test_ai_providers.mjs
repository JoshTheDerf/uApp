// Provider robustness against a mock server: truncated and overloaded
// streams retry, Retry-After is honoured, Stop interrupts a reply in flight
// without writing it, usage is recorded, and the OpenAI-compatible and
// Responses paths behave.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp-server`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-ai-providers";
const FILE = `${DIR}/p.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

const calls = {};
const log = (k, req, body) => { (calls[k] ||= []).push({ body, headers: req.headers, t: Date.now() }); return calls[k].length; };
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });

const anthEvents = (text, usage = {}) => [
  { type: "message_start", message: { model: "claude-test", usage: { input_tokens: 11, cache_read_input_tokens: 700, cache_creation_input_tokens: 30, output_tokens: 1, ...usage } } },
  { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
  { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
  { type: "content_block_stop", index: 0 },
  { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 9 } },
  { type: "message_stop" },
];
const sse = (res, events) => {
  res.writeHead(200, { "content-type": "text/event-stream" });
  for (const e of events) res.write(`event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`);
};

const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const [, scen] = req.url.split("/");
  if (scen === "trunc") {
    const n = log(scen, req, body);
    if (n === 1) { sse(res, anthEvents("half an answ").slice(0, 3)); return res.end(); }
    sse(res, anthEvents("complete answer")); return res.end();
  }
  if (scen === "over") {
    const n = log(scen, req, body);
    if (n === 1) { sse(res, [anthEvents("")[0], { type: "error", error: { type: "overloaded_error", message: "Overloaded" } }]); return res.end(); }
    sse(res, anthEvents("after overload")); return res.end();
  }
  if (scen === "ratelimit") {
    const n = log(scen, req, body);
    if (n === 1) { res.writeHead(429, { "content-type": "application/json", "retry-after": "2" }); return res.end(JSON.stringify({ error: { message: "slow down" } })); }
    sse(res, anthEvents("after rate limit")); return res.end();
  }
  if (scen === "bad") {
    log(scen, req, body);
    res.writeHead(400, { "content-type": "application/json" });
    return res.end(JSON.stringify({ error: { message: "nope" } }));
  }
  if (scen === "slow") {
    // Starts answering, then keeps the stream open with pings.
    log(scen, req, body);
    sse(res, anthEvents("partial words").slice(0, 3));
    const t = setInterval(() => { try { res.write(`event: ping\ndata: {"type":"ping"}\n\n`); } catch {} }, 300);
    setTimeout(() => { clearInterval(t); try { res.end(); } catch {} }, 15000);
    req.on("close", () => clearInterval(t));
    return;
  }
  if (scen === "nsslow") {
    log(scen, req, body);
    const t = setTimeout(() => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ stop_reason: "end_turn", content: [{ type: "text", text: "too late" }] }));
    }, 8000);
    req.on("close", () => clearTimeout(t));
    return;
  }
  if (scen === "shape") {
    log(scen, req, body);
    sse(res, anthEvents("shape ok")); return res.end();
  }
  if (scen === "oc") {
    log(scen, req, body);
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(`data: ${JSON.stringify({ model: "local-m", choices: [{ delta: { content: "compatible ok" } }] })}\n\n`);
    res.write(`data: ${JSON.stringify({ choices: [{ delta: {}, finish_reason: "stop" }] })}\n\n`);
    res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 50, completion_tokens: 4 } })}\n\n`);
    res.write("data: [DONE]\n\n");
    return res.end();
  }
  if (scen === "long" || scen === "longfail") {
    // One request, many tool rounds of ~6KB each; summarizer calls answered
    // (or refused, for longfail) separately.
    const sys = (body.messages || []).find((m) => m.role === "system");
    const isSum = sys && sys.content.startsWith("You are a summarizer");
    log(isSum ? `${scen}-sum` : scen, req, body);
    res.writeHead(isSum && scen === "longfail" ? 400 : 200, { "content-type": "application/json" });
    if (isSum) return res.end(JSON.stringify(scen === "longfail" ? { error: { message: "no summaries today" } }
      : { choices: [{ message: { content: `SUMMARY ${calls[`${scen}-sum`].length}` } }] }));
    const steps = calls[scen].length;
    const msg = steps < 14
      ? { content: null, tool_calls: [{ id: `r${steps}`, type: "function", function: { name: "read_file", arguments: JSON.stringify({ name: "data/big.txt" }) } }] }
      : { content: "long run done" };
    return res.end(JSON.stringify({ choices: [{ message: msg, finish_reason: "stop" }] }));
  }
  if (scen === "ri") {
    const n = log(scen, req, body);
    res.writeHead(200, { "content-type": "application/json" });
    if (n === 1) return res.end(JSON.stringify({ status: "incomplete", incomplete_details: { reason: "max_output_tokens" },
      output: [{ type: "reasoning", summary: [] }] }));
    return res.end(JSON.stringify({ status: "completed", output: [{ type: "message", content: [{ type: "output_text", text: "responses ok" }] }],
      usage: { input_tokens: 40, output_tokens: 3 } }));
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

class Instance {
  async start(file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    if (info.reused) { console.error("FAIL: stale uapp-server has the test file open"); process.exit(2); }
    this.port = info.port; this.token = new URL(info.url).searchParams.get("t");
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
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error("rpc timeout " + method)); } }, 30000);
    });
  }
  async stop() { this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}
const waitFor = async (fn, ms = 20000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(150); } return null; };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const A = new Instance(); await A.start(FILE);
const rowsOf = async (session) => (await A.rpc("chat.list", { session })).rows;
const idle = (session) => waitFor(() => A.aiEvents.some((e) => e.session === session && (e.state === "idle" || e.state === "error")));
let n = 0;
const send = async (provider, text) => {
  await A.rpc("config.set", { key: "ai", value: provider });
  const { id } = await A.rpc("sessions.create", { title: `t${++n}` });
  await A.rpc("chat.send", { session: id, text });
  return id;
};
const anth = (scen, extra = {}) => ({ provider: "anthropic", api_key: "k", model: "claude-sonnet-5-5", base_url: `${MOCK}/${scen}`, ...extra });

console.log("\n[1] a stream that ends without message_stop is retried, not saved");
{
  const s = await send(anth("trunc"), "hi");
  await idle(s);
  const rows = await rowsOf(s);
  const texts = rows.filter((r) => r[4] === "assistant").map((r) => r[5]);
  ok(calls.trunc.length === 2, `retried once (${calls.trunc.length} calls)`);
  ok(texts.length === 1 && texts[0].includes("complete answer") && !texts[0].includes("half an answ"), "only the complete reply was stored");
  ok(!rows.some((r) => r[4] === "system"), "no error row");
  const content = JSON.parse(texts[0]);
  ok(content.usage && content.usage.input_tokens === 11 && content.usage.output_tokens === 9
     && content.usage.cache_read_tokens === 700 && content.usage.cache_write_tokens === 30, `usage recorded on the row ${JSON.stringify(content.usage)}`);
  ok(content.model === "claude-test", "answering model recorded");
}

console.log("\n[2] overloaded_error mid-stream is retried");
{
  const s = await send(anth("over"), "hi");
  await idle(s);
  const rows = await rowsOf(s);
  ok(calls.over.length === 2, `retried once (${calls.over.length} calls)`);
  ok(rows.some((r) => r[4] === "assistant" && r[5].includes("after overload")), "answer from the retry stored");
}

console.log("\n[3] Retry-After is honoured");
{
  const s = await send(anth("ratelimit"), "hi");
  await idle(s);
  const [a, b] = calls.ratelimit;
  ok(b && b.t - a.t >= 1900, `second attempt waited for Retry-After (${b && b.t - a.t}ms)`);
  ok(A.aiEvents.some((e) => e.session === s && e.state === "retry" && /retrying in 2s/.test(e.message)), "retry announced with the server's delay");
}

console.log("\n[4] a 400 is not retried");
{
  const s = await send(anth("bad"), "hi");
  await idle(s);
  const rows = await rowsOf(s);
  ok(calls.bad.length === 1, `one call (${calls.bad.length})`);
  ok(rows.some((r) => r[4] === "system" && r[5].includes("provider returned 400: nope")), "error surfaced in chat");
}

console.log("\n[5] Stop interrupts a streaming reply and writes nothing");
{
  const s = await send(anth("slow"), "go");
  ok(!!(await waitFor(() => A.aiEvents.some((e) => e.session === s && e.state === "streaming"))), "reply started streaming");
  const t0 = Date.now();
  await A.rpc("ai.stop", { session: s });
  await idle(s);
  ok(Date.now() - t0 < 3000, `run ended promptly (${Date.now() - t0}ms)`);
  const rows = await rowsOf(s);
  ok(!rows.some((r) => r[4] === "assistant"), "no assistant row from the interrupted reply");
  ok(!rows.some((r) => r[4] === "system"), "stop is not reported as an error");
}

console.log("\n[6] Stop interrupts a non-streamed request");
{
  const s = await send({ provider: "zai", api_key: "k", model: "glm-5.3", base_url: `${MOCK}/nsslow` }, "go");
  await waitFor(() => calls.nsslow && calls.nsslow.length);
  await sleep(500);
  const t0 = Date.now();
  await A.rpc("ai.stop", { session: s });
  await idle(s);
  ok(Date.now() - t0 < 3000, `run ended promptly (${Date.now() - t0}ms)`);
  await sleep(500);
  ok(!(await rowsOf(s)).some((r) => r[4] === "assistant"), "no assistant row");
}

console.log("\n[7] Claude request shape: default model, adaptive thinking, effort, binding beta");
{
  const s = await send({ provider: "anthropic", api_key: "k", base_url: `${MOCK}/shape` }, "hi");
  await idle(s);
  const { body, headers } = calls.shape[0];
  ok(body.model === "claude-opus-5-5", `default model (${body.model})`);
  ok(body.thinking && body.thinking.type === "adaptive" && body.thinking.display === "summarized"
     && body.thinking.block_binding.prefix_mismatch_behavior === "drop_block", "adaptive, summarized thinking with drop_block");
  ok((headers["anthropic-beta"] || "").includes("thinking-binding-controls-2026-08-01"), "binding beta header sent");
  ok(body.output_config && body.output_config.effort === "medium", "effort set explicitly");
  ok(body.max_tokens === 64000, `streamed turns get room to think (${body.max_tokens})`);
  ok(body.fallbacks === undefined, "no server-side fallback through a custom base URL");
  ok(A.aiEvents.some((e) => e.session === s && e.state === "streaming"), "streamed");
}

console.log("\n[8] OpenAI-compatible provider");
{
  const s0 = await send({ provider: "openai-compatible", model: "m" }, "hi");
  await idle(s0);
  ok((await rowsOf(s0)).some((r) => r[4] === "system" && r[5].includes("set Base URL")), "missing base URL is a clear error");
  const s = await send({ provider: "openai-compatible", model: "m", base_url: `${MOCK}/oc/`, context_tokens: 32000 }, "hi");
  await idle(s);
  const c = calls.oc[0];
  ok(c.body.stream === true, "streams by default");
  ok(!c.headers.authorization, "no Authorization header without a key");
  ok(c.body.max_tokens === 8192, `default max_tokens (${c.body.max_tokens})`);
  const row = (await rowsOf(s)).find((r) => r[4] === "assistant");
  ok(row && row[5].includes("compatible ok"), "answer stored");
  ok(row && JSON.parse(row[5]).usage.input_tokens === 50, "usage recorded");
}

console.log("\n[9] Responses API: incomplete with no output retries with more room");
{
  const s = await send({ provider: "openai", api_key: "k", model: "gpt-x", base_url: `${MOCK}/ri`, max_tokens: 1000 }, "hi");
  await idle(s);
  ok(calls.ri.length === 2, `retried (${calls.ri.length})`);
  ok(calls.ri[1].body.max_output_tokens === 2000, `budget doubled (${calls.ri[1] && calls.ri[1].body.max_output_tokens})`);
  ok(calls.ri[0].body.stream === true, "streams by default");
  ok((await rowsOf(s)).some((r) => r[4] === "assistant" && r[5].includes("responses ok")), "answer stored");
}

console.log("\n[10] images: oversize refused up front, only recent turns resend theirs");
{
  await A.rpc("config.set", { key: "ai", value: anth("shape") });
  const { id: s } = await A.rpc("sessions.create", { title: "img" });
  const png = (w, h, pad = 0) => {
    const b = Buffer.alloc(33 + pad);
    Buffer.from("89504e470d0a1a0a0000000d49484452", "hex").copy(b, 0);
    b.writeUInt32BE(w, 16); b.writeUInt32BE(h, 20);
    return b.toString("base64");
  };
  let err = "";
  await A.rpc("chat.send", { session: s, text: "huge", attachments: [{ name: "wide.png", b64: png(9000, 10) }] }).catch((e) => (err = e.message));
  ok(/wide\.png can't go to the assistant: it is 9000x10 pixels/.test(err), `too many pixels refused: ${err}`);
  err = "";
  await A.rpc("chat.send", { session: s, text: "heavy", attachments: [{ name: "big.jpg", b64: Buffer.alloc(4_000_000).toString("base64") }] }).catch((e) => (err = e.message));
  ok(/big\.jpg can't go to the assistant: it is 4\.0MB/.test(err), `too many bytes refused: ${err}`);
  const before = calls.shape.length;
  for (let i = 0; i < 4; i++) {
    const done = A.aiEvents.length;
    await A.rpc("chat.send", { session: s, text: `photo ${i}`, attachments: [{ name: `p${i}.png`, b64: png(10, 10) }] });
    await waitFor(() => A.aiEvents.slice(done).some((e) => e.session === s && e.state === "idle"));
  }
  ok(calls.shape.length === before + 4, "four turns answered");
  const last = calls.shape.at(-1).body.messages.flatMap((m) => m.content);
  const images = last.filter((b) => b.type === "image");
  ok(images.length === 3, `three newest turns resend images (${images.length})`);
  ok(last.some((b) => b.type === "text" && /image data\/p0\.png was attached here/.test(b.text)), "the oldest became a note");
}

console.log("\n[11] one long run compacts more than once and keeps the request");
{
  await A.rpc("files.write", { name: "data/big.txt", text: "lorem ipsum ".repeat(500) });
  const cfg = (scen) => ({ provider: "openai-compatible", model: "m", base_url: `${MOCK}/${scen}`, stream: false, max_tokens: 1000, context_tokens: 9000 });
  const s = await send(cfg("long"), "please read the big file over and over");
  await idle(s);
  const rows = await rowsOf(s);
  ok(rows.some((r) => r[4] === "assistant" && r[5].includes("long run done")), "run finished");
  const sums = (calls["long-sum"] || []).length;
  ok(sums >= 2, `compacted more than once (${sums})`);
  const summary = rows.find((r) => r[4] === "summary");
  ok(summary && summary[5].includes("please read the big file over and over"), "summary carries the request word for word");
  const cap = 8000 * 3;
  const sizes = calls.long.map((c) => JSON.stringify(c.body.messages).length);
  ok(Math.max(...sizes) < cap * 1.3, `requests stayed near the budget (max ${Math.max(...sizes)})`);
  ok(calls.long.every((c) => c.body.messages[1].role === "user"), "every request starts with a user turn");

  const s2 = await send(cfg("longfail"), "please read the big file over and over");
  await idle(s2);
  const fails = (calls["longfail-sum"] || []).length;
  ok((await rowsOf(s2)).some((r) => r[5].includes("long run done")), "run still finished without a summarizer");
  ok(fails >= 1 && fails <= 4, `failed summarizer not retried every step (${fails} tries over ${calls.longfail.length} steps)`);
}

await A.stop();
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
