// Console forwarding: the live app iframe forwards its console.* output and
// uncaught errors to the server (method "log.write" over the app's own WS),
// which buffers them for the chat-tab AI's read_console tool. This test plays
// the browser side directly — sends log.write frames and reads them back via
// the console.read RPC — and checks the generation ("reset") + error filtering
// and the read-acks-everything behaviour the auto-alert relies on.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync } from "node:fs";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-console";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });
const proc = spawn(BIN, ["open", `${DIR}/c.uapp`, "--headless"], {
  env: { ...process.env, UAPP_DEVICE: "devconsole", UAPP_LINGER_SECS: "600" },
  stdio: ["ignore", "pipe", "pipe"],
});
let stderr = ""; proc.stderr.on("data", (d) => (stderr += d));
const line = await new Promise((res, rej) => {
  let b = ""; const t = setTimeout(() => rej(new Error("no startup line. stderr: " + stderr)), 15000);
  proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res(b.slice(0, i)); } });
});
const info = JSON.parse(line);
if (info.reused) { console.error("FAIL: a uapp-server from an earlier run still has the test file open (old code!) — pkill -x uapp-server"); process.exit(2); }
const TOKEN = new URL(info.url).searchParams.get("t");

function client(context) {
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${TOKEN}`);
  const pending = new Map(); let id = 1;
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 15000);
  });
  // Fire-and-forget log frame, exactly as uapp.js's logSend does.
  const log = (level, text) => ws.send(JSON.stringify({ method: "log.write", params: { level, text } }));
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event") return;
    const pr = pending.get(m.id);
    if (pr) { pending.delete(m.id); m.error ? pr.reject(new Error(m.error.message)) : pr.resolve(m.result); }
  };
  const ready = new Promise((r) => (ws.onopen = () => {
    if (context) ws.send(JSON.stringify({ method: "ctx.register", params: { context } }));
    r();
  }));
  return { ws, rpc, log, ready };
}

// The frame ordering guarantee: log.write and a following rpc go out on the
// same socket in order, and the server processes log.write synchronously
// before dispatching later RPCs, so a read after logs sees them.
const app = client("app");
await app.ready;

try {
  console.log("\n[1] logs forwarded and read back");
  app.log("reset");                       // page load -> new generation
  app.log("log", "hello from the app");
  app.log("warn", "careful now");
  app.log("error", "TypeError: x is not a function\n    at render (app/index.html:12:5)");
  let r = await app.rpc("console.read", {});
  ok(r.entries.length === 3, "three lines buffered", JSON.stringify(r.entries.map((e) => e.level)));
  ok(r.entries.some((e) => e.level === "error" && /TypeError/.test(e.text)), "error text + stack preserved");
  const gen1 = r.latest_gen;
  ok(typeof gen1 === "number" && gen1 >= 1, "generation assigned", String(gen1));

  console.log("\n[2] only_errors filters to error/warn");
  r = await app.rpc("console.read", { only_errors: true });
  ok(r.entries.length === 2 && r.entries.every((e) => e.level === "error" || e.level === "warn"),
    "only error+warn returned", JSON.stringify(r.entries.map((e) => e.level)));

  console.log("\n[3] reset opens a new generation; only_latest hides the old one");
  app.log("reset");                       // simulates the iframe reloading after an edit
  app.log("log", "fresh line after reload");
  r = await app.rpc("console.read", { only_latest: true });
  ok(r.latest_gen === gen1 + 1, "generation bumped on reset", `${gen1} -> ${r.latest_gen}`);
  ok(r.entries.length === 1 && r.entries[0].text === "fresh line after reload",
    "only_latest shows just the current load", JSON.stringify(r.entries.map((e) => e.text)));
  r = await app.rpc("console.read", { only_latest: false });
  ok(r.entries.length === 4, "only_latest:false shows all generations", String(r.entries.length));

  console.log("\n[4] a plain client (no 'app' context) can still read");
  const shell = client(null);
  await shell.ready;
  r = await shell.rpc("console.read", {});
  ok(r.entries.length >= 1, "read works from any connection");
  shell.ws.close();
} finally {
  app.ws.close();
  proc.kill("SIGKILL");
}

console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
