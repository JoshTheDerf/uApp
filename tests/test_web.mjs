// Web tools + MCP self-service: web.fetch text extraction, web.search parsing
// (mock DDG via UAPP_SEARCH_URL), and the AI adding an MCP server mid-run
// with its tools becoming callable in the SAME run.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-web";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- one mock server: web page, fake DDG, MCP endpoint, AI provider ----
let aiCalls = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = req.method === "POST" ? JSON.parse((await readBody(req)) || "{}") : {};
  const send = (o, h = {}) => { res.writeHead(200, { "content-type": "application/json", ...h }); res.end(JSON.stringify(o)); };
  const u = new URL(req.url, "http://x");
  if (u.pathname === "/page") {
    res.writeHead(200, { "content-type": "text/html" });
    return res.end(`<html><head><title>Rate Sheet 2026</title><script>var secret="NOPE";</script>
      <style>.x{color:red}</style></head>
      <body><h1>Shipping rates</h1><p>Zone A costs &amp; fees: $12.50 &lt;per box&gt;</p></body></html>`);
  }
  if (u.pathname === "/search") {
    res.writeHead(200, { "content-type": "text/html" });
    const q = u.searchParams.get("q");
    return res.end(`<html><body>
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frates&rut=x">Example Rates for ${q}</a>
      <a class="result__snippet" href="#">The <b>official</b> rate table.</a>
      <a rel="nofollow" class="result__a" href="https://direct.example.org/doc">Direct Result</a>
      <a class="result__snippet" href="#">A directly linked doc.</a>
      </body></html>`);
  }
  if (u.pathname === "/mcp") {
    if (body.method === "initialize") return send({ jsonrpc: "2.0", id: body.id, result: { protocolVersion: "2025-03-26", capabilities: {}, serverInfo: { name: "m" } } }, { "Mcp-Session-Id": "s1" });
    if (body.method === "notifications/initialized") { res.writeHead(202); return res.end(); }
    if (body.method === "tools/list") return send({ jsonrpc: "2.0", id: body.id, result: { tools: [{ name: "echo", description: "echo", inputSchema: { type: "object", properties: { msg: { type: "string" } } } }] } });
    if (body.method === "tools/call") return send({ jsonrpc: "2.0", id: body.id, result: { content: [{ type: "text", text: `ECHO:${body.params.arguments.msg}` }] } });
  }
  if (u.pathname === "/chat/completions") {
    aiCalls.push(body);
    const tc = (id, name, args) => ({ choices: [{ message: { content: null, tool_calls: [
      { id, type: "function", function: { name, arguments: JSON.stringify(args) } } ] } }] });
    const n = aiCalls.length;
    if (n === 1) return send(tc("w1", "add_mcp_server", { name: "crm", url: `${MOCK}/mcp` }));
    if (n === 2) return send(tc("w2", "mcp__crm__echo", { msg: "mid-run" }));
    return send({ choices: [{ message: { content: "connected and used the server" } }] });
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/w.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "devweb", UAPP_LINGER_SECS: "600",
         UAPP_ALLOW_LOCAL_FETCH: "1", UAPP_SEARCH_URL: `${MOCK}/search?q={}` },
  stdio: ["ignore", "pipe", "pipe"],
});
const line = await new Promise((res) => { let b = ""; proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) res(b.slice(0, i)); }); });
const info = JSON.parse(line);
const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${new URL(info.url).searchParams.get("t")}`);
let id = 1; const pending = new Map();
await new Promise((r) => (ws.onopen = r));
ws.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.method === "event") return;
  const p = pending.get(m.id); if (p) { pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); } };
const rpc = (method, params = {}) => new Promise((resolve, reject) => {
  const i = id++; pending.set(i, { resolve, reject });
  ws.send(JSON.stringify({ id: i, method, params }));
  setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 30000);
});
const waitFor = async (fn, ms = 20000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(250); } return null; };

console.log("\n[1] fetch_url: readable text extraction");
{
  const r = await rpc("web.fetch", { url: `${MOCK}/page` });
  ok(r.title === "Rate Sheet 2026", `title extracted (${r.title})`);
  ok(r.text.includes("Zone A costs & fees: $12.50 <per box>"), "entities decoded, text kept");
  ok(!r.text.includes("NOPE") && !r.text.includes("color:red"), "script/style stripped");
}

console.log("\n[2] web_search: result parsing");
{
  const r = await rpc("web.search", { query: "shipping rates" });
  ok(r.results.length === 2, `parsed ${r.results.length} results`);
  ok(r.results[0].url === "https://example.com/rates", "uddg-wrapped URL decoded");
  ok(r.results[0].title.includes("Example Rates"), "title parsed");
  ok(r.results[0].snippet.includes("official rate table"), "snippet parsed (tags stripped)");
  ok(r.results[1].url === "https://direct.example.org/doc", "direct URL kept");
}

console.log("\n[3] tools.list: web tools ungated, MCP management gated");
{
  const tools = await rpc("tools.list");
  ok(tools.find((t) => t.name === "web_search")?.gated === false, "web_search ungated");
  ok(tools.find((t) => t.name === "fetch_url")?.gated === false, "fetch_url ungated");
  ok(tools.find((t) => t.name === "add_mcp_server")?.gated === true, "add_mcp_server gated");
}

console.log("\n[4] AI adds an MCP server and uses it in the SAME run");
{
  await rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "t", model: "m", base_url: MOCK } });
  await rpc("chat.send", { text: "connect the crm and ping it", mode: "auto" });
  const done = await waitFor(async () => {
    const r = await rpc("chat.list");
    return r.rows.some((row) => row[5].includes("connected and used the server")) ? r : null;
  });
  ok(!!done, "run completed");
  ok(!aiCalls[0].tools.some((t) => t.function.name.startsWith("mcp__")), "no MCP tools before add");
  ok(aiCalls[1].tools.some((t) => t.function.name === "mcp__crm__echo"), "MCP tools appeared mid-run after add_mcp_server");
  const toolMsgs = done.rows.filter((r) => r[4] === "tool").map((r) => JSON.parse(r[5]));
  const echo = toolMsgs.flatMap((c) => c.results).find((r) => r.name === "mcp__crm__echo");
  ok(!!echo && echo.content.includes("ECHO:mid-run"), "new server's tool executed");
  const cfg = await rpc("config.get", { key: "mcp" });
  ok(cfg.length === 1 && cfg[0].name === "crm" && cfg[0].enabled === true, "server saved to replicated config");
}

console.log("\n[5] status / toggle / remove");
{
  const st = await rpc("mcp.status");
  ok(st.length === 1 && st[0].toolCount === 1, "mcp.status reports tool count");
  await rpc("mcp.toggle", { name: "crm", enabled: false });
  const tools = await rpc("tools.list");
  ok(!tools.some((t) => t.name.startsWith("mcp__")), "disabled server's tools hidden");
  const st2 = await rpc("mcp.status");
  ok(st2[0].enabled === false, "status shows disabled");
  await rpc("mcp.remove", { name: "crm" });
  const cfg = await rpc("config.get", { key: "mcp" });
  ok(Array.isArray(cfg) && cfg.length === 0, "server removed from config");
  let err = null;
  try { await rpc("mcp.add", { name: "bad", url: `${MOCK}/nope` }); } catch (e) { err = e; }
  ok(!!err && err.message.includes("not saved"), "unreachable server rejected before saving");
}

proc.kill("SIGINT"); await sleep(300); proc.kill("SIGKILL");
mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
