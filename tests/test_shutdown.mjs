// Lifecycle test: the server exits when the last client disconnects
// (and stays up while one is connected). Also checks per-port auth cookies.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-shutdown";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

function start(device, file, linger = "2") {
  const proc = spawn(BIN, ["open", file, "--headless"], {
    env: { ...process.env, UAPP_DEVICE: device, UAPP_LINGER_SECS: linger },
    stdio: ["ignore", "pipe", "pipe"],
  });
  return new Promise((res, rej) => {
    let b = ""; const t = setTimeout(() => rej(new Error("startup timeout")), 15000);
    proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) { clearTimeout(t); res({ proc, info: JSON.parse(b.slice(0, i)) }); } });
  });
}
const alive = (proc) => proc.exitCode === null;

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

console.log("\n[1] exits when never connected");
{
  const { proc } = await start("devsd1", `${DIR}/a.uapp`);
  const exited = await new Promise((res) => {
    const t = setTimeout(() => res(false), 20000);
    proc.on("exit", () => { clearTimeout(t); res(true); });
  });
  ok(exited, "server exited after linger with zero clients");
}

console.log("\n[2] stays up while connected, exits after disconnect");
{
  const { proc, info } = await start("devsd2", `${DIR}/b.uapp`);
  const token = new URL(info.url).searchParams.get("t");
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${token}`);
  await new Promise((r) => (ws.onopen = r));
  await sleep(9000); // well past the 2s linger
  ok(alive(proc), "server stayed alive with a connected client");
  ws.close();
  const exited = await new Promise((res) => {
    const t = setTimeout(() => res(false), 20000);
    proc.on("exit", () => { clearTimeout(t); res(true); });
  });
  ok(exited, "server exited after last client disconnected");
}

console.log("\n[3] per-port auth cookies don't collide");
{
  const A = await start("devsd3", `${DIR}/c.uapp`, "60");
  const B = await start("devsd4", `${DIR}/d.uapp`, "60");
  const tA = new URL(A.info.url).searchParams.get("t");
  const tB = new URL(B.info.url).searchParams.get("t");
  // Shell response must set a port-scoped cookie
  const shell = await fetch(`http://127.0.0.1:${A.info.port}/?t=${tA}`);
  const setc = shell.headers.get("set-cookie") || "";
  ok(setc.startsWith(`uapp_t_${A.info.port}=`), `cookie is port-scoped (${setc.split(";")[0].split("=")[0]})`);
  ok((shell.headers.get("cache-control") || "").includes("no-store"), "shell served with no-store");
  const js = await fetch(`http://127.0.0.1:${A.info.port}/shell/main.js?t=${tA}`);
  ok((js.headers.get("cache-control") || "").includes("no-store"), "shell/main.js served with no-store");
  // Browser sends BOTH apps' cookies to A (same host) — A must still auth
  const both = await fetch(`http://127.0.0.1:${A.info.port}/app/`, {
    headers: { Cookie: `uapp_t_${A.info.port}=${tA}; uapp_t_${B.info.port}=${tB}` },
  });
  ok(both.status === 200, "iframe reload authenticates with both apps' cookies present");
  // B's cookie alone must NOT authenticate against A
  const wrong = await fetch(`http://127.0.0.1:${A.info.port}/app/`, {
    headers: { Cookie: `uapp_t_${B.info.port}=${tB}` },
  });
  ok(wrong.status === 403, "another app's cookie is rejected");
  A.proc.kill("SIGKILL"); B.proc.kill("SIGKILL");
}

console.log("\n[4] rename + close from the shell");
{
  const { proc, info } = await start("devsd5", `${DIR}/e.uapp`, "600");
  const token = new URL(info.url).searchParams.get("t");
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${token}`);
  const pending = new Map(); let nid = 1; const syncEvents = [];
  await new Promise((r) => (ws.onopen = r));
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event") { if (m.params.type === "sync") syncEvents.push(m.params); return; }
    const p = pending.get(m.id);
    if (p) { pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
  };
  const rpc = (method, params = {}) => new Promise((res, rej) => {
    const id = nid++; pending.set(id, { resolve: res, reject: rej });
    ws.send(JSON.stringify({ id, method, params }));
    setTimeout(() => { if (pending.has(id)) { pending.delete(id); rej(new Error("timeout " + method)); } }, 10000);
  });
  await rpc("app.rename", { name: "Payroll Hub" });
  const infoNow = await rpc("app.info");
  ok(infoNow.name === "Payroll Hub", "app renamed via app.rename");
  let err = null;
  try { await rpc("app.rename", { name: "" }); } catch (e) { err = e; }
  ok(!!err, "empty name rejected");
  // the icons module must be served (shell depends on it)
  const icons = await fetch(`http://127.0.0.1:${info.port}/icons.js?t=${token}`);
  ok(icons.status === 200 && (await icons.text()).includes("uappIcon"), "icons.js served");
  await rpc("app.quit");
  const exited = await new Promise((res) => {
    const t = setTimeout(() => res(false), 10000);
    proc.on("exit", () => { clearTimeout(t); res(true); });
  });
  ok(exited, "app.quit shut the server down");
  ok(syncEvents.some((e) => e.state === "closed"), "clients told to close their window");
  // rename persisted durably (op log) — reopen and check
  const again = await start("devsd5", `${DIR}/e.uapp`, "600");
  const ws2 = new WebSocket(`ws://127.0.0.1:${again.info.port}/ws?t=${new URL(again.info.url).searchParams.get("t")}`);
  const p2 = new Map(); let nid2 = 1;
  await new Promise((r) => (ws2.onopen = r));
  ws2.onmessage = (ev) => { const m = JSON.parse(ev.data); if (m.method === "event") return;
    const p = p2.get(m.id); if (p) { p2.delete(m.id); p(m.result); } };
  const rpc2 = (method, params = {}) => new Promise((res) => { const id = nid2++; p2.set(id, res); ws2.send(JSON.stringify({ id, method, params })); });
  const info2 = await rpc2("app.info");
  ok(info2.name === "Payroll Hub", "rename survived restart (replicated op)");
  again.proc.kill("SIGKILL");
}

console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
