// Scratch app: `uapp` with no file opens a blank, unsaved app in a temp file.
// app.info reports unsaved:true; GET /download.uapp returns a complete,
// openable .uapp containing whatever you built.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync, writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-scratch";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

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

// Launch with NO file argument.
const proc = spawn(BIN, ["--headless"], {
  env: { ...process.env, UAPP_DEVICE: "devscr", UAPP_LINGER_SECS: "600" },
  stdio: ["ignore", "pipe", "pipe"],
});
const info = await new Promise((res, rej) => {
  let b = ""; const t = setTimeout(() => rej(new Error("startup timeout")), 15000);
  proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(JSON.parse(b.slice(0, i))); } });
});
const TOKEN = new URL(info.url).searchParams.get("t");
const { rpc } = await connect(info);

console.log("\n[1] blank scratch app, flagged unsaved");
{
  const i = await rpc("app.info");
  ok(i.unsaved === true, "app.info reports unsaved:true");
  const files = await rpc("files.list");
  ok(files.rows.some((r) => r[0] === "app/index.html"), "has the default blank app/index.html");
  // it's a working app — build something in it
  await rpc("sql.batch", { sql: "CREATE TABLE todos(id INTEGER PRIMARY KEY, task TEXT)" });
  await rpc("sql.exec", { sql: "INSERT INTO todos(task) VALUES(?)", params: ["buy milk"] });
}

console.log("\n[2] download.uapp returns a complete, openable app");
{
  const noauth = await fetch(`http://127.0.0.1:${info.port}/download.uapp`);
  ok(noauth.status === 403, "download requires auth");
  const r = await fetch(`http://127.0.0.1:${info.port}/download.uapp?t=${TOKEN}`);
  ok(r.status === 200, "download ok with token");
  ok((r.headers.get("content-disposition") || "").includes(".uapp"), "served as a .uapp attachment");
  const bytes = Buffer.from(await r.arrayBuffer());
  ok(bytes.slice(0, 15).toString() === "SQLite format 3", "download is a valid SQLite/sqlar file");
  writeFileSync(`${DIR}/saved.uapp`, bytes);
}

console.log("\n[3] the downloaded app opens and has the data + no unsaved flag");
{
  const p2 = spawn(BIN, ["open", `${DIR}/saved.uapp`, "--headless"], {
    env: { ...process.env, UAPP_DEVICE: "devscr2", UAPP_LINGER_SECS: "600" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const info2 = await new Promise((res, rej) => {
    let b = ""; const t = setTimeout(() => rej(new Error("startup timeout")), 15000);
    p2.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(JSON.parse(b.slice(0, i))); } });
  });
  const { rpc: r2 } = await connect(info2);
  const i2 = await r2("app.info");
  ok(!i2.unsaved, "a normally-opened file is NOT flagged unsaved");
  const q = await r2("sql.query", { sql: "SELECT task FROM todos" });
  ok(q.rows.length === 1 && q.rows[0][0] === "buy milk", "downloaded app kept the data");
  p2.kill("SIGKILL");
}

proc.kill("SIGINT"); await sleep(300); proc.kill("SIGKILL");
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
