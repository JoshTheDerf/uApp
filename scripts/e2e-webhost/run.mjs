// End-to-end: library + archive-less web host + proxy (one origin) + headless
// chromium. A fake desktop page opens two .uapps from the library in two
// frames; each must run its own app, both at once; publish goes back to the
// library; a bare /uapp/demo/ opens the launcher, whose samples come from the
// library and open by navigating the frame itself.
import { spawn, execSync } from "node:child_process";
import { mkdirSync, rmSync, existsSync } from "node:fs";
import { connect } from "./cdp.mjs";

// Run:  node scripts/e2e-webhost/run.mjs   (after scripts/build-web.sh and a
// debug build of uapp-server, uapp-library and make-demo-apps; needs
// chromium-browser). Not part of `just test`: it needs the wasm build.
const HERE = new URL("./", import.meta.url).pathname;
const REPO = new URL("../../", import.meta.url).pathname.replace(/\/$/, "");
const APPS = `${process.env.HOME}/snap/chromium/common/uapp-e2e-apps-${process.pid}`;
const TOKEN = "e2e-token-" + Math.random().toString(36).slice(2);
const port = () => 20000 + Math.floor(Math.random() * 20000);
const LIB = port(), DEMO = port(), PROXY = port(), CDP = port();
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const procs = [];
const run = (cmd, args, extra = {}) => { const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], ...extra }); p.stderr.on("data", (d) => process.env.E2E_VERBOSE && process.stderr.write(`[${cmd.split("/").pop()}] ${d}`)); p.stdout.on("data", (d) => process.env.E2E_VERBOSE && process.stdout.write(`[${cmd.split("/").pop()}] ${d}`)); procs.push(p); return p; };
const waitHttp = async (url, ms = 20000) => { const t0 = Date.now(); for (;;) { try { const r = await fetch(url); if (r.status < 500) return; } catch {} if (Date.now() - t0 > ms) throw new Error("not up: " + url); await sleep(150); } };

const profile = `${process.env.HOME}/snap/chromium/common/uapp-e2e-${process.pid}`;
mkdirSync(profile, { recursive: true });
// The shelf: a launcher whose samples point at the library, plus two samples.
mkdirSync(APPS, { recursive: true });
execSync(`UAPP_SAMPLES_BASE=/uapp/apps/ "${REPO}/target/debug/make-demo-apps" "${APPS}"`, { cwd: REPO, stdio: "ignore" });
for (const f of ["kanban-board.uapp", "habit-tracker.uapp"]) execSync(`cp "${REPO}/examples/${f}" "${APPS}/"`);
try {
  run(`${REPO}/target/debug/uapp-library`, [APPS, "--port", String(LIB), "--token", TOKEN]);
  run(`${REPO}/target/debug/uapp-server`, ["serve", "--chrome", `${REPO}/dist-web`, "--port", String(DEMO), "--open", "/uapp/apps/launcher.uapp"]);
  run("node", [`${HERE}proxy.mjs`, String(DEMO), String(LIB), String(PROXY)]);
  await waitHttp(`http://127.0.0.1:${LIB}/`);
  await waitHttp(`http://127.0.0.1:${DEMO}/health`);
  await waitHttp(`http://127.0.0.1:${PROXY}/desk.html`);
  const ORIGIN = `http://127.0.0.1:${PROXY}`;

  // The host serves its page with the default-open meta and COI headers through the proxy.
  {
    const r = await fetch(`${ORIGIN}/uapp/demo/`);
    const t = await r.text();
    ok(r.status === 200 && t.includes('name="uapp-open" content="/uapp/apps/launcher.uapp"'), "web host page carries the default-open meta");
    ok(r.headers.get("cross-origin-embedder-policy") === "credentialless", "COI headers on the page");
    const l = await (await fetch(`${ORIGIN}/uapp/apps/`)).json();
    ok(l.apps.map((a) => a.name).join() === "habit-tracker.uapp,kanban-board.uapp,launcher.uapp", "library lists the shelf", JSON.stringify(l.apps.map((a) => a.name)));
  }

  run("chromium-browser", ["--headless=new", `--remote-debugging-port=${CDP}`, "--no-sandbox", `--user-data-dir=${profile}`, "--window-size=1400,900", "about:blank"]);
  await waitHttp(`http://127.0.0.1:${CDP}/json/version`, 30000);

  // ---- two apps in two frames of one page -----------------------------------
  console.log("desktop with two app windows:");
  const desk = await connect(CDP, `${ORIGIN}/desk.html`);
  const F = (id) => `document.getElementById("${id}").contentWindow`;
  const inner = (id) => `${F(id)}.document.getElementById("appframe").contentDocument`;
  const ready = (id) => `(() => { try { const w = ${F(id)}; const d = ${inner(id)}; return !!(w.__uappBase && d && d.title && d.readyState === "complete" && w.document.getElementById("boot-splash") && w.document.getElementById("boot-splash").classList.contains("gone")); } catch (e) { return false; } })()`;
  try {
    await desk.waitFor(ready("w1"), 90000);
    await desk.waitFor(ready("w2"), 90000);
  } catch (e) {
    console.log("  (frames did not come up:", e.message, ")");
    if (process.env.E2E_SHOTS) await desk.shot(`${HERE}desk-fail.png`);
    const s1 = await desk.evalJs(`(() => { try { const w = ${F("w1")}; return { base: w.__uappBase, splash: w.document.getElementById("boot-status")?.textContent, detail: w.document.getElementById("boot-detail")?.textContent, body: w.document.body.innerText.slice(0, 300) }; } catch (e) { return String(e); } })()`);
    console.log("  w1 state:", JSON.stringify(s1));
    const s2 = await desk.evalJs(`(() => { try { const w = ${F("w2")}; return { base: w.__uappBase, splash: w.document.getElementById("boot-status")?.textContent, detail: w.document.getElementById("boot-detail")?.textContent, body: w.document.body.innerText.slice(0, 300) }; } catch (e) { return String(e); } })()`);
    console.log("  w2 state:", JSON.stringify(s2));
    throw e;
  }
  await sleep(1000);
  const splash = await desk.evalJs(`(() => { const s = ${F("w1")}.document.getElementById("boot-splash"); const c = ${F("w1")}.getComputedStyle(s); return { display: c.display, opacity: c.opacity, cls: s.className }; })()`);
  console.log("  splash after ready:", JSON.stringify(splash));
  ok(splash.display === "none", "the boot splash is gone once the app painted", JSON.stringify(splash));
  const st = await desk.evalJs(`({
    b1: ${F("w1")}.__uappBase, b2: ${F("w2")}.__uappBase,
    t1: ${inner("w1")}.title, t2: ${inner("w2")}.title,
    u1: ${F("w1")}.document.getElementById("appframe").src, u2: ${F("w2")}.document.getElementById("appframe").src,
    a1: ${F("w1")}.__uappSiteArchive, a2: ${F("w2")}.__uappSiteArchive,
    overlay1: ${F("w1")}.document.body.innerText.includes("Demo opened in another tab"),
    overlay2: ${F("w2")}.document.body.innerText.includes("Demo opened in another tab"),
    iso1: ${F("w1")}.crossOriginIsolated, iso2: ${F("w2")}.crossOriginIsolated,
    top: location.href,
  })`);
  console.log("  ", JSON.stringify(st));
  ok(/^\/uapp\/demo\/i\/[a-z0-9]+\/$/.test(st.b1) && /^\/uapp\/demo\/i\/[a-z0-9]+\/$/.test(st.b2) && st.b1 !== st.b2, "each shell has its own instance prefix");
  ok(st.u1.startsWith(ORIGIN + st.b1 + "app/") && st.u2.startsWith(ORIGIN + st.b2 + "app/"), "app frames live under their shell's prefix");
  ok(st.t1 === "Kanban Board", "frame 1 runs the Kanban app", st.t1);
  ok(st.t2 === "Habit Tracker", "frame 2 runs the Habit app", st.t2);
  ok(!st.overlay1 && !st.overlay2, "neither shell went dormant");
  ok(st.a1 === `${ORIGIN}/uapp/apps/kanban-board.uapp` && st.a2 === `${ORIGIN}/uapp/apps/habit-tracker.uapp`, "archives resolved to the library URLs");
  ok(st.iso1 && st.iso2, "both frames are cross-origin isolated (SharedArrayBuffer bridge available)");
  ok(st.top.endsWith("/desk.html"), "the desktop page itself never navigated");

  // Toolbar messages from an app page reach ITS shell, not the desktop top.
  const tb = await desk.evalJs(`(async () => { const w = ${F("w1")}; const d = ${inner("w1")}; const u = d.defaultView.uapp; if (!u || !u.toolbar) return "no uapp.toolbar"; const s = await u.toolbar.state?.(); return s === null ? "null" : typeof s; })()`);
  console.log("  toolbar.state() from the app frame ->", tb);
  ok(tb === "object", "the toolbar handshake found the shell (not the top window)", tb);

  // A query in each frame hits its own database.
  const q = await desk.evalJs(`(async () => {
    const sql = "select name from sqlite_master where type='table' and name not like 'uapp_%' and name != 'sqlar' order by name";
    const q1 = await ${inner("w1")}.defaultView.uapp.query(sql);
    const q2 = await ${inner("w2")}.defaultView.uapp.query(sql);
    const names = (q) => (Array.isArray(q) ? q : (q && q.rows) || []).map((r) => r.name).join(","); return [names(q1), names(q2)];
  })()`);
  console.log("  tables:", JSON.stringify(q));
  ok(q[0] && q[1] && q[0] !== q[1], "each frame talks to its own engine (different schemas)", JSON.stringify(q));
  if (process.env.E2E_SHOTS) await desk.shot(`${HERE}desk.png`);

  // ---- publish back to the library ----------------------------------------
  console.log("\npublish to the library:");
  const before = (await (await fetch(`${ORIGIN}/uapp/apps/`)).json()).apps.find((a) => a.name === "kanban-board.uapp").etag;
  await desk.evalJs(`${inner("w1")}.defaultView.uapp.exec("create table if not exists e2e_marker(x)")`);
  await sleep(2500); // auto-save debounce
  const pub = await desk.evalJs(`(async () => { try { return await ${F("w1")}.__uappSitePublish("${TOKEN}"); } catch (e) { return { error: String(e.message || e), status: e.status }; } })()`);
  console.log("  ", JSON.stringify(pub));
  ok(pub && pub.ok === true && pub.etag, "publish accepted by the library", JSON.stringify(pub));
  const after = (await (await fetch(`${ORIGIN}/uapp/apps/`)).json()).apps.find((a) => a.name === "kanban-board.uapp").etag;
  ok(after !== before && after === pub.etag, "the shelf's ETag moved to the published copy");
  const bad = await desk.evalJs(`(async () => { try { return await ${F("w1")}.__uappSitePublish("wrong-token"); } catch (e) { return { error: String(e.message || e), status: e.status }; } })()`);
  ok(bad && bad.status === 403, "a wrong token is refused", JSON.stringify(bad));
  // The pill's own Publish button (token remembered for the session).
  await desk.evalJs(`(async () => { const w = ${F("w1")}; w.sessionStorage.setItem("uapp.publish.token", "${TOKEN}"); await w.__uappPublish(); })()`);
  const pillText = await desk.evalJs(`(() => { const p = ${F("w1")}.document.getElementById("site-sync-pill"); return p ? p.textContent : null; })()`);
  console.log("  sync pill:", JSON.stringify(pillText));
  ok(pillText !== null && /Published/.test(pillText), "the sync pill reports the publish", pillText);
  await desk.close();

  // ---- a bare visit opens the launcher; a sample opens by navigating the frame --
  console.log("\nbare /uapp/demo/ opens the launcher from the library:");
  const demo = await connect(CDP, `${ORIGIN}/uapp/demo/`);
  const innerTop = `document.getElementById("appframe").contentDocument`;
  await demo.waitFor(`(() => { try { const d = ${innerTop}; return !!(d && d.body && d.body.innerText.includes("Drag a sample") && document.getElementById("boot-splash").classList.contains("gone")); } catch { return false; } })()`, 90000);
  const chips = await demo.evalJs(`[...${innerTop}.querySelectorAll(".chip[data-url]")].map((c) => c.dataset.url).slice(0, 3)`);
  ok(chips.length > 0 && chips.every((u) => u.startsWith("/uapp/apps/")), "sample chips point at the library", JSON.stringify(chips));
  ok((await demo.evalJs(`window.__uappSiteArchive`)) === `${ORIGIN}/uapp/apps/launcher.uapp`, "the launcher came from the library (default open)");
  // Open a sample: the launcher page asks its host; the shell fetches from the
  // library, stores it, and navigates THIS window to ?app=<id>.
  await demo.evalJs(`${innerTop}.defaultView.uapp.rpc("host.sample", { url: "/uapp/apps/habit-tracker.uapp", name: "Habit Tracker" }).catch((e) => String(e))`);
  await demo.waitFor(`location.search.includes("app=") && (() => { try { const d = ${innerTop}; return d && d.title === "Habit Tracker" && document.getElementById("boot-splash").classList.contains("gone"); } catch { return false; } })()`, 60000);
  const loc = await demo.evalJs(`location.pathname + location.search`);
  ok(/^\/uapp\/demo\/\?app=[a-z0-9]+$/.test(loc), "the frame navigated itself to ?app=<id>", loc);
  ok((await demo.evalJs(`${innerTop}.title`)) === "Habit Tracker", "and runs the sample");
  // Closing the document goes back to the launcher — still in this window.
  await demo.evalJs(`(async () => { const { rpc } = await import("/uapp/demo/shell/core.js"); return rpc("app.quit", {}).catch((e) => String(e)); })()`);
  await demo.waitFor(`!location.search.includes("app=") && (() => { try { const d = ${innerTop}; return !!(d && d.body && d.body.innerText.includes("Drag a sample")); } catch { return false; } })()`, 60000);
  ok(true, "closing returns to the launcher in the same window");
  if (process.env.E2E_SHOTS) await demo.shot(`${HERE}demo.png`);
  await demo.close();
} catch (e) {
  failed++;
  console.log("  ✗ FAIL:", e.stack || e);
} finally {
  for (const p of procs) { try { p.kill("SIGKILL"); } catch {} }
  try { execSync("pkill -9 -x chrome 2>/dev/null || true"); } catch {}
  try { rmSync(profile, { recursive: true, force: true }); } catch {}
  try { rmSync(APPS, { recursive: true, force: true }); } catch {}
}
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
