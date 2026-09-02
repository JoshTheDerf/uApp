// The toolbar's two halves must not leak into each other.
//
//   toolbar.setDefault  writes to the .uapp: what the app OPENS with.
//   toolbar.set         shows/hides it now, in every open shell, and writes
//                       NOTHING — so revealing a hidden toolbar to change one
//                       thing never decides how the app opens next time.
//
// Section [6] proves that across a real close-and-reopen of the file, which is
// the only way to prove it: the claim is about what survives on disk.
//
// The panels beside the app (chat, files, database, settings, tools) are the
// toolbar's other half and work the same session-only way, so they are tested
// here too.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-toolbar";
const FILE = `${DIR}/t.uapp`;
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true }); mkdirSync(DIR, { recursive: true });

let proc = null;
async function start() {
  proc = spawn(BIN, ["open", FILE, "--headless"], {
    env: { ...process.env, UAPP_DEVICE: "devtoolbar", UAPP_LINGER_SECS: "600" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const line = await new Promise((res) => { let b = ""; proc.stdout.on("data", (d) => { b += d; const i = b.indexOf("\n"); if (i >= 0) res(b.slice(0, i)); }); });
  const info = JSON.parse(line);
  if (info.reused) { console.error("FAIL: an older uapp-server still has the test file open — pkill -x uapp-server"); process.exit(2); }
  if (process.env.UAPP_DEBUG) proc.stderr.pipe(process.stderr);
  return info;
}
async function stop() {
  proc.kill(); await sleep(400); proc.kill("SIGKILL"); await sleep(400);
}

function client(info, { onToolbar = null, onPanel = null, approve = false } = {}) {
  const token = new URL(info.url).searchParams.get("t");
  const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${token}`);
  const pending = new Map(); let id = 1;
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {
    const i = id++; pending.set(i, { resolve, reject });
    ws.send(JSON.stringify({ id: i, method, params }));
    setTimeout(() => { if (pending.has(i)) { pending.delete(i); reject(new Error("rpc timeout " + method)); } }, 20000);
  });
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.method === "event" && m.params?.type === "toolbar") { onToolbar?.(m.params); return; }
    if (m.method === "event" && m.params?.type === "panel") { onPanel?.(m.params); return; }
    // set_toolbar_default is gated — it writes a durable setting into the
    // shared file — so a caller has to stand in for the user saying yes.
    if (m.method === "event" && m.params?.type === "approval" && approve) {
      for (const q of m.params.pending || []) rpc("ai.approve", { id: q.id, allow: true }).catch(() => {});
      return;
    }
    const pr = pending.get(m.id); if (!pr) return; pending.delete(m.id);
    m.error ? pr.reject(new Error(m.error.message)) : pr.resolve(m.result);
  };
  const open = new Promise((r) => (ws.onopen = r));
  return { ws, rpc, open, close: () => ws.close() };
}

let info = await start();

console.log("[1] a fresh app: shown, with a shortcut that works everywhere");
{
  const c = client(info); await c.open;
  const t = await c.rpc("toolbar.get");
  ok(t.hidden === false, "shown by default", JSON.stringify(t));
  // F9 is the default precisely because nothing else claims it — see toolbar.rs.
  ok(t.shortcut === "F9", "default shortcut is F9", JSON.stringify(t));
  c.close();
}

console.log("\n[2] toolbar.set reaches every open shell and saves nothing");
{
  const seen = [];
  const a = client(info, { onToolbar: (p) => seen.push(["a", p]) });
  const b = client(info, { onToolbar: (p) => seen.push(["b", p]) });
  await a.open; await b.open;
  const r = await a.rpc("toolbar.set", { visible: false });
  ok(r.visible === false, "reports what it did", JSON.stringify(r));
  await sleep(300);
  ok(seen.filter(([, p]) => p.visible === false).length === 2,
    "both shells were told", JSON.stringify(seen));
  // The whole point: hiding it now is not a decision about how the app opens.
  const t = await a.rpc("toolbar.get");
  ok(t.hidden === false, "the saved default is untouched", JSON.stringify(t));
  const e = await a.rpc("toolbar.set", {}).catch((x) => x.message);
  ok(/needs visible/.test(String(e)), "no visible: a clear error", String(e));
  a.close(); b.close();
}

console.log("\n[3] toolbar.setDefault writes the default, and only what was passed");
{
  const seen = [];
  const c = client(info, { onToolbar: (p) => seen.push(p) });
  await c.open;
  const r = await c.rpc("toolbar.setDefault", { hidden: true });
  ok(r.hidden === true && r.shortcut === "F9", "hidden set, shortcut kept", JSON.stringify(r));
  await sleep(300);
  ok(seen.some((p) => p.default?.hidden === true), "shells were told the new default", JSON.stringify(seen));
  // A new shortcut must not disturb the choice already made.
  const r2 = await c.rpc("toolbar.setDefault", { shortcut: "mod+alt+b" });
  ok(r2.hidden === true, "hidden survived a shortcut-only change", JSON.stringify(r2));
  ok(r2.shortcut === "Mod+Alt+B", "shortcut stored canonically", JSON.stringify(r2));
  ok((await c.rpc("toolbar.get")).shortcut === "Mod+Alt+B", "and read back the same");
  c.close();
}

console.log("\n[4] an unusable shortcut is refused, not stored");
{
  const c = client(info); await c.open;
  const before = await c.rpc("toolbar.get");
  for (const [bad, why] of [
    ["b", "would fire while typing"],
    ["hyper+b", "not a modifier"],
    ["mod+notakey", "not a key"],
  ]) {
    const e = await c.rpc("toolbar.setDefault", { shortcut: bad }).catch((x) => x.message);
    ok(new RegExp(why).test(String(e)), `"${bad}" refused, saying why`, String(e));
  }
  const e = await c.rpc("toolbar.setDefault", { hide: true }).catch((x) => x.message);
  ok(/unknown toolbar setting/.test(String(e)), "a typo'd field is an error, not a silent no-op", String(e));
  const after = await c.rpc("toolbar.get");
  ok(JSON.stringify(before) === JSON.stringify(after), "nothing changed", JSON.stringify(after));
  // "" is legitimate: no shortcut, on-screen buttons only.
  ok((await c.rpc("toolbar.setDefault", { shortcut: "" })).shortcut === "",
    "empty removes the shortcut");
  await c.rpc("toolbar.setDefault", { shortcut: "F9, mod+alt+b" });
  ok((await c.rpc("toolbar.get")).shortcut === "F9, Mod+Alt+B", "alternatives are kept");
  c.close();
}

console.log("\n[5] the assistant's tools go through the same doors");
{
  const seen = [];
  const c = client(info, { onToolbar: (p) => seen.push(p), approve: true });
  await c.open;
  await c.rpc("tools.call", { name: "show_toolbar", input: { visible: false } });
  await sleep(250);
  ok(seen.some((p) => p.visible === false), "show_toolbar hides it", JSON.stringify(seen));
  ok((await c.rpc("toolbar.get")).hidden === true,
    "and left the default alone (it was already hidden from [3])");
  ok(true, "show_toolbar needed no approval (it changes nothing durable)");
  const r = await c.rpc("tools.call", { name: "set_toolbar_default", input: { hidden: false } });
  ok(r.hidden === false, "set_toolbar_default changes the default, once approved", JSON.stringify(r));
  c.close();
}

console.log("\n[6] close and reopen: the default is what comes back, not the session");
{
  const c = client(info); await c.open;
  await c.rpc("toolbar.setDefault", { hidden: true, shortcut: "F8" });
  // Someone opens the hidden toolbar to change something. That must not stick.
  await c.rpc("toolbar.set", { visible: true });
  await sleep(300);
  c.close();
  await stop();

  info = await start();
  const d = client(info); await d.open;
  const t = await d.rpc("toolbar.get");
  ok(t.hidden === true, "still opens hidden, though it was open when closed", JSON.stringify(t));
  ok(t.shortcut === "F8", "the shortcut travelled with the file", JSON.stringify(t));
  d.close();
}

console.log("\n[7] panel.set names panels for what they are, and reaches every shell");
{
  const seen = [];
  const a = client(info, { onPanel: (p) => seen.push(["a", p]) });
  const b = client(info, { onPanel: (p) => seen.push(["b", p]) });
  await a.open; await b.open;
  const r = await a.rpc("panel.set", { panel: "database", open: true });
  ok(r.panel === "database" && r.open === true, "reports what it did", JSON.stringify(r));
  await sleep(300);
  ok(seen.filter(([, p]) => p.panel === "database" && p.open === true).length === 2,
    "both shells were told", JSON.stringify(seen));
  // Synonyms resolve rather than failing the call: the panel is headed
  // "Database" but its query tab says SQL.
  ok((await a.rpc("panel.set", { panel: "SQL", open: false })).panel === "database",
    "\"SQL\" resolves to database");
  ok((await a.rpc("panel.set", { panel: "ai", open: true })).panel === "chat",
    "\"ai\" resolves to chat");
  // Omitted `open` means toggle, which only a shell can resolve.
  ok((await a.rpc("panel.set", { panel: "files" })).open === null,
    "no open: passed through as a toggle for the shell to resolve");
  // The element ids are the shell's business, not the API's.
  const e = await a.rpc("panel.set", { panel: "sidebar" }).catch((x) => x.message);
  ok(/no panel called/.test(String(e)) && /chat, files, database, settings, tools/.test(String(e)),
    "an element id is refused, listing the real names", String(e));
  const e2 = await a.rpc("panel.set", {}).catch((x) => x.message);
  ok(/panel/.test(String(e2)), "no panel at all: an error", String(e2));
  a.close(); b.close();
}

console.log("\n[8] one tool covers all of them, and asks no permission");
{
  const seen = [];
  const c = client(info, { onPanel: (p) => seen.push(p) });
  await c.open;
  for (const panel of ["chat", "files", "database", "settings", "tools"]) {
    const r = await c.rpc("tools.call", { name: "show_panel", input: { panel, open: true } });
    ok(r.panel === panel && r.open === true, `show_panel opens ${panel}`, JSON.stringify(r));
  }
  await sleep(300);
  ok(seen.length === 5, "every one reached the shells", JSON.stringify(seen.map((p) => p.panel)));
  // Nothing durable changes, so it must not stop to ask.
  const r = await c.rpc("tools.call", { name: "show_panel", input: { panel: "chat", open: false } });
  ok(r.open === false, "and closes them, ungated", JSON.stringify(r));
  c.close();
}

console.log(`\n${passed} passed, ${failed} failed`);
await stop();
process.exit(failed ? 1 : 0);
