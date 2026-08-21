// Updating an app from a template: drag a template .uapp onto an app you're
// already using and its CODE is replaced (app/ files, schema) while everything
// that is YOURS stays — data/ files, table rows, chat, file history, the app's
// id and name, and your own config (API key included).
//
// Covers the RPC pair the shell drives (template.inspect → template.apply),
// the --keep-stale variant, the `uapp update` CLI (including --dry-run), and
// the refusals for a file that isn't a .uapp.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync, writeFileSync, readFileSync, copyFileSync } from "node:fs";
import { execFileSync } from "node:child_process";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-update";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

function start(device, file) {
  const proc = spawn(BIN, ["open", file, "--headless"], {
    env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: "600", UAPP_TEST_DROPS: "1" },
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
async function download(info, path, out) {
  const t = new URL(info.url).searchParams.get("t");
  const r = await fetch(`http://127.0.0.1:${info.port}${path}?t=${t}`);
  if (!r.ok) throw new Error(`${path}: ${r.status}`);
  writeFileSync(out, Buffer.from(await r.arrayBuffer()));
}
const b64 = (f) => readFileSync(f).toString("base64");

// ---------------------------------------------------------------- v1: the app
const A = await start("devup1", `${DIR}/crm.uapp`);
const { rpc } = await connect(A.info);
await rpc("app.rename", { name: "Customer CRM" });
await rpc("files.write", { name: "index.html", text: "<html>CRM v1</html>" });
await rpc("files.write", { name: "js/app.js", text: "// v1 logic" });
await rpc("files.write", { name: "js/legacy.js", text: "// dropped in v2" });
await rpc("sql.batch", { sql: "CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT, email TEXT); CREATE INDEX idx_cust ON customers(name)" });
await download(A.info, "/template.uapp", `${DIR}/tpl-v1.uapp`);

// The user's own content, added after the app was built.
await rpc("sql.exec", { sql: "INSERT INTO customers(name,email) VALUES(?,?)", params: ["Bob", "b@x.com"] });
await rpc("sql.exec", { sql: "INSERT INTO customers(name,email) VALUES(?,?)", params: ["Ann", "a@y.com"] });
await rpc("sql.batch", { sql: "CREATE TABLE my_notes(note TEXT); INSERT INTO my_notes VALUES('mine')" });
await rpc("files.write", { name: "data/customers.csv", text: "name,email\nBob,b@x.com\n" });
await rpc("config.set", { key: "ai", value: { provider: "zai", api_key: "MY-OWN-KEY", model: "glm-5.3" } });
await rpc("chat.send", { text: "hello", _norun: true }).catch(() => {});
const before = await rpc("app.info");

// ------------------------------------------------------- v2: the new template
// Built by opening v1's template as its own app and editing it, exactly how a
// developer would ship an update.
{
  const T = await start("devup2", `${DIR}/tpl-v1.uapp`);
  const { rpc: rt } = await connect(T.info);
  await rt("files.write", { name: "index.html", text: "<html>CRM v2</html>" });
  await rt("files.write", { name: "js/app.js", text: "// v2 logic" });
  await rt("files.write", { name: "js/new.js", text: "// new in v2" });
  await rt("files.delete", { name: "js/legacy.js" });
  await rt("sql.batch", { sql: "ALTER TABLE customers ADD COLUMN phone TEXT" });
  await rt("sql.batch", { sql: "ALTER TABLE customers ADD COLUMN tier TEXT NOT NULL DEFAULT 'std'" });
  await rt("sql.batch", { sql: "CREATE TABLE invoices(id INTEGER PRIMARY KEY, customer INTEGER, cents INTEGER)" });
  await rt("sql.batch", { sql: "CREATE VIEW v_top AS SELECT name FROM customers ORDER BY name" });
  await rt("config.set", { key: "template_only", value: { hello: true } });
  await download(T.info, "/template.uapp", `${DIR}/tpl-v2.uapp`);
  T.proc.kill("SIGKILL");
}
copyFileSync(`${DIR}/crm.uapp`, `${DIR}/crm-cli.uapp`); // for the CLI checks below

console.log("\n[1] inspect: what the update would change (nothing written yet)");
let token;
{
  const r = await rpc("template.inspect", { b64: b64(`${DIR}/tpl-v2.uapp`), name: "CRM v2 template.uapp" });
  token = r.token;
  const p = r.plan;
  ok(!!token, "a staging token comes back");
  ok(p.source.file === "CRM v2 template.uapp", "the source file is named back");
  ok(p.source.appId && p.source.appId !== before.appId, "the template is a different lineage");
  ok(p.target.appId === before.appId, "the target is this app");
  ok(p.files.changed.includes("app/index.html") && p.files.changed.includes("app/js/app.js"),
    "edited files show as changed", JSON.stringify(p.files));
  ok(p.files.added.includes("app/js/new.js"), "a new file shows as added");
  ok(p.files.stale.includes("app/js/legacy.js"), "a file the template dropped shows as stale");
  ok(p.schema.addColumns.some((c) => c.column === "phone") &&
     p.schema.addColumns.some((c) => c.column === "tier"), "new columns are planned",
     JSON.stringify(p.schema.addColumns));
  ok(p.schema.createTables.some((t) => t.table === "invoices"), "a new table is planned");
  ok(p.schema.createObjects.some((o) => o.name === "v_top"), "a new view is planned");
  ok(p.schema.keptTables.includes("my_notes"), "a table only this app has is listed as kept");
  ok(p.config.added.includes("template_only") && !p.config.added.includes("ai"),
    "config: only keys this app lacks", JSON.stringify(p.config));
  // Still a dry run.
  const idx = await rpc("files.read", { name: "index.html" });
  ok(idx.text.includes("v1"), "inspect changed nothing on its own");
}

console.log("\n[2] apply: code updated, data untouched");
{
  const r = await rpc("template.apply", { token, remove_stale: true });
  ok(r.applied.filesWritten === 3, `3 files written (${r.applied.filesWritten})`);
  ok(r.applied.filesRemoved === 1, `1 stale file removed (${r.applied.filesRemoved})`);
  ok(r.applied.columnsAdded === 2, `2 columns added (${r.applied.columnsAdded})`);
  ok(r.applied.tablesCreated === 1, `1 table created (${r.applied.tablesCreated})`);
  ok(r.applied.objectsUpdated >= 1, `the view was created (${r.applied.objectsUpdated})`);

  const idx = await rpc("files.read", { name: "index.html" });
  ok(idx.text.includes("v2"), "app code is the new version");
  const files = (await rpc("files.list")).rows.map((r) => r[0]);
  ok(files.includes("app/js/new.js"), "new app file is there");
  ok(!files.includes("app/js/legacy.js"), "stale app file is gone");
  ok(files.includes("data/customers.csv"), "the user's data/ file is untouched");

  const rows = await rpc("sql.query", { sql: "SELECT name, email, phone, tier FROM customers ORDER BY name" });
  ok(rows.rows.length === 2, `both customer rows survived (${rows.rows.length})`);
  ok(rows.rows[0][0] === "Ann" && rows.rows[0][1] === "a@y.com", "row content survived");
  ok(rows.rows[0][2] === null, "the new nullable column is NULL on old rows");
  ok(rows.rows[0][3] === "std", "the new NOT NULL DEFAULT column got its default");
  const mine = await rpc("sql.query", { sql: "SELECT note FROM my_notes" });
  ok(mine.rows.length === 1 && mine.rows[0][0] === "mine", "a table the template never had is untouched");
  const inv = await rpc("sql.query", { sql: "SELECT count(*) FROM invoices" });
  ok(inv.rows[0][0] === 0, "the new table exists and is empty");
  const view = await rpc("sql.query", { sql: "SELECT count(*) FROM v_top" });
  ok(view.rows[0][0] === 2, "the new view works against existing rows");

  const ai = await rpc("config.get", { key: "ai" });
  ok(ai && ai.api_key === "MY-OWN-KEY", `the user's API key was NOT overwritten (${JSON.stringify(ai)})`);
  const extra = await rpc("config.get", { key: "template_only" });
  ok(extra && extra.hello === true, "a config key only the template had was filled in");

  const after = await rpc("app.info");
  ok(after.appId === before.appId, "the app keeps its identity (app_id)");
  ok(after.name === "Customer CRM", `the app keeps its name (${after.name})`);
  const chat = await rpc("chat.list");
  ok(chat.rows.length >= 1, `chat history survived (${chat.rows.length} message(s))`);

  const hist = await rpc("files.history", { name: "app/index.html" });
  ok(hist.rows.length >= 1, "the overwrite is revertible (file history recorded)");
  const prov = await rpc("sql.query", { sql: "SELECT value FROM uapp_meta WHERE key='template_source'" });
  ok(prov.rows.length === 1, "provenance recorded (template_source)");
}

console.log("\n[3] a second update is a no-op, and --keep-stale keeps extras");
{
  const r = await rpc("template.inspect", { b64: b64(`${DIR}/tpl-v2.uapp`) });
  ok(r.plan.files.changed.length === 0 && r.plan.files.added.length === 0,
    "re-applying the same template finds nothing to change", JSON.stringify(r.plan.files));
  ok(r.plan.schema.addColumns.length === 0 && r.plan.schema.createTables.length === 0,
    "and nothing to migrate");
  await rpc("template.discard", { token: r.token });
  let threw = "";
  await rpc("template.apply", { token: r.token }).catch((e) => (threw = e.message));
  ok(/no longer staged/.test(threw), `a discarded token can't be applied (${threw})`);

  // A locally-added app file + keep_stale: it stays.
  await rpc("files.write", { name: "js/local-tweak.js", text: "// mine" });
  const r2 = await rpc("template.inspect", { b64: b64(`${DIR}/tpl-v2.uapp`) });
  ok(r2.plan.files.stale.includes("app/js/local-tweak.js"), "a locally-added app file is listed as stale");
  const a2 = await rpc("template.apply", { token: r2.token, remove_stale: false });
  ok(a2.applied.filesRemoved === 0 && a2.applied.staleKept === 1, "keep-stale leaves it alone");
  const files = (await rpc("files.list")).rows.map((r) => r[0]);
  ok(files.includes("app/js/local-tweak.js"), "the local file is still there");
}

console.log("\n[4] a native window drop of a .uapp offers the update by id");
{
  // What the desktop window does: the OS drop is held server-side under an id
  // and the page asks for it by id — it never sees a path.
  const events = [];
  const ws2 = new WebSocket(`ws://127.0.0.1:${A.info.port}/ws?t=${new URL(A.info.url).searchParams.get("t")}`);
  await new Promise((r) => (ws2.onopen = r));
  ws2.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.method === "event") events.push(m.params); };
  // Hover carries the names too, so the shell can tell an app update from
  // files to store and not light up a drop zone that won't be used.
  await rpc("drop.simulate", { phase: "hover", paths: [`${DIR}/tpl-v2.uapp`], x: 10, y: 10 });
  await new Promise((r) => setTimeout(r, 250));
  const hover = events.find((e) => e.type === "drop_hover");
  ok(hover && hover.names && /\.uapp$/i.test(hover.names[0] || ""),
    `the hover event names the dragged file (${hover && JSON.stringify(hover.names)})`);
  await rpc("drop.simulate", { paths: [`${DIR}/tpl-v2.uapp`], x: 10, y: 10 });
  await new Promise((r) => setTimeout(r, 300));
  const drop = events.find((e) => e.type === "drop_files");
  ok(!!drop && /\.uapp$/i.test(drop.names[0]), `the drop arrives with the file name (${drop && drop.names})`);
  const r = await rpc("template.inspect", { id: drop.id, name: drop.names[0] });
  ok(r.token && r.plan.source.file === drop.names[0], "inspect works from a drop id");
  await rpc("template.discard", { token: r.token });
  let stale = "";
  await rpc("template.inspect", { id: drop.id }).catch((e) => (stale = e.message));
  ok(/expired/.test(stale), `a drop id is one-shot (${stale})`);
  ws2.close();
}

console.log("\n[5] refusals");
{
  let e1 = "";
  await rpc("template.inspect", { b64: Buffer.from("not a database at all, just text").toString("base64"), name: "notes.txt" })
    .catch((e) => (e1 = e.message));
  ok(/plain SQLite database|not a \.uapp|integrity/.test(e1), `a non-.uapp file is refused (${e1})`);
  let e2 = "";
  // A plain SQLite database that is not a .uapp.
  const plain = `${DIR}/plain.db`;
  execFileSync("sqlite3", [plain, "CREATE TABLE t(x); INSERT INTO t VALUES(1);"]);
  await rpc("template.inspect", { b64: b64(plain), name: "plain.db" }).catch((e) => (e2 = e.message));
  ok(/not a \.uapp/.test(e2), `a foreign SQLite db is refused (${e2})`);
}
A.proc.kill("SIGKILL");
await new Promise((r) => setTimeout(r, 400));

console.log("\n[6] the CLI: uapp update (and --dry-run)");
{
  const run = (args) => execFileSync(BIN, args, { encoding: "utf8", env: { ...process.env, UAPP_DEVICE: "devupcli" } });
  const plan = JSON.parse(run(["update", `${DIR}/crm-cli.uapp`, `${DIR}/tpl-v2.uapp`, "--dry-run"]));
  ok(plan.files.changed.includes("app/index.html"), "--dry-run prints the plan as JSON");
  const q = (sql) => execFileSync("sqlite3", [`${DIR}/crm-cli.uapp`, sql], { encoding: "utf8" }).trim();
  ok(q("SELECT count(*) FROM customers") === "2", "--dry-run wrote nothing");

  const out = run(["update", `${DIR}/crm-cli.uapp`, `${DIR}/tpl-v2.uapp`]);
  ok(/file\(s\) written/.test(out), `the CLI reports what it did (${out.trim()})`);
  ok(q("SELECT count(*) FROM customers") === "2", "CLI update kept the rows");
  ok(q("SELECT count(*) FROM pragma_table_info('customers') WHERE name='phone'") === "1",
    "CLI update migrated the schema");
  ok(q("SELECT count(*) FROM sqlar WHERE name='app/js/legacy.js'") === "0", "CLI update removed the stale file");
  ok(q("SELECT count(*) FROM sqlar WHERE name='data/customers.csv'") === "1", "CLI update kept the data file");
}

console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
