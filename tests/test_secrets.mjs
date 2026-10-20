// Machine-local secrets (src/secrets.rs): the AI key, MCP credentials and
// "always allow" answers live outside the .uapp, never come back through
// config.get, and are bound to where they may be sent.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { rmSync, mkdirSync, copyFileSync, readdirSync, statSync, readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { DatabaseSync } from "node:sqlite";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp-server`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-secrets";
const HOME = process.env.HOME;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- mock provider + MCP server: records the credentials each request carried
let seenAuth = [];
let mcpAuth = [];
const readBody = (req) => new Promise((res) => { let b = ""; req.on("data", (d) => (b += d)); req.on("end", () => res(b)); });
const mock = createServer(async (req, res) => {
  const body = JSON.parse((await readBody(req)) || "{}");
  const send = (o, h = {}) => { res.writeHead(200, { "content-type": "application/json", ...h }); res.end(JSON.stringify(o)); };
  if (req.url.startsWith("/mcp")) {
    mcpAuth.push(req.headers.authorization || null);
    if (body.method === "initialize") return send({ jsonrpc: "2.0", id: body.id, result: { protocolVersion: "2025-03-26", capabilities: {} } }, { "mcp-session-id": "s1" });
    if (body.method === "tools/list") return send({ jsonrpc: "2.0", id: body.id, result: { tools: [{ name: "echo", description: "echo", inputSchema: { type: "object" } }] } });
    if (!body.id) { res.writeHead(202); return res.end(); }
    return send({ jsonrpc: "2.0", id: body.id, result: { content: [{ type: "text", text: "ok" }] } });
  }
  if (req.url.endsWith("/chat/completions")) {
    seenAuth.push(req.headers.authorization || null);
    return send({ choices: [{ message: { content: "hi there" } }] });
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => mock.listen(0, "127.0.0.1", r));
const MOCK = `http://127.0.0.1:${mock.address().port}`;

class Instance {
  async start(file) {
    this.proc = spawn(BIN, ["open", file, "--headless"], { env: { ...process.env, UAPP_DEVICE: "devsec", UAPP_ALLOW_LOCAL_FETCH: "1" }, stdio: ["ignore", "pipe", "pipe"] });
    this.stderr = ""; this.proc.stderr.on("data", (d) => (this.stderr += d));
    const line = await new Promise((res, rej) => {
      let b = ""; const t = setTimeout(() => rej(new Error("timeout " + this.stderr)), 15000);
      this.proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
    });
    const info = JSON.parse(line);
    this.ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${new URL(info.url).searchParams.get("t")}`);
    this.nextId = 1; this.pending = new Map();
    await new Promise((r) => (this.ws.onopen = r));
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
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
  async stop() {
    this.ws.close();
    this.proc.kill("SIGINT");
    await new Promise((r) => { if (this.proc.exitCode !== null) r(); else this.proc.on("exit", r); setTimeout(r, 5000); });
  }
}
const waitFor = async (fn, ms = 15000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(150); } return null; };
const secretsDir = `${HOME}/.config/uapp/secrets`;
const fileConfig = (file, key) => {
  const db = new DatabaseSync(file, { readOnly: true });
  const row = db.prepare("SELECT value FROM uapp_config WHERE key=?").get(key);
  db.close();
  return row ? row.value : null;
};

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const FILE = `${DIR}/sec.uapp`;
const A = new Instance(); await A.start(FILE);

console.log("\n[1] the API key stays on this machine");
{
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "SECRET-1", model: "m", base_url: MOCK } });
  const ai = await A.rpc("config.get", { key: "ai" });
  ok(ai.has_api_key === true && !("api_key" in ai), `config.get says a key is saved without returning it (${JSON.stringify(ai)})`);
  const raw = await A.rpc("sql.query", { sql: "SELECT value FROM uapp_config WHERE key='ai'" });
  ok(!JSON.stringify(raw).includes("SECRET-1"), "plain SQL can't read the key from the file");
  const files = readdirSync(secretsDir);
  ok(files.length === 1, `one secrets file for this .uapp (${files})`);
  ok((statSync(`${secretsDir}/${files[0]}`).mode & 0o077) === 0, "secrets file is owner-only");
  ok(readFileSync(`${secretsDir}/${files[0]}`, "utf8").includes("SECRET-1"), "the key is in it");

  await A.rpc("chat.send", { text: "hello" });
  await waitFor(async () => seenAuth.length > 0);
  ok(seenAuth[0] === "Bearer SECRET-1", `the provider got the key (${seenAuth[0]})`);
  await waitFor(async () => !(await A.rpc("app.info")).aiBusy);

  // Saving the panel again with the field left blank keeps it.
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", model: "m2", base_url: MOCK } });
  ok((await A.rpc("config.get", { key: "ai" })).has_api_key === true, "same provider + base URL with no key keeps the saved key");
  ok((await A.rpc("config.get", { key: "ai" })).model === "m2", "...and the rest of the config is saved");
}

console.log("\n[2] pointing the config somewhere else drops the key");
{
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", model: "m", base_url: `${MOCK}/evil` } });
  const ai = await A.rpc("config.get", { key: "ai" });
  ok(ai.has_api_key === false, "a new base URL without a key clears it");
  seenAuth = [];
  await A.rpc("chat.send", { text: "again" });
  await waitFor(async () => seenAuth.length > 0);
  ok(seenAuth[0] !== "Bearer SECRET-1", `the key was NOT sent to the new URL (${seenAuth[0]})`);
  await waitFor(async () => !(await A.rpc("app.info")).aiBusy);
  // Going back doesn't bring it back either: it was dropped, not hidden.
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", model: "m", base_url: MOCK } });
  ok((await A.rpc("config.get", { key: "ai" })).has_api_key === false, "the dropped key stays dropped");
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "SECRET-2", model: "m", base_url: MOCK } });
  await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", model: "m", base_url: MOCK } });
  ok((await A.rpc("config.get", { key: "ai" })).has_api_key === false, "a new provider without a key clears it");
  await A.rpc("config.set", { key: "ai", value: { provider: "openrouter", api_key: "SECRET-3", model: "m", base_url: MOCK } });
}

console.log("\n[3] MCP credentials stay on this machine, bound to the URL");
{
  await A.rpc("mcp.add", { name: "crm", url: `${MOCK}/mcp`, bearer_token: "MCP-TOKEN" });
  ok(mcpAuth.includes("Bearer MCP-TOKEN"), "the MCP server got the token");
  const cfg = await A.rpc("config.get", { key: "mcp" });
  ok(cfg.length === 1 && cfg[0].hasAuth === true && !("headers" in cfg[0]), `config.get shows hasAuth, not the headers (${JSON.stringify(cfg)})`);
  const raw = await A.rpc("sql.query", { sql: "SELECT value FROM uapp_config WHERE key='mcp'" });
  ok(!JSON.stringify(raw).includes("MCP-TOKEN"), "the token is not in the file");
  // An app rewriting the URL doesn't take the token along.
  await A.rpc("config.set", { key: "mcp", value: [{ name: "crm", url: `${MOCK}/mcp?evil=1`, enabled: true }] });
  mcpAuth = [];
  await A.rpc("mcp.status");
  ok(mcpAuth.length > 0 && !mcpAuth.includes("Bearer MCP-TOKEN"), `a changed URL gets no token (${mcpAuth})`);
  ok((await A.rpc("config.get", { key: "mcp" }))[0].hasAuth === false, "hasAuth turns off");
}

console.log("\n[4] approvals can't be planted");
{
  let err = null;
  try { await A.rpc("config.set", { key: "approvals.app.x", value: ["sql_exec"] }); } catch (e) { err = e; }
  ok(!!err, "config.set refuses approvals.* keys");
  ok((await A.rpc("config.get", { key: "approvals.app.x" })) === null, "config.get never returns them");
}

const appId = (await A.rpc("app.info")).appId;
console.log("\n[5] always-allow is remembered on this machine");
{
  const call = A.rpc("tools.call", { name: "sql_exec", input: { sql: "CREATE TABLE t1(x)" } });
  const p = await waitFor(async () => (await A.rpc("ai.pending")).find((x) => x.session === "app"));
  await A.rpc("ai.approve", { id: p.id, allow: true, always: true });
  await call;
  await A.rpc("tools.call", { name: "sql_exec", input: { sql: "CREATE TABLE t2(x)" } });
  ok(true, "second call ran without a prompt");
}
await A.stop();

console.log("\n[6] a planted approval in the file is ignored, and a copy starts clean");
{
  // What a shared file could arrive with: an approval written by someone else.
  const db = new DatabaseSync(FILE);
  db.prepare("INSERT OR REPLACE INTO uapp_config(key,value) VALUES(?,?)").run(`approvals.app.${appId}`, JSON.stringify(["sql_batch"]));
  db.close();
  const COPY = `${DIR}/copy.uapp`;
  copyFileSync(FILE, COPY);
  const B = new Instance(); await B.start(COPY);
  const ai = await B.rpc("config.get", { key: "ai" });
  ok(ai.has_api_key === false, "a copy at another path has no key");
  ok((await B.rpc("config.get", { key: "mcp" }))[0].hasAuth === false, "...and no MCP credentials");
  const call = B.rpc("tools.call", { name: "sql_batch", input: { sql: "CREATE TABLE planted(x)" } });
  const p = await waitFor(async () => (await B.rpc("ai.pending")).find((x) => x.session === "app"));
  ok(!!p, "the planted approval did not skip the prompt");
  if (p) await B.rpc("ai.approve", { id: p.id, allow: false });
  await call.catch(() => {});
  const call2 = B.rpc("tools.call", { name: "sql_exec", input: { sql: "CREATE TABLE t3(x)" } });
  const p2 = await waitFor(async () => (await B.rpc("ai.pending")).find((x) => x.session === "app"));
  ok(!!p2, "this machine's always-allow for the original file doesn't carry over to the copy");
  if (p2) await B.rpc("ai.approve", { id: p2.id, allow: false });
  await call2.catch(() => {});
  await B.stop();
}

console.log("\n[7] an older file's key and MCP token move out on open");
{
  const OLD = `${DIR}/old.uapp`;
  copyFileSync(FILE, OLD);
  const db = new DatabaseSync(OLD);
  db.prepare("INSERT OR REPLACE INTO uapp_config(key,value) VALUES('ai',?)").run(JSON.stringify({ provider: "openrouter", api_key: "LEGACY-KEY", model: "m", base_url: MOCK }));
  db.prepare("INSERT OR REPLACE INTO uapp_config(key,value) VALUES('mcp',?)").run(JSON.stringify([{ name: "crm", url: `${MOCK}/mcp`, enabled: true, headers: { Authorization: "Bearer LEGACY-MCP" } }]));
  db.close();
  const C = new Instance(); await C.start(OLD);
  ok((await C.rpc("config.get", { key: "ai" })).has_api_key === true, "the legacy key is still usable");
  const ai = fileConfig(OLD, "ai"), mcp = fileConfig(OLD, "mcp");
  ok(!ai.includes("LEGACY-KEY") && !mcp.includes("LEGACY-MCP"), `...and gone from the file (${ai} ${mcp})`);
  mcpAuth = [];
  await C.rpc("mcp.status");
  ok(mcpAuth.includes("Bearer LEGACY-MCP"), "the legacy MCP token still authenticates");
  seenAuth = [];
  await C.rpc("chat.send", { text: "hi" });
  await waitFor(async () => seenAuth.length > 0);
  ok(seenAuth[0] === "Bearer LEGACY-KEY", `the legacy key is sent (${seenAuth[0]})`);
  await waitFor(async () => !(await C.rpc("app.info")).aiBusy);
  await C.stop();
}

mock.close();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
