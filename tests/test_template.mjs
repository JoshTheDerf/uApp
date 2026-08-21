// Template export: /template.uapp downloads a copy with app-role files and
// empty tables — no user data (data/ files, rows, chat, history), no API key,
// and a fresh app_id. A file's role IS its top-level directory: app/ ships in
// the template, data/ does not; files.list reports it as a column.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync, writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-template";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

function start(device, file) {
  const proc = spawn(BIN, ["open", file, "--headless"], {
    env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: "600" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  return new Promise((res, rej) => {
    let b = ""; const t = setTimeout(() => rej(new Error("startup timeout")), 15000);
    proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res({ proc, info: JSON.parse(b.slice(0, i)) }); } });
  });
}
function connect(info) {
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${new URL(info.url).searchParams.get("t")}`);
  const pending = new Map(); let id = 1;
  ws.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.method === "event") return;
    const p = pending.get(m.id); if (p) { pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); } };
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 30000);
  });
  return new Promise((r) => (ws.onopen = () => r({ ws, rpc })));
}

const A = await start("devtp1", `${DIR}/crm.uapp`);
const { rpc } = await connect(A.info);
const TOKEN = new URL(A.info.url).searchParams.get("t");

// Build a "real" app: code files, user data files, table with rows, config.
await rpc("app.rename", { name: "Customer CRM" });
// Unprefixed paths fold into app/; user content is written under data/.
await rpc("files.write", { name: "index.html", text: "<html><script src='/uapp.js'></script>CRM</html>" });
await rpc("files.write", { name: "js/app.js", text: "// app logic ".repeat(50) });
await rpc("files.write", { name: "data/customers.csv", text: "name,email\nBob,b@x.com\n" });
await rpc("files.write", { name: "app/seed/countries.csv", text: "US\nCA\nMX\n" });
await rpc("files.write", { name: "data/notes-export.csv", text: "private,stuff\n" });
await rpc("sql.batch", { sql: "CREATE TABLE customers(name TEXT, email TEXT); INSERT INTO customers VALUES('Bob','b@x.com'); CREATE INDEX idx_cust ON customers(name); CREATE VIEW v_names AS SELECT name FROM customers" });
await rpc("config.set", { key: "ai", value: { provider: "zai", api_key: "SECRET-KEY-123", model: "glm-5.3" } });
await rpc("config.set", { key: "mcp", value: [{ name: "crm", url: "https://example.com/mcp", enabled: true }] });
console.log("\n[1] files.list reports the role each directory implies");
{
  const f = await rpc("files.list");
  const names = f.rows.map((r) => r[0]);
  const role = (n) => f.rows.find((r) => r[0] === n)?.[3];
  ok(f.columns.includes("role"), "role column present");
  ok(names.every((n) => n.startsWith("app/") || n.startsWith("data/")),
    "every name lives under app/ or data/", JSON.stringify(names));
  ok(role("app/js/app.js") === "app", "a file under app/ is app code");
  ok(role("data/customers.csv") === "data", "a file under data/ is user data");
  ok(role("app/seed/countries.csv") === "app", "a seed dataset kept in app/ stays app");
  ok(role("data/notes-export.csv") === "data", "an export written to data/ is data");
}

console.log("\n[2] template download: auth + headers");
{
  const noauth = await fetch(`http://127.0.0.1:${A.info.port}/template.uapp`);
  ok(noauth.status === 403, "requires auth");
  const r = await fetch(`http://127.0.0.1:${A.info.port}/template.uapp?t=${TOKEN}`);
  ok(r.status === 200, "downloads with token");
  const cd = r.headers.get("content-disposition") || "";
  ok(cd.includes('attachment') && cd.includes("Customer CRM template.uapp"), `attachment filename (${cd})`);
  writeFileSync(`${DIR}/tpl.uapp`, Buffer.from(await r.arrayBuffer()));
}

console.log("\n[3] template contents: app files in, user data out");
{
  const T = await start("devtp2", `${DIR}/tpl.uapp`);
  const { rpc: rt } = await connect(T.info);
  const f = await rt("files.list");
  const names = f.rows.map((r) => r[0]);
  ok(names.includes("app/index.html") && names.includes("app/js/app.js"),
    "app code files present", JSON.stringify(names));
  ok(names.includes("app/seed/countries.csv"), "seed dataset under app/ included");
  ok(!names.includes("data/customers.csv"), "data/ upload excluded");
  ok(!names.includes("data/notes-export.csv"), "data/ export excluded");
  ok(!names.some((n) => n.startsWith("data/")), "the template carries no data/ files at all");
  const idx = await rt("files.read", { name: "index.html" });
  ok(idx.text.includes("CRM"), "file content survives (compressed copy verbatim)");

  const schema = await rt("sql.query", { sql: "SELECT type, name FROM sqlite_master WHERE name IN ('customers','idx_cust','v_names')" });
  ok(schema.rows.length === 3, `schema (table+index+view) carried over (${schema.rows.length})`);
  const cnt = await rt("sql.query", { sql: "SELECT count(*) FROM customers" });
  ok(cnt.rows[0][0] === 0, "table is empty — no user rows");
  const chat = await rt("chat.list");
  ok(chat.rows.length === 0, "no chat history");
  const hist = await rt("sql.query", { sql: "SELECT count(*) FROM uapp_file_history" });
  ok(hist.rows[0][0] === 0, "no file history");

  const ai = await rt("config.get", { key: "ai" });
  ok(ai && ai.provider === "zai" && ai.model === "glm-5.3" && !("api_key" in ai), `ai config kept minus api_key (${JSON.stringify(ai)})`);
  const mcp = await rt("config.get", { key: "mcp" });
  ok(Array.isArray(mcp) && mcp[0].name === "crm", "mcp servers carried over");

  const infoA = await rpc("app.info");
  const infoT = await rt("app.info");
  ok(infoT.appId && infoT.appId !== infoA.appId, "fresh app_id (new lineage)");
  ok(infoT.name === "Customer CRM", "name preserved");

  // the template is a working app: writes + replication machinery run
  await rt("sql.exec", { sql: "INSERT INTO customers VALUES(?,?)", params: ["Ann", "a@y.com"] });
  const c2 = await rt("sql.query", { sql: "SELECT count(*) FROM customers" });
  ok(c2.rows[0][0] === 1, "template is immediately usable as a new app");
  T.proc.kill("SIGKILL");
}

A.proc.kill("SIGKILL");
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
