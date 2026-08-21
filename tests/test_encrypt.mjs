// Optional at-rest encryption: the .uapp is a SQLCipher database.
// Covers: encrypted create, wrong-password rejection, data round-trip across
// kill -9, no plaintext leakage into the file, encrypt-in-place, passwd,
// and decrypt.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync, existsSync, readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-encrypt";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

const PW = "correct horse battery staple";
const MARKER = "s3cret-plaintext-marker-42"; // must never appear on disk

function start(device, file, { password = null, extraEnv = {} } = {}) {
  const args = ["open", file, "--headless"];
  if (password) args.push("--password", password);
  const proc = spawn(BIN, args, {
    env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: "600", ...extraEnv },
    stdio: ["ignore", "pipe", "pipe"],
  });
  return new Promise((res, rej) => {
    let b = ""; const t = setTimeout(() => rej(new Error("startup timeout")), 15000);
    proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res({ proc, info: JSON.parse(b.slice(0, i)) }); } });
    proc.on("exit", (code) => { clearTimeout(t); rej(new Error(`exited early code=${code}`)); });
  });
}
function run(args, extraEnv = {}) {
  const proc = spawn(BIN, args, { env: { ...process.env, ...extraEnv }, stdio: ["ignore", "pipe", "pipe"] });
  return new Promise((res) => {
    let out = "", err = "";
    proc.stdout.on("data", (d) => (out += d));
    proc.stderr.on("data", (d) => (err += d));
    proc.on("exit", (code) => res({ code, out, err }));
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
const isSqlite = (f) => { try { return readFileSync(f).subarray(0, 15).toString() === "SQLite format 3"; } catch { return false; } };

const FILE = `${DIR}/enc.uapp`;

console.log("\n[1] new --encrypt creates a non-SQLite, non-leaking file");
{
  const r = await run(["new", FILE, "--encrypt", "--password", PW]);
  ok(r.code === 0 && r.out.includes("encrypted"), `uapp new --encrypt succeeded (${r.out.trim()})`);
  ok(existsSync(FILE), "file exists");
  ok(!isSqlite(FILE), "file does not start with the SQLite magic");
  ok(!readFileSync(FILE).includes("SQLite format 3"), "no SQLite header string anywhere in the file");
  // opening it WITHOUT a password must fail clearly
  const bad = await run(["open", FILE, "--headless", "--port", "0"], { UAPP_LINGER_SECS: "600" });
  ok(bad.code !== 0 && /password|encrypt/i.test(bad.err), `no-password open fails with a clear error (${bad.err.trim().slice(0, 80)})`);
  // wrong password must fail clearly
  const wrong = await run(["open", FILE, "--headless", "--port", "0", "--password", "nope"], { UAPP_LINGER_SECS: "600", UAPP_DEVICE: "encw" });
  ok(wrong.code !== 0 && /wrong master password/i.test(wrong.err), `wrong password fails clearly (${wrong.err.trim().slice(0, 80)})`);
}

console.log("\n[2] encrypted app round-trips data across kill -9");
{
  const A = await start("enc1", FILE, { password: PW });
  const { rpc } = await connect(A.info);
  const info0 = await rpc("app.info");
  ok(info0.encrypted === true, "app.info reports encrypted: true");
  await rpc("sql.batch", { sql: `CREATE TABLE secrets(id INTEGER PRIMARY KEY, body TEXT)` });
  await rpc("sql.exec", { sql: "INSERT INTO secrets(body) VALUES(?)", params: [MARKER] });
  await rpc("files.write", { name: "index.html", text: `<html>${MARKER}</html>` });
  const q = await rpc("sql.query", { sql: "SELECT body FROM secrets" });
  ok(q.rows[0][0] === MARKER, "write visible immediately");
  A.proc.kill("SIGKILL"); await sleep(400);
  // Reopen with the password: every committed write must decrypt cleanly.
  const B = await start("enc2", FILE, { password: PW });
  const { rpc: rb } = await connect(B.info);
  const q2 = await rb("sql.query", { sql: "SELECT body FROM secrets" });
  ok(q2.rows.length === 1 && q2.rows[0][0] === MARKER, "row survived kill -9 (committed transaction)");
  const f = await rb("files.read", { name: "index.html" });
  ok(f.text.includes(MARKER), "file survived kill -9");
  // Nothing readable at rest.
  ok(!readFileSync(FILE).includes(MARKER), "the file does not contain the marker in plaintext");
  B.proc.kill("SIGKILL"); await sleep(300);
}

console.log("\n[3] plaintext files still open without a password (regression)");
{
  const F2 = `${DIR}/plain.uapp`;
  const r = await run(["new", F2]);
  ok(r.code === 0, `uapp new (plaintext) succeeded (${r.out.trim()})`);
  ok(isSqlite(F2), "plaintext file has the SQLite magic");
  const A = await start("pl1", F2);
  const { rpc } = await connect(A.info);
  const info = await rpc("app.info");
  ok(info.encrypted === false, "app.info reports encrypted: false");
  await rpc("sql.batch", { sql: "CREATE TABLE t(x)" });
  await rpc("sql.exec", { sql: "INSERT INTO t VALUES(1)" });
  A.proc.kill("SIGKILL"); await sleep(300);
}

console.log("\n[4] encrypt-in-place, passwd, decrypt");
{
  const F3 = `${DIR}/conv.uapp`;
  await run(["new", F3]);
  const A0 = await start("cv0", F3);
  const { rpc: r0 } = await connect(A0.info);
  await r0("sql.batch", { sql: "CREATE TABLE keep(x)" });
  await r0("sql.exec", { sql: `INSERT INTO keep VALUES('${MARKER}')` });
  A0.proc.kill("SIGKILL"); await sleep(300);

  // encrypt in place
  const enc = await run(["encrypt", F3, "--password", PW]);
  ok(enc.code === 0, `uapp encrypt succeeded (${enc.out.trim()})`);
  ok(!isSqlite(F3), "file no longer looks like SQLite");
  const again = await run(["encrypt", F3, "--password", PW]);
  ok(again.code !== 0 && /already encrypted/.test(again.err), "re-encrypt refuses with 'already encrypted'");

  // data survives encryption; open with password
  const A = await start("cv1", F3, { password: PW });
  const { rpc } = await connect(A.info);
  const q = await rpc("sql.query", { sql: "SELECT x FROM keep" });
  ok(q.rows[0][0] === MARKER, "data survived encrypt-in-place");
  A.proc.kill("SIGKILL"); await sleep(300);

  // passwd: old stops working, new works, data intact
  const pw = await run(["passwd", F3, "--password", PW], { UAPP_PASSWORD: "new-pass-9" });
  ok(pw.code === 0, `uapp passwd succeeded (${pw.out.trim()})`);
  const oldpw = await run(["open", F3, "--headless", "--port", "0", "--password", PW], { UAPP_DEVICE: "cvx", UAPP_LINGER_SECS: "600" });
  ok(oldpw.code !== 0 && /wrong master password/i.test(oldpw.err), "old password rejected after passwd");
  const B = await start("cv2", F3, { password: "new-pass-9" });
  const { rpc: rb } = await connect(B.info);
  const q2 = await rb("sql.query", { sql: "SELECT x FROM keep" });
  ok(q2.rows[0][0] === MARKER, "data intact after passwd");
  B.proc.kill("SIGKILL"); await sleep(300);

  // decrypt: back to plain SQLite, opens without password, data intact
  const dec = await run(["decrypt", F3, "--password", "new-pass-9"]);
  ok(dec.code === 0, `uapp decrypt succeeded (${dec.out.trim()})`);
  ok(isSqlite(F3), "file is plain SQLite again");
  const C = await start("cv3", F3);
  const { rpc: rc } = await connect(C.info);
  const q3 = await rc("sql.query", { sql: "SELECT x FROM keep" });
  ok(q3.rows[0][0] === MARKER, "data intact after decrypt");
  C.proc.kill("SIGKILL"); await sleep(300);
}

console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
