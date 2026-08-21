// End-to-end test for uapp: one process, direct SQLite on the .uapp file.
// Covers bootstrap, writes + change events, files/history, CSV import, chat,
// the no-sibling-files rule, durability (graceful + kill -9), double-open
// reuse via the servers registry, and rolling snapshots.
import { spawn, execSync } from "node:child_process";
import { writeFileSync, mkdirSync, rmSync, existsSync, readdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-share";
const FILE = `${DIR}/ops.uapp`;

let passed = 0, failed = 0;
function ok(cond, name, extra = "") {
  if (cond) { passed++; console.log(`  ✓ ${name}`); }
  else { failed++; console.log(`  ✗ FAIL: ${name} ${extra}`); }
}

class Instance {
  constructor(device, file = FILE) {
    this.device = device;
    this.file = file;
  }
  async start() {
    this.proc = spawn(BIN, ["open", this.file, "--headless"], {
      env: { ...process.env, UAPP_DEVICE: this.device },
      stdio: ["ignore", "pipe", "pipe"],
    });
    this.stderr = "";
    this.proc.stderr.on("data", (d) => { this.stderr += d; });
    const line = await new Promise((resolve, reject) => {
      let buf = "";
      const t = setTimeout(() => reject(new Error(`${this.device}: no startup line. stderr: ${this.stderr}`)), 15000);
      this.proc.stdout.on("data", (d) => {
        buf += d;
        const nl = buf.indexOf("\n");
        if (nl >= 0) { clearTimeout(t); resolve(buf.slice(0, nl)); }
      });
      this.proc.on("exit", (c) => { clearTimeout(t); reject(new Error(`${this.device} exited ${c}: ${this.stderr}`)); });
    });
    const info = JSON.parse(line);
    this.port = info.port;
    this.token = new URL(info.url).searchParams.get("t");
    this.url = info.url;
    this.reused = !!info.reused;
    if (!this.reused) await this.connect();
    return info;
  }
  async connect() {
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    await new Promise((res, rej) => {
      this.ws.onopen = res;
      this.ws.onerror = (e) => rej(new Error("ws error"));
    });
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") { this.events.push(m.params); return; }
      const p = this.pending.get(m.id);
      if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
    };
  }
  rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error(`rpc timeout: ${method}`)); } }, 20000);
    });
  }
  async http(path) {
    const r = await fetch(`http://127.0.0.1:${this.port}${path}`, { headers: { Cookie: `uapp_t_${this.port}=${this.token}` } });
    return { status: r.status, text: await r.text() };
  }
  kill9() { this.proc.kill("SIGKILL"); }
  async stop() { try { this.ws?.close(); } catch {} this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}

async function waitFor(fn, ms = 8000, step = 300) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try { const v = await fn(); if (v) return v; } catch {}
    await sleep(step);
  }
  return null;
}

const sq = (sql) => execSync(`sqlite3 "${FILE}" "${sql}"`).toString().trim();

// ---------------- tests ----------------
rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });

console.log("\n[1] blank-file bootstrap (file browser 'New Document' case)");
writeFileSync(`${DIR}/blank.uapp`, ""); // 0-byte file, like a template copy
const blank = new Instance("devblank", `${DIR}/blank.uapp`);
await blank.start();
{
  const h = await blank.http("/health");
  ok(h.text === "ok", "health endpoint");
  const shell = await blank.http("/");
  ok(shell.status === 200 && shell.text.includes("uapp"), "shell page served");
  const appPage = await blank.http("/app/");
  ok(appPage.status === 200 && appPage.text.includes("This app is empty"), "default app page in iframe");
  const uappjs = await blank.http("/uapp.js");
  ok(uappjs.status === 200 && uappjs.text.includes("window.uapp"), "uapp.js client served");
  const noauth = await fetch(`http://127.0.0.1:${blank.port}/app/`);
  ok(noauth.status === 403, "requests without token are rejected");
  const info = await blank.rpc("app.info");
  ok(info.device === "devblank", "app.info over ws rpc");
}
await blank.stop();

console.log("\n[2] writes land in the file; change events push");
const A = new Instance("devA");
await A.start();
await A.rpc("sql.batch", { sql: "CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY, title TEXT, created INTEGER);" });
await A.rpc("sql.exec", { sql: "INSERT INTO jobs(title, created) VALUES(?,?)", params: ["fix gate", 1755000000000] });
{
  const q = await A.rpc("sql.query", { sql: "SELECT title FROM jobs" });
  ok(q.rows.length === 1 && q.rows[0][0] === "fix gate", "A sees own write");
  // The write is already committed to the FILE (no checkpoint delay): a
  // plain sqlite3 read of the live file sees it.
  ok(sq("SELECT title FROM jobs") === "fix gate", "committed write visible in the file immediately");
  const evt = A.events.find((e) => e.type === "changes" && e.data);
  ok(!!evt, "data-change event pushed");
}

console.log("\n[3] no sibling files, ever");
{
  const entries = readdirSync(DIR).filter((f) => f.startsWith("ops.uapp") && f !== "ops.uapp");
  ok(entries.length === 0, "no .sync/-journal/-wal next to the live file", JSON.stringify(entries));
}

console.log("\n[4] file write + reload event + history");
await A.rpc("files.write", { name: "index.html", text: "<html><body><h1>Job Tracker v1</h1></body></html>" });
{
  const page = await A.http("/app/");
  ok(page.text.includes("Job Tracker v1"), "serves the new index.html");
  const hist = await A.rpc("files.history", { name: "index.html" });
  ok(hist.rows.length >= 1, "file history recorded");
  const filesEvt = A.events.find((e) => e.type === "changes" && e.files);
  ok(!!filesEvt, "files-change event (iframe reload)");
  // revert
  const firstId = hist.rows[hist.rows.length - 1][0];
  await A.rpc("files.revert", { name: "index.html", id: firstId });
  const back = await A.http("/app/");
  ok(back.text.includes("This app is empty"), "revert restores previous version");
}

console.log("\n[5] CSV import + complex SQL + chat");
{
  const csv = "region,product,amount\nNorth,Widget,120.5\nNorth,Gadget,80\nSouth,Widget,200\nSouth,Gadget,150\n" +
    Array.from({ length: 2000 }, (_, i) => `East,Item${i % 7},${(i * 13) % 400}`).join("\n");
  const b64 = Buffer.from(csv).toString("base64");
  await A.rpc("chat.send", { text: "here's the sales data", attachments: [{ name: "sales.csv", b64 }] }).catch(() => {});
  // chat.send tries to run AI (unconfigured) — attachment should still be stored
  const files = await A.rpc("files.list");
  ok(files.rows.some((r) => r[0] === "data/sales.csv"), "chat upload stored under data/", JSON.stringify(files.rows.map((r) => r[0])));
  const imp = await A.rpc("import.csv", { file: "data/sales.csv", table: "sales" });
  ok(imp.rows === 2004, `import.csv imported ${imp.rows} rows`);
  const q = await A.rpc("sql.query", {
    sql: "SELECT region, sum(amount) s FROM sales GROUP BY region HAVING s > 100 ORDER BY s DESC",
  });
  ok(q.rows.length >= 2, "complex aggregate over imported data");
  const sysmsg = await waitFor(async () => {
    const r = await A.rpc("chat.list");
    const sys = r.rows.filter((x) => x[4] === "system");
    return sys.length ? sys : null;
  });
  ok(!!sysmsg && sysmsg[0][5].includes("no AI provider configured"), "unconfigured AI surfaces friendly error in chat");
  // The chat panel's setup banner drives off app.info.aiConfigured.
  ok((await A.rpc("app.info")).aiConfigured === false, "app.info reports no AI provider");
  await A.rpc("config.set", { key: "ai", value: { provider: "anthropic", api_key: "k" } });
  ok((await A.rpc("app.info")).aiConfigured === true, "aiConfigured flips once a provider is saved");
  await A.rpc("config.set", { key: "ai", value: null });
  ok((await A.rpc("app.info")).aiConfigured === false, "and back off when the provider is cleared");
}

console.log("\n[6] double-open reuses the running server (registry)");
{
  const B = new Instance("devB");
  const info = await B.start();
  ok(info.reused === true, "second open reports reused");
  ok(info.port === A.port, "and points at the running server's port");
}

console.log("\n[7] compaction (VACUUM) keeps everything");
{
  const before = await A.rpc("sql.query", { sql: "SELECT count(*) FROM sales" });
  const r = await A.rpc("app.compact");
  ok(r.ok === true && r.fileSize > 0, "app.compact runs and reports the file size");
  const after = await A.rpc("sql.query", { sql: "SELECT count(*) FROM sales" });
  ok(after.rows[0][0] === before.rows[0][0], "data intact after VACUUM");
}

console.log("\n[8] crash safety: kill -9 after ack, data survives");
{
  await A.rpc("sql.exec", { sql: "INSERT INTO jobs(title, created) VALUES(?,?)", params: ["crash survivor", 1755000002000] });
  A.kill9();
  await sleep(500);
  const A2 = new Instance("devA");
  await A2.start();
  const q = await A2.rpc("sql.query", { sql: "SELECT count(*) FROM jobs WHERE title='crash survivor'" });
  ok(q.rows[0][0] === 1, "acked write survives kill -9 + restart (committed transaction)");
  globalThis.A2 = A2;
}

console.log("\n[9] graceful close: file self-contained, registry addr removed");
{
  const A2 = globalThis.A2;
  await A2.rpc("sql.exec", { sql: "INSERT INTO jobs(title, created) VALUES(?,?)", params: ["last write", 1755000003000] });
  await A2.stop();
  ok(sq("SELECT count(*) FROM jobs") === "3", "all rows readable with plain sqlite3 after close");
  // A fresh open must NOT try to reuse the dead server.
  const A3 = new Instance("devA");
  const info = await A3.start();
  ok(!info.reused, "fresh open starts a new server (stale addr ignored)");
  const q = await A3.rpc("sql.query", { sql: "SELECT count(*) FROM jobs" });
  ok(q.rows[0][0] === 3, "reopen sees full state");
  await A3.stop();
}

console.log("\n[10] rolling snapshots exist locally");
{
  const appId = sq("SELECT value FROM uapp_meta WHERE key='app_id'");
  const snapdir = `${process.env.HOME}/.local/share/uapp/${appId}/snapshots`;
  const snaps = existsSync(snapdir) ? readdirSync(snapdir) : [];
  ok(snaps.length >= 1, `rolling snapshots kept (${snaps.length})`);
}

console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
