// Install-as-app tests: app.installShortcut creates a launcher entry that
// runs `uapp <file>` (Linux: .desktop in ~/.local/share/applications), and
// shortcutStatus/removeShortcut track it via the machine-local install record
// so a rename never leaves a duplicate launcher behind.
import { spawn } from "node:child_process";
import { rmSync, mkdirSync, existsSync, readFileSync, readdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-install";
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });

// HOME is redirected so the test doesn't touch the real applications menu.
const proc = spawn(BIN, ["open", `${DIR}/a.uapp`, "--headless"], {
  env: { ...process.env, HOME: DIR },
  stdio: ["ignore", "pipe", "pipe"],
});
let out = "";
proc.stdout.on("data", (d) => (out += d));
for (let i = 0; i < 50 && !out.includes("port"); i++) await sleep(100);
const info = JSON.parse(out);
const token = new URL(info.url).searchParams.get("t");

const ws = new WebSocket(`ws://127.0.0.1:${info.port}/ws?t=${token}`);
await new Promise((r) => (ws.onopen = r));
const call = (method, params = {}) => new Promise((res, rej) => {
  const id = Math.random().toString();
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id === id) m.error ? rej(new Error(m.error.message)) : res(m.result);
  };
  ws.send(JSON.stringify({ id, method, params }));
});

const APPS = `${DIR}/.local/share/applications`;
const ICONS = `${DIR}/.local/share/uapp/icons`;
const entries = () => (existsSync(APPS) ? readdirSync(APPS).filter((f) => /^uapp-.*\.desktop$/.test(f)) : []);
const isLinux = process.platform === "linux";

// 0. before anything is installed, status says so (Linux + a saved file)
let r;
try { r = await call("app.shortcutStatus"); } catch (e) { r = { err: e.message }; }
if (isLinux) {
  ok(r.supported === true, "status: supported on linux with a saved file", JSON.stringify(r));
  ok(r.installed === false && r.detail === null, "status: not installed before install", JSON.stringify(r));
} else {
  ok("installed" in (r || {}), "status: responds off-linux", JSON.stringify(r));
}

// 1. install works and reports where the entry landed
try { r = await call("app.installShortcut"); } catch (e) { r = { err: e.message }; }
const desk = `${APPS}/uapp-a.desktop`;
if (existsSync(desk)) {
  const txt = readFileSync(desk, "utf8");
  ok(r.ok && /applications menu/.test(r.message || ""), "installShortcut returns ok + message", JSON.stringify(r));
  ok(txt.includes("Type=Application"), ".desktop is a valid application entry");
  ok(txt.includes(`"${DIR}/a.uapp"`), ".desktop Exec opens the exact .uapp file");
  ok(/^Name=a$/m.test(txt), ".desktop carries the app name");
} else if (!isLinux) {
  ok(true, "non-linux: entry creation skipped in this test");
} else {
  ok(false, "installShortcut created the .desktop entry", JSON.stringify(r));
}

// 2. installing twice (replace) doesn't error
try { r = await call("app.installShortcut"); ok(!!r.ok, "re-install replaces cleanly"); }
catch (e) { ok(false, "re-install replaces cleanly", e.message); }

// 3. status flips to installed, with a human-readable location
try { r = await call("app.shortcutStatus"); } catch (e) { r = { err: e.message }; }
if (isLinux) {
  ok(r.installed === true, "status: installed after install", JSON.stringify(r));
  ok(typeof r.detail === "string" && r.detail.includes("uapp-a.desktop"), "status: detail names the entry", JSON.stringify(r));
}

// 4. rename + re-install replaces the old slug's entry — exactly ONE entry left
try { r = await call("app.rename", { name: "b" }); } catch (e) { r = { err: e.message }; }
try { r = await call("app.installShortcut"); } catch (e) { r = { err: e.message }; }
if (isLinux) {
  ok(!existsSync(desk), "rename: old slug's .desktop is gone");
  ok(existsSync(`${APPS}/uapp-b.desktop`), "rename: new slug's .desktop exists", JSON.stringify(r));
  ok(entries().length === 1, "rename: exactly one launcher entry remains", JSON.stringify(entries()));
}

// 5. custom icon: a tiny valid PNG lands in the icons dir and Icon= points at it
const PNG_1x1 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
try { r = await call("app.installShortcut", { icon_b64: PNG_1x1 }); } catch (e) { r = { err: e.message }; }
const iconFile = `${ICONS}/b.png`;
if (isLinux) {
  ok(r.ok === true, "install with icon succeeds", JSON.stringify(r));
  ok(existsSync(iconFile), "icon file written to <data_dir>/uapp/icons");
  const txt = readFileSync(`${APPS}/uapp-b.desktop`, "utf8");
  ok(txt.includes(`Icon=${iconFile}`), ".desktop Icon= points at the custom icon", txt.match(/^Icon=.*$/m)?.[0]);
}

// 6. invalid icons are rejected with a clear error
try { r = await call("app.installShortcut", { icon_b64: "bm90LWEtcG5n" }); r = { err: null }; }
catch (e) { r = { err: e.message }; }
ok(/PNG/.test(r.err || ""), "non-PNG icon_b64 is rejected", JSON.stringify(r));

// 7. removeShortcut deletes entry + icon; status flips back
try { r = await call("app.removeShortcut"); } catch (e) { r = { err: e.message }; }
if (isLinux) {
  ok(typeof r.message === "string" && !r.err, "removeShortcut returns a message", JSON.stringify(r));
  ok(entries().length === 0, "remove: no launcher entries remain", JSON.stringify(entries()));
  ok(!existsSync(iconFile), "remove: icon file is gone too");
  const s = await call("app.shortcutStatus");
  ok(s.installed === false && s.detail === null, "status: back to not installed", JSON.stringify(s));
  // removing again is an error — there's nothing installed
  try { await call("app.removeShortcut"); ok(false, "second remove errors"); }
  catch (e) { ok(/no launcher/.test(e.message), "second remove errors", e.message); }
}

ws.close();
proc.kill("SIGTERM");
await sleep(300);
console.log(`${failed ? "FAILED" : "PASSED"}: ${passed} ok, ${failed} failed`);
process.exit(failed ? 1 : 0);
