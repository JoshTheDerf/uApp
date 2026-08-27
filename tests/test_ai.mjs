// AI tool-loop test with mock providers + mock remote MCP server.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-ai";
const FILE = `${DIR}/ai.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---------- mock provider + MCP ----------
let oaCalls = [], anthCalls = [], mcpCalls = [], thinkCalls = [], nsCalls = [], respCalls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });

const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (obj, headers = {}) => {
    res.writeHead(200, { "content-type": "application/json", ...headers });
    res.end(JSON.stringify(obj));
  };
  if (req.url === "/chat/completions") {
    oaCalls.push(body);
    const n = oaCalls.length;
    const tc = (id, name, args) => ({ id, type: "function", function: { name, arguments: JSON.stringify(args) } });
    let msg;
    if (n === 1) msg = { content: "Building it now.", tool_calls: [
      tc("c1", "sql_batch", { sql: "CREATE TABLE IF NOT EXISTS notes(id INTEGER PRIMARY KEY, body TEXT, created INTEGER);" }) ] };
    else if (n === 2) msg = { content: null, tool_calls: [
      tc("c2", "write_file", { name: "index.html", content: "<html><body><h1>Notes App</h1><script src=\"/uapp.js\"></script></body></html>" }) ] };
    else if (n === 3) msg = { content: null, tool_calls: [ tc("c3", "mcp__crm__echo", { msg: "hello from model" }) ] };
    else if (n === 4) msg = { content: "All done — notes app is ready!" };
    // Section [5]: edit_file — surgical replace, bad match, replace_all.
    else if (n === 5) msg = { content: null, tool_calls: [ tc("e1", "edit_file", { name: "index.html", old_string: "<h1>Notes App</h1>", new_string: "<h1>Notes App v2</h1>" }) ] };
    else if (n === 6) msg = { content: null, tool_calls: [ tc("e2", "edit_file", { name: "index.html", old_string: "no such text anywhere", new_string: "x" }) ] };
    else if (n === 7) msg = { content: null, tool_calls: [ tc("e3", "edit_file", { name: "index.html", old_string: "body", new_string: "BODY", replace_all: true }) ] };
    else if (n === 8) msg = { content: null, tool_calls: [ tc("e4", "edit_file", { name: "index.html", old_string: "BODY", new_string: "flurp" }) ] }; // ambiguous without replace_all
    else if (n === 9) msg = { content: null, tool_calls: [ tc("e5", "edit_file", { name: "index.html", old_string: "<h1>", new_string: "<h1>" }) ] }; // no-op: identical
    else msg = { content: "edits complete" };

    // Local (section [6]) requests stream:true — reply as an OpenAI SSE stream,
    // splitting each tool call's id/name from its arguments across deltas (as
    // real servers do) to exercise the fragment-assembly in read_openai_sse.
    // Everything else keeps the plain JSON reply.
    if (body.stream) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      const ev = (o) => res.write(`data: ${JSON.stringify(o)}\n\n`);
      if (msg.content) ev({ choices: [{ delta: { content: msg.content } }] });
      (msg.tool_calls || []).forEach((t, i) => {
        ev({ choices: [{ delta: { tool_calls: [{ index: i, id: t.id, type: "function", function: { name: t.function.name, arguments: "" } }] } }] });
        ev({ choices: [{ delta: { tool_calls: [{ index: i, function: { arguments: t.function.arguments } }] } }] });
      });
      ev({ choices: [{ delta: {}, finish_reason: msg.tool_calls ? "tool_calls" : "stop" }] });
      res.write("data: [DONE]\n\n");
      return res.end();
    }
    return send({ choices: [{ message: msg }] });
  }
  if (req.url === "/think/v1/messages") {
    thinkCalls.push(body);
    res.writeHead(200, { "content-type": "text/event-stream" });
    const ev = (o) => res.write(`data: ${JSON.stringify(o)}\n\n`);
    ev({ type: "message_start" });
    if (thinkCalls.length === 1) {
      // Entire budget consumed by reasoning — no text, no tool call.
      ev({ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } });
      ev({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "hmm ".repeat(50) } });
      ev({ type: "content_block_stop", index: 0 });
      ev({ type: "message_delta", delta: { stop_reason: "max_tokens" } });
    } else {
      ev({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
      ev({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "recovered after retry" } });
      ev({ type: "content_block_stop", index: 0 });
      ev({ type: "message_delta", delta: { stop_reason: "end_turn" } });
    }
    ev({ type: "message_stop" });
    return res.end();
  }
  if (req.url === "/ns/v1/messages") {
    // Non-streaming JSON replies (what z.ai actually does well).
    nsCalls.push(body);
    const n = nsCalls.length;
    if (n === 1)
      return send({ stop_reason: "tool_use", content: [
        { type: "thinking", thinking: "plan v1", signature: "s1" },
        { type: "tool_use", id: "t1", name: "get_schema", input: {} } ] });
    if (n === 2)
      return send({ stop_reason: "tool_use", content: [
        { type: "thinking", thinking: "plan v2", signature: "s2" },
        { type: "tool_use", id: "t2", name: "get_schema", input: {} } ] });
    return send({ stop_reason: "end_turn", content: [{ type: "text", text: "all done non-streaming" }] });
  }
  if (req.url === "/v1/messages") {
    anthCalls.push(body);
    const n = anthCalls.length;
    if (n === 1) {
      // Real SSE stream: thinking + streamed text + tool_use via json deltas.
      res.writeHead(200, { "content-type": "text/event-stream" });
      const ev = (o) => res.write(`data: ${JSON.stringify(o)}\n\n`);
      ev({ type: "message_start" });
      ev({ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } });
      ev({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "need the schema" } });
      ev({ type: "content_block_delta", index: 0, delta: { type: "signature_delta", signature: "sig123" } });
      ev({ type: "content_block_stop", index: 0 });
      ev({ type: "content_block_start", index: 1, content_block: { type: "text", text: "" } });
      ev({ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "Checking " } });
      ev({ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "the schema." } });
      ev({ type: "content_block_stop", index: 1 });
      ev({ type: "content_block_start", index: 2, content_block: { type: "tool_use", id: "a1", name: "get_schema", input: {} } });
      ev({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: "{}" } });
      ev({ type: "content_block_stop", index: 2 });
      ev({ type: "message_delta", delta: { stop_reason: "tool_use" } });
      ev({ type: "message_stop" });
      return res.end();
    }
    // Non-streamed JSON despite stream:true — exercises the fallback path.
    return send({ content: [{ type: "text", text: "Schema looks good (anthropic path)." }] });
  }
  if (req.url === "/responses") {
    // OpenAI's newer Responses API shape: flat function_call/message items
    // in `output`, tool results replayed back as function_call_output.
    respCalls.push(body);
    const n = respCalls.length;
    if (n === 1)
      return send({ status: "completed", output: [
        { type: "function_call", call_id: "r1", name: "get_schema", arguments: "{}" } ] });
    return send({ status: "completed", output: [
      { type: "message", role: "assistant", content: [{ type: "output_text", text: "done via responses api" }] } ] });
  }
  if (req.url === "/mcp") {
    mcpCalls.push(body);
    if (body.method === "initialize")
      return send({ jsonrpc: "2.0", id: body.id, result: { protocolVersion: "2025-03-26", capabilities: {}, serverInfo: { name: "mock" } } }, { "Mcp-Session-Id": "sess-1" });
    if (body.method === "notifications/initialized") { res.writeHead(202); return res.end(); }
    if (body.method === "tools/list")
      return send({ jsonrpc: "2.0", id: body.id, result: { tools: [{ name: "echo", description: "echo back", inputSchema: { type: "object", properties: { msg: { type: "string" } } } }] } });
    if (body.method === "tools/call")
      return send({ jsonrpc: "2.0", id: body.id, result: { content: [{ type: "text", text: `ECHO:${body.params.arguments.msg}` }] } });
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

// ---------- instance helper (trimmed copy) ----------
class Instance {
  async start(device, file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: device }, stdio: ["ignore", "pipe", "pipe"] });
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
    this.aiEvents = [];
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
const waitFor = async (fn, ms = 20000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(300); } return null; };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const A = new Instance(); await A.start("devai", FILE);

console.log("\n[1] OpenAI-compatible provider (z.ai/OpenRouter shape) with MCP tool");
await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "test", model: "glm-4.6", base_url: MOCK } });
await A.rpc("config.set", { key: "mcp", value: [{ name: "crm", url: `${MOCK}/mcp` }] });
{
  const tools = await A.rpc("mcp.tools");
  ok(tools.length === 1 && tools[0].name === "echo", "mcp.tools lists remote MCP tools");
  await A.rpc("chat.send", { text: "build me a notes app" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("All done")) ? r : null;
  });
  ok(!!done, "tool loop ran to completion");
  const q = await A.rpc("sql.query", { sql: "SELECT name FROM sqlite_master WHERE name='notes'" });
  ok(q.rows.length === 1, "sql_batch tool created the table");
  const f = await A.rpc("files.read", { name: "index.html" });
  ok(f.text && f.text.includes("Notes App"), "write_file tool wrote index.html");
  const toolRows = done.rows.filter((r) => r[4] === "tool");
  ok(toolRows.some((r) => r[5].includes("ECHO:hello from model")), "remote MCP tool call executed and result returned");
  const sys = oaCalls[0].messages.find((m) => m.role === "system");
  ok(sys && sys.content.includes("DETERMINISM"), "system prompt sent");
  ok(oaCalls.length === 4, `loop iterations: ${oaCalls.length} provider calls`);
  const toolMsg = oaCalls[1].messages.find((m) => m.role === "tool");
  ok(!!toolMsg && toolMsg.tool_call_id === "c1", "tool results threaded back in OpenAI format");
}

console.log("\n[2] Anthropic provider path");
await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "test", model: "claude-sonnet-5", base_url: MOCK } });
{
  await A.rpc("chat.send", { text: "check the schema" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("anthropic path")) ? r : null;
  });
  ok(!!done, "anthropic loop completed");
  const req = anthCalls[1];
  ok(req && req.messages.some((m) => Array.isArray(m.content) && m.content.some((b) => b.type === "tool_result" && b.tool_use_id === "a1")),
     "tool_result threaded back in Anthropic format");
  ok(typeof req.system === "string" && req.system.includes("uapp"), "anthropic system prompt");
  const hist = await A.rpc("files.history", { name: "index.html" });
  ok(hist.rows.length >= 1, "AI file write recorded in history (rollback available)");
  const stream = A.aiEvents.filter((e) => e.state === "streaming");
  ok(stream.length >= 1 && stream.at(-1).text.includes("Checking the schema."),
     "streamed text pushed to shell as it arrived");
  const chat = await A.rpc("chat.list");
  const think = chat.rows.filter((r) => r[4] === "assistant")
    .flatMap((r) => (JSON.parse(r[5]).blocks || []))
    .find((b) => b.type === "thinking" && b.signature === "sig123");
  ok(!!think && think.thinking.includes("need the schema"), "streamed thinking block assembled with signature");
}

console.log("\n[3] thinking-exhaustion: auto-retry with doubled budget (explicit cap)");
await A.rpc("config.set", { key: "ai", value: { provider: "zai", api_key: "test", model: "glm-5.3", max_tokens: 4096, base_url: `${MOCK}/think` } });
{
  await A.rpc("chat.send", { text: "think hard about this" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("recovered after retry")) ? r : null;
  });
  ok(!!done, "run recovered via automatic retry");
  ok(thinkCalls.length === 2, `retried exactly once (${thinkCalls.length} calls)`);
  ok(thinkCalls[0].max_tokens === 4096, `explicit budget sent (got ${thinkCalls[0].max_tokens})`);
  ok(thinkCalls[1].max_tokens === 8192, `retry doubled the budget (got ${thinkCalls[1].max_tokens})`);
  const sys = done.rows.filter((r) => r[4] === "system");
  ok(sys.length === 0, "no error surfaced to chat");
}

console.log("\n[4] zai: non-streaming by default + stale thinking stripped from history");
await A.rpc("config.set", { key: "ai", value: { provider: "zai", api_key: "test", model: "glm-5.3", base_url: `${MOCK}/ns` } });
{
  await A.rpc("chat.send", { text: "use the schema twice" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("all done non-streaming")) ? r : null;
  });
  ok(!!done, "non-streaming zai tool loop completed");
  ok(nsCalls.length === 3, `three provider calls (${nsCalls.length})`);
  ok(nsCalls.every((c) => c.stream === undefined), "zai requests are non-streaming by default (no stream flag)");
  ok(nsCalls.every((c) => c.max_tokens === undefined), "zai requests omit max_tokens by default (a present cap becomes the thinking budget)");
  ok(anthCalls[0].stream === true, "anthropic requests still stream");
  const thinking = nsCalls[2].messages
    .filter((m) => m.role === "assistant")
    .flatMap((m) => m.content.filter((b) => b.type === "thinking"));
  ok(thinking.length === 1 && thinking[0].thinking === "plan v2",
     `only the final assistant turn's thinking is replayed (got ${thinking.length})`);
}

console.log("\n[5] edit_file: surgical replace, miss reported, replace_all");
await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "test", model: "glm-4.6", base_url: MOCK } });
{
  await A.rpc("chat.send", { text: "tweak the heading" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("edits complete")) ? r : null;
  });
  ok(!!done, "edit_file loop completed");
  const f = await A.rpc("files.read", { name: "index.html" });
  ok(f.text.includes("<h1>Notes App v2</h1>"), "exact replacement applied in place");
  ok(f.text.includes("<BODY>") && f.text.includes("</BODY>") && !f.text.includes("<body>"),
     "replace_all replaced every occurrence");
  const toolRows = done.rows.filter((r) => r[4] === "tool").map((r) => r[5]);
  ok(toolRows.some((t) => t.includes("not found") && t.includes("is_error")),
     "missing old_string surfaced as a tool error, not a crash");
  ok(toolRows.some((t) => t.includes("replacements")), "successful edit reports replacement count");
  ok(toolRows.some((t) => t.includes("appears 2 times") && t.includes("is_error")),
     "ambiguous match without replace_all rejected with count + guidance");
  ok(toolRows.some((t) => t.includes("identical") && t.includes("is_error")),
     "old_string == new_string rejected");
  ok(!f.text.includes("flurp"), "ambiguous edit changed nothing");
  const hist = await A.rpc("files.history", { name: "index.html" });
  ok(hist.rows.length >= 2, "each edit kept a history version");
}

console.log("\n[6] local provider: requires an explicit model, otherwise reuses the OpenAI-compat path");
{
  await A.rpc("config.set", { key: "ai", value: { provider: "local", api_key: "", base_url: MOCK } });
  const before = await A.rpc("chat.list");
  await A.rpc("chat.send", { text: "no model set" });
  const errored = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.length > before.rows.length && r.rows.some((row) => row[4] === "system") ? r : null;
  });
  ok(!!errored, "run finished (with an error) rather than hanging");
  const err = errored.rows.filter((r) => r[4] === "system").at(-1);
  ok(!!err && err[5].includes("set Model in Settings"), "empty model on 'local' surfaces a clear error");

  await A.rpc("config.set", { key: "ai", value: { provider: "local", api_key: "", model: "qwen3.8", base_url: MOCK } });
  const before2 = await A.rpc("chat.list");
  await A.rpc("chat.send", { text: "model is set now" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.length > before2.rows.length ? r : null;
  });
  ok(!!done, "run completed");
  ok(!done.rows.slice(before2.rows.length).some((r) => r[4] === "system"),
     "no error once a model is set — same OpenAI-compat path as openrouter");
  ok(oaCalls.at(-1).stream === true, "local requests stream by default (slow local models look stalled otherwise)");
}

console.log("\n[7] openai provider: newer Responses API (flat tools, function_call/output items)");
await A.rpc("config.set", { key: "ai", value: { provider: "openai", api_key: "test", model: "gpt-5.1", base_url: MOCK } });
{
  await A.rpc("chat.send", { text: "check the schema via responses api" });
  const done = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    return r.rows.some((row) => row[5].includes("done via responses api")) ? r : null;
  });
  ok(!!done, "responses-api loop completed");
  ok(respCalls.length === 2, `two provider calls (${respCalls.length})`);
  ok(typeof respCalls[0].instructions === "string" && respCalls[0].instructions.length > 0,
     "system prompt sent as top-level instructions, not an input item");
  const tool = respCalls[0].tools[0];
  ok(tool.type === "function" && typeof tool.name === "string" && !tool.function,
     "tools are flat (no nested 'function' key, unlike chat/completions)");
  // The whole chat session's history accumulates across every earlier test
  // section, so pick the LAST function_call_output — the one from this turn.
  const out = (respCalls[1].input || []).filter((i) => i.type === "function_call_output").at(-1);
  ok(!!out && out.call_id === "r1", "tool result threaded back as function_call_output");
}

await A.stop();
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
