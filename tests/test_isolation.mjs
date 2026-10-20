// App isolation: the app frame is served from its own origin
// (http://localhost:<port>) with its own token, and its connections get the
// app role. Checks the routes on each origin, the socket's role, the RPC
// allowlist, the event split, that an app's tool prompt can only be answered
// by the owner, and that [::1] serves the app origin too.
import { spawn } from "node:child_process";
import { request } from "node:http";
import { lookup } from "node:dns/promises";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || `${process.cwd()}/target/debug/uapp`;
const DIR = process.env.TESTDIR || "/tmp/uapp-test-isolation";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/iso.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "deviso", UAPP_LINGER_SECS: "600" },
  stdio: ["ignore", "pipe", "pipe"],
});
let stderr = ""; proc.stderr.on("data", (d) => (stderr += d));
const info = JSON.parse(await new Promise((res, rej) => {
  let b = ""; const t = setTimeout(() => rej(new Error("no startup line. stderr: " + stderr)), 15000);
  proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
}));
if (info.reused) { console.error("FAIL: a uapp from an earlier run still has the test file open (old code!); stop that uapp process first"); process.exit(2); }
const P = info.port;
const T = new URL(info.url).searchParams.get("t");
const SHELL = `http://127.0.0.1:${P}`, APP = `http://localhost:${P}`;

/// One HTTP request with exactly these headers (Host included), to `addr`.
function http(addr, path, headers = {}, method = "GET", body = null) {
  return new Promise((resolve, reject) => {
    const r = request({ host: addr, port: P, path, method, headers, family: addr.includes(":") ? 6 : 4 }, (res) => {
      let b = ""; res.on("data", (d) => (b += d));
      res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body: b }));
    });
    r.on("error", reject);
    if (body) r.write(body);
    r.end();
  });
}
const onApp = (path, h = {}, m, b) => http("127.0.0.1", path, { Host: `localhost:${P}`, ...h }, m, b);
const onShell = (path, h = {}, m, b) => http("127.0.0.1", path, { Host: `127.0.0.1:${P}`, ...h }, m, b);

function client(url, headers) {
  const ws = new WebSocket(url, headers ? { headers } : undefined);
  const pending = new Map(); const events = []; let id = 1;
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event") { events.push(m.params); return; }
    const p = pending.get(m.id);
    if (p) { pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
  };
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 20000);
  });
  const opened = new Promise((res) => { ws.onopen = () => res(true); ws.onerror = () => res(false); });
  return { ws, rpc, events, opened };
}
const refused = async (c, method, params) => {
  try { await c.rpc(method, params); return false; } catch (e) { return /only available to the uApp editor|start with "app\."/.test(e.message); }
};
const waitFor = async (fn, ms = 10000) => { const e = Date.now() + ms; while (Date.now() < e) { try { const v = await fn(); if (v) return v; } catch {} await sleep(100); } return null; };

try {
  console.log("\n[1] the shell names the app's origin and token");
  const shell = await onShell(`/?t=${T}`);
  const src = (/id="appframe" src="([^"]+)"/.exec(shell.body) || [])[1] || "";
  const A = new URL(src, SHELL).searchParams.get("a") || "";
  ok(src.startsWith(`${APP}/app/?a=`) && /^[0-9a-f]{64}$/.test(A) && A !== T, "frame src is the app origin with its own token", src);
  ok((shell.headers["content-security-policy"] || "").includes(`frame-src 'self' ${APP}`), "shell CSP frame-src names the app origin");

  console.log("\n[2] app-origin routes");
  {
    const r = await onApp(`/app/?a=${A}`);
    ok(r.status === 200 && !r.body.includes("shell-topbar"), "app page with the token");
    ok(r.headers["content-security-policy"] === `frame-ancestors 'self' ${SHELL}`, "only our shell may frame it", r.headers["content-security-policy"]);
    ok(String(r.headers["set-cookie"] || "").includes(`uapp_a_${P}=${A}`) && /HttpOnly/.test(r.headers["set-cookie"]) && /SameSite=Strict/.test(r.headers["set-cookie"]), "app cookie set");
    ok((await onApp("/app/")).status === 403, "no token, no fetch metadata: refused");
    ok((await onApp("/app/", { "Sec-Fetch-Site": "cross-site" })).status === 403, "cross-site without the token: refused");
    ok((await onApp("/app/", { "Sec-Fetch-Site": "same-site" })).status === 403, "same-site (another localhost port) without the token: refused");
    ok((await onApp("/app/", { "Sec-Fetch-Site": "same-origin" })).status === 200, "the app's own same-origin request");
    ok((await onApp("/app/", { Referer: `${APP}/app/` })).status === 200, "older engine: a Referer on the app origin");
    ok((await onApp("/app/", { Referer: `http://localhost:${P + 1}/` })).status === 403, "a Referer from another port: refused");
    ok((await onApp(`/app/?a=${A}`, { Origin: "http://evil.example" })).status === 403, "another Origin, even with the token: refused");
    ok((await onApp("/app/", { Cookie: `uapp_a_${P}=${A}` })).status === 200, "the app cookie works for non-browser clients");
    const root = await onApp(`/?a=${A}`);
    ok(root.status === 200 && !root.body.includes("shell-topbar"), "/ on the app origin is the app, not the shell");
    ok((await onApp(`/?t=${T}`, { Cookie: `uapp_t_${P}=${T}` })).status === 403, "the owner token means nothing on the app origin");
    for (const p of ["/shell/main.js", "/shell.css", "/icons.js", "/download.uapp", "/template.uapp", "/site.uapp"]) {
      ok((await onApp(`${p}?a=${A}`)).status === 404, `${p} is not on the app origin`);
    }
    ok((await onApp(`/upload?name=data/x.txt&a=${A}`, { "Content-Type": "text/plain" }, "POST", "hello")).status === 404, "upload is not on the app origin");
    ok((await onApp(`/scratch/?a=${A}`)).status === 200, "the scratch page lives on the app origin");
  }

  console.log("\n[3] shell-origin routes");
  {
    ok((await onShell(`/?a=${A}`)).status === 403, "the app token doesn't open the shell");
    ok((await onShell(`/app/?a=${A}`)).status === 403, "the app token doesn't read files on the shell origin");
    ok((await onShell(`/scratch/?t=${T}`)).status === 404, "no scratch page on the shell origin");
    ok((await onShell(`/index.html?t=${T}`)).status === 404, "no root-level app files on the shell origin");
    const f = await onShell(`/app/index.html?t=${T}`);
    ok(f.status === 200 && /; sandbox$/.test(f.headers["content-security-policy"] || ""), "the shell reads app HTML sandboxed", f.headers["content-security-policy"]);
  }

  console.log("\n[4] [::1] serves the app origin");
  {
    const r = await http("::1", `/app/?a=${A}`, { Host: `localhost:${P}` });
    ok(r.status === 200, "GET over [::1] with Host localhost", String(r.status));
    const addrs = (await lookup("localhost", { all: true })).map((a) => a.address);
    for (const a of addrs) {
      const s = (await http(a, `/app/?a=${A}`, { Host: `localhost:${P}` }).catch(() => ({ status: 0 }))).status;
      ok(s === 200, `localhost resolves to ${a}, and that answers`, String(s));
    }
  }

  console.log("\n[5] sockets and roles");
  const owner = client(`ws://127.0.0.1:${P}/ws?t=${T}`);
  const app = client(`ws://localhost:${P}/ws?a=${A}`, { Origin: APP });
  ok(await owner.opened, "owner socket on the shell origin");
  ok(await app.opened, "app socket on the app origin");
  ok(!(await client(`ws://localhost:${P}/ws`, { Origin: APP }).opened), "app socket without the token: refused");
  ok(!(await client(`ws://localhost:${P}/ws?a=${A}`, { Origin: "http://evil.example" }).opened), "app socket from another Origin: refused");
  ok(!(await client(`ws://localhost:${P}/ws?a=${A}`, { Origin: SHELL }).opened), "app socket with the shell's Origin: refused");
  ok(!(await client(`ws://127.0.0.1:${P}/ws?a=${A}`).opened), "the app token doesn't open an owner socket");
  ok(!(await client(`ws://localhost:${P}/ws?t=${T}`, { Origin: APP }).opened), "the owner token doesn't open a socket on the app origin");

  console.log("\n[6] what an app connection may call");
  for (const [m, p] of [
    ["ai.approve", { id: "x", allow: true }], ["ai.answer", { id: "x", answers: [] }], ["ai.pending", {}],
    ["config.set", { key: "ai", value: { provider: "local", base_url: "http://attacker.example/v1" } }],
    ["config.get", { key: "ai" }], ["config.set", { key: "toolbar", value: {} }], ["config.set", { key: "tools_disabled", value: [] }],
    ["mcp.add", { name: "x", url: "http://127.0.0.1:9/" }], ["mcp.remove", { name: "x" }],
    ["chat.send", { text: "hi" }], ["chat.list", {}], ["sessions.list", {}], ["ai.run", {}],
    ["prefs.get", {}], ["prefs.set", { key: "shell", value: "browser" }], ["crypt.set", { new_password: "x" }],
    ["template.apply", { token: "x" }], ["app.save", {}], ["app.rename", { name: "pwned" }], ["eval.run", { code: "1" }],
    ["console.read", {}], ["browser.open", { url: "https://example.com" }], ["win.close", {}], ["drop.ingest", { id: "x" }],
  ]) ok(await refused(app, m, p), `app refused ${m}${p.key ? " " + p.key : ""}`);
  ok((await owner.rpc("ai.pending")) !== undefined, "the owner still gets ai.pending");

  const inf = await app.rpc("app.info");
  ok(inf.name !== undefined && !("path" in inf), "app.info without the file's path");
  ok((await owner.rpc("app.info")).path, "the owner's app.info has the path");
  await app.rpc("config.set", { key: "app.theme", value: { dark: true } });
  ok((await app.rpc("config.get", { key: "app.theme" }))?.dark === true, "an app keeps its own app.* config");
  ok((await owner.rpc("config.get", { key: "app.theme" }))?.dark === true, "...which the owner can read too");
  const tb = await app.rpc("toolbar.setDefault", { hidden: false, shortcut: "F9" });
  ok(tb.shortcut === "F9", "toolbar.setDefault still works for apps");
  await app.rpc("sql.exec", { sql: "CREATE TABLE notes(t TEXT)" });
  await app.rpc("sql.exec", { sql: "INSERT INTO notes VALUES(?)", params: ["hi"] });
  ok((await app.rpc("sql.query", { sql: "SELECT t FROM notes" })).rows[0][0] === "hi", "sql works");
  ok((await app.rpc("files.list")).rows.length > 0, "files work");

  console.log("\n[6b] data files need the token; the app's code doesn't");
  {
    const PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    await owner.rpc("files.write", { name: "data/sales.csv", text: "a,b\n1,2\n" });
    await owner.rpc("files.write", { name: "data/pic.png", b64: PNG });
    await owner.rpc("files.write", { name: "app/page.html", text: "<h1>page</h1>" });
    const st = async (path, h = {}) => (await onApp(path, h)).status;
    // What any local program can send: the app's Host and a forged Sec-Fetch-Site.
    ok(await st("/data/sales.csv", { "Sec-Fetch-Site": "none" }) === 403, "forged Sec-Fetch-Site: none gets no data file");
    ok(await st("/data/sales.csv", { "Sec-Fetch-Site": "same-origin" }) === 403, "forged Sec-Fetch-Site: same-origin gets no data file");
    ok(await st("/data/sales.csv", { Referer: `${APP}/app/` }) === 403, "a Referer without the token gets no data file");
    ok(await st("/sales.csv", { "Sec-Fetch-Site": "same-origin" }) === 403, "nor through a root-level name that falls back to data/");
    ok(await st("/app/data/sales.csv", { "Sec-Fetch-Site": "same-origin" }) === 403, "nor through /app/data/...");
    ok(await st("/data/pic.png", { "Sec-Fetch-Site": "same-origin", Referer: `${APP}/app/?a=${"0".repeat(64)}` }) === 403, "a Referer with another token gets nothing");
    ok(await st(`/data/sales.csv?a=${A}`) === 200, "the token on the URL");
    ok(await st("/data/pic.png", { "Sec-Fetch-Site": "same-origin", Referer: `${APP}/app/?a=${A}` }) === 200, "the token in the Referer (an app page's own request)");
    ok(await st("/sales.csv", { "Sec-Fetch-Site": "same-origin", Referer: `${APP}/some/route?x=1&a=foo&a=${A}#h` }) === 200, "...after the app's own ?a= and a hash");
    ok(await st("/app/page.html", { "Sec-Fetch-Site": "same-origin" }) === 200, "the app's code still loads without it (workers, stylesheet url()s, no-referrer)");
    ok(await st("/uapp.js", { "Sec-Fetch-Site": "same-origin" }) === 200, "and so does uapp.js");
    const nav = await onApp("/app/page.html?x=1", { "Sec-Fetch-Site": "same-origin", "Sec-Fetch-Mode": "navigate", Referer: `${APP}/app/?a=${A}` });
    ok(nav.status === 302 && nav.headers.location === `/app/page.html?x=1&a=${A}`, "a page reached without ?a= is sent back with it", `${nav.status} ${nav.headers.location}`);
    const nav2 = await onApp(`/app/page.html?a=${A}`, { "Sec-Fetch-Site": "same-origin", "Sec-Fetch-Mode": "navigate", Referer: `${APP}/app/?a=${A}` });
    ok(nav2.status === 200, "one that has it isn't");
  }

  console.log("\n[7] events");
  owner.events.length = 0; app.events.length = 0;
  await app.rpc("notify.show", { title: "Done", body: "All synced" });
  await app.rpc("toolbar.set", { visible: true });
  await sleep(300);
  ok(owner.events.some((e) => e.type === "notification" && e.title === "Done" && e.body === "All synced"), "notify.show reaches the shell");
  ok(!app.events.some((e) => e.type === "notification"), "...and not the app");
  ok(app.events.some((e) => e.type === "toolbar"), "the app gets toolbar events");

  console.log("\n[8] an app's tool prompt is answered by the owner only");
  {
    owner.events.length = 0; app.events.length = 0;
    const call = app.rpc("tools.call", { name: "sql_exec", input: { sql: "INSERT INTO notes VALUES('from tool')" } });
    const pend = await waitFor(() => owner.events.filter((e) => e.type === "approval").map((e) => e.pending).find((l) => l.some((p) => p.name === "sql_exec")));
    const id = pend && pend.find((p) => p.name === "sql_exec").id;
    ok(!!id, "the prompt shows on the owner's socket");
    ok(!app.events.some((e) => e.type === "approval" || e.type === "question"), "the app never sees the prompt");
    ok(await refused(app, "ai.approve", { id, allow: true, always: true }), "the app can't approve its own prompt");
    await owner.rpc("ai.approve", { id, allow: true });
    const r = await call;
    ok(r && r.changes === 1, "approved by the owner, the call runs", JSON.stringify(r));

    const agent = app.rpc("tools.call", { name: "agent_run", input: { description: "x", prompt: "y" } }).then(() => "ran", (e) => e.message);
    const ap = await waitFor(() => owner.events.filter((e) => e.type === "approval").map((e) => e.pending).find((l) => l.some((p) => p.name === "agent_run")));
    ok(!!ap, "agent_run from an app asks first");
    if (ap) await owner.rpc("ai.approve", { id: ap.find((p) => p.name === "agent_run").id, allow: false });
    ok(/^denied/.test(await agent), "...and a no means no");
  }

  console.log("\n[9] transport messages still work for the app");
  {
    app.ws.send(JSON.stringify({ method: "log.write", params: { level: "reset" } }));
    app.ws.send(JSON.stringify({ method: "log.write", params: { level: "error", text: "boom" } }));
    const r = await waitFor(async () => (await owner.rpc("console.read", {})).entries.find((e) => e.text === "boom"));
    ok(!!r, "console forwarding from the app reaches the owner's console.read");
  }
} catch (e) {
  failed++;
  console.log("  ✗ FAIL: threw", e.stack || e);
}
proc.kill("SIGKILL");
console.log(`\n${passed} passed, ${failed} failed`);
if (failed) console.log(stderr.slice(-3000));
process.exit(failed ? 1 : 0);
