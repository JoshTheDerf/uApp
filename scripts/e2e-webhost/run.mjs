// End-to-end: library + archive-less web host + proxy (one origin) + headless
// chromium. A fake desktop page opens two .uapps from the library in two
// frames; each must run its own app, both at once; publish goes back to the
// library; a bare /uapp/demo/ opens the launcher, whose samples come from the
// library and open by navigating the frame itself. Also the hardening around
// it: who may send the shell RPCs, host.* for the launcher only, secrets
// left out of downloads, failed and steady-stream saves, the tab handover,
// the confirm for an app from another site, and boot failures (a worker that
// does not load, one from another build) on two more web hosts.
import { spawn, execSync } from "node:child_process";
import { mkdirSync, rmSync, existsSync, readdirSync, readFileSync, cpSync, writeFileSync } from "node:fs";
import { connect } from "./cdp.mjs";

// Run:  node scripts/e2e-webhost/run.mjs   (after scripts/build-web.sh and a
// debug build of uapp-server, uapp-library and make-demo-apps; needs
// chromium-browser). Not part of `just test`: it needs the wasm build.
//
// E2E_CHROME     browser binary (default chromium-browser)
// E2E_TMP        where the profile and the shelf go. The default is inside
//                snap chromium's confinement, the only place it honours
//                --user-data-dir (anywhere else it silently uses its own
//                default profile, cached service worker and all).
// E2E_SHOTS=<dir> screenshots of each stage there;  E2E_VERBOSE=1  server logs
const HERE = new URL("./", import.meta.url).pathname;
const REPO = new URL("../../", import.meta.url).pathname.replace(/\/$/, "");
const SHOTS = process.env.E2E_SHOTS ? process.env.E2E_SHOTS.replace(/\/?$/, "/") : "";
const CHROME = process.env.E2E_CHROME || "chromium-browser";
const TMP = process.env.E2E_TMP || `${process.env.HOME}/snap/chromium/common`;
const APPS = `${TMP}/uapp-e2e-apps-${process.pid}`;
const TOKEN = "e2e-token-" + Math.random().toString(36).slice(2);
const port = () => 20000 + Math.floor(Math.random() * 20000);
const LIB = port(), DEMO = port(), PROXY = port(), CDP = port(), BAD = port(), SKEW = port();
let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const procs = [];
const cleanup = []; // directories to remove at the end
const run = (cmd, args, extra = {}) => { const p = spawn(cmd, args, { stdio: ["ignore", "pipe", "pipe"], ...extra }); p.stderr.on("data", (d) => process.env.E2E_VERBOSE && process.stderr.write(`[${cmd.split("/").pop()}] ${d}`)); p.stdout.on("data", (d) => process.env.E2E_VERBOSE && process.stdout.write(`[${cmd.split("/").pop()}] ${d}`)); procs.push(p); return p; };
// Snap chromium's processes are named "chrome" and outlive the pid we
// spawned, and other runs (other people's browsers) may be using chrome too:
// kill exactly the processes running our profile, by pid.
function killProfile(dir) {
  for (const pid of readdirSync("/proc").filter((d) => /^\d+$/.test(d))) {
    let cmd = "";
    try { cmd = readFileSync(`/proc/${pid}/cmdline`, "utf8"); } catch { continue; }
    if (cmd.includes(`--user-data-dir=${dir}\0`) || cmd.endsWith(`--user-data-dir=${dir}`)) {
      try { process.kill(Number(pid), "SIGKILL"); } catch {}
    }
  }
}
const waitHttp = async (url, ms = 20000) => { const t0 = Date.now(); for (;;) { try { const r = await fetch(url); if (r.status < 500) return; } catch {} if (Date.now() - t0 > ms) throw new Error("not up: " + url); await sleep(150); } };

const profile = `${TMP}/uapp-e2e-${process.pid}`;
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

  run(CHROME, ["--headless=new", `--remote-debugging-port=${CDP}`, "--no-sandbox", `--user-data-dir=${profile}`, "--window-size=1400,900", "about:blank"]);
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
    if (SHOTS) await desk.shot(`${SHOTS}desk-fail.png`);
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
  if (SHOTS) await desk.shot(`${SHOTS}desk.png`);

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
  // The pill's own Publish button: it asks for the token (kept in memory only).
  await desk.evalJs(`(async () => { const w = ${F("w1")}; w.prompt = () => "${TOKEN}"; await w.__uappPublish(); })()`);
  const pillText = await desk.evalJs(`(() => { const p = ${F("w1")}.document.getElementById("site-sync-pill"); return p ? p.textContent : null; })()`);
  console.log("  sync pill:", JSON.stringify(pillText));
  ok(pillText !== null && /Published/.test(pillText), "the sync pill reports the publish", pillText);
  // A copy that doesn't know which server version it came from must not
  // overwrite the server without being told to.
  const nb = await desk.evalJs(`(async () => { const w = ${F("w1")}; const keep = w.__uappSiteState.etag; w.__uappSiteState.etag = ""; try { await w.__uappSitePublish("${TOKEN}"); return "published"; } catch (e) { return { status: e.status, noBaseline: !!e.noBaseline }; } finally { w.__uappSiteState.etag = keep; } })()`);
  ok(nb && nb.noBaseline && nb.status === 428, "a copy without a baseline ETag is not published unforced", JSON.stringify(nb));

  // ---- device secrets stay out of downloads ---------------------------------
  // In the browser the API key, MCP headers and tool approvals live in the
  // file (on native they never do). The OPFS copy may keep them; a download
  // must not.
  console.log("\ndownloads leave device secrets behind:");
  const setCfg = (key, value) => `${inner("w1")}.defaultView.uapp.rpc("config.set", ${JSON.stringify({ key, value })})`;
  await desk.evalJs(setCfg("ai", { provider: "anthropic", model: "x", api_key: "sk-e2e-secret-key" }));
  await desk.evalJs(setCfg("mcp", [{ name: "e2e", url: "https://mcp.invalid/", headers: { authorization: "Bearer e2e-mcp-secret" } }]));
  await desk.evalJs(setCfg("approvals.app.e2e", ["run_js"]));
  const secrets = await desk.evalJs(`(async () => {
    const w = ${F("w1")};
    await w.__uappFlushSave();
    const text = (b) => new TextDecoder("latin1").decode(b);
    const dl = text(new Uint8Array(await (await w.fetch(w.__uappBase + "download.uapp")).arrayBuffer()));
    const dir = await (await w.navigator.storage.getDirectory()).getDirectoryHandle("apps");
    const saved = text(new Uint8Array(await (await (await dir.getFileHandle(w.__uappSiteState.id + ".uapp")).getFile()).arrayBuffer()));
    const has = (s) => ({ key: s.includes("sk-e2e-secret-key"), mcp: s.includes("e2e-mcp-secret"), approvals: s.includes("approvals.app.e2e"), sqlite: s.startsWith("SQLite format 3"), provider: s.includes("anthropic") });
    return { download: has(dl), saved: has(saved) };
  })()`);
  console.log("  ", JSON.stringify(secrets));
  ok(secrets.download.sqlite && secrets.download.provider && !secrets.download.key && !secrets.download.mcp && !secrets.download.approvals,
     "a downloaded copy has no API key, MCP headers or approvals (and keeps the provider)");
  ok(secrets.saved.key && secrets.saved.mcp && secrets.saved.approvals, "the copy saved in this browser keeps them");

  // ---- who may talk to the shell ------------------------------------------
  console.log("\nonly the shell's own frames get RPCs:");
  const appQuery = (sql) => `(async () => { const r = await ${inner("w1")}.defaultView.uapp.query(${JSON.stringify(sql)}); return Array.isArray(r) ? r : r.rows; })()`;
  const tableExists = async (name) => (await desk.evalJs(appQuery(`select count(*) n from sqlite_master where name='${name}'`)))[0].n > 0;
  const rpcFrom = (table, id) => `parent.parent.postMessage({ __uappRPC: true, m: { id: ${id}, method: 'sql.exec', params: { sql: 'create table ${table}(x)' } } }, '*')`;
  // The desktop frames the shell: same origin, but not inside it.
  await desk.evalJs(`(() => { ${F("w1")}.postMessage({ __uappRPC: true, m: { id: 901, method: "sql.exec", params: { sql: "create table e2e_from_parent(x)" } } }, "*"); return true; })()`);
  // An opaque-origin iframe inside the app (an ad, a third-party embed).
  await desk.evalJs(`(() => { const d = ${inner("w1")}; const f = d.createElement("iframe"); f.sandbox = "allow-scripts"; f.srcdoc = "<script>${rpcFrom("e2e_from_ad", 902)}</" + "script>"; d.body.appendChild(f); return true; })()`);
  // A same-origin frame the app embeds itself: still ours.
  await desk.evalJs(`(() => { const d = ${inner("w1")}; const f = d.createElement("iframe"); f.srcdoc = "<script>${rpcFrom("e2e_from_embed", 903)}</" + "script>"; d.body.appendChild(f); return true; })()`);
  await sleep(2000);
  ok(!(await tableExists("e2e_from_parent")), "a page framing the shell gets no RPCs");
  ok(!(await tableExists("e2e_from_ad")), "a sandboxed iframe inside the app gets no RPCs");
  ok(await tableExists("e2e_from_embed"), "a same-origin frame nested in the app still does");
  const hostTry = await desk.evalJs(`${inner("w1")}.defaultView.uapp.rpc("host.apps").then(() => "allowed", (e) => String(e.message || e))`);
  ok(/only available to the launcher/.test(hostTry), "host.* is refused to an app opened with ?open=", hostTry);

  // ---- auto-save -------------------------------------------------------------
  console.log("\nauto-save:");
  const t0 = await desk.evalJs(`Date.now()`);
  await desk.evalJs(`(async () => { const u = ${inner("w1")}.defaultView.uapp; await u.exec("create table if not exists e2e_tick(t)"); const end = Date.now() + 7000; while (Date.now() < end) { await u.exec("insert into e2e_tick values(?)", [Date.now()]); await new Promise((r) => setTimeout(r, 400)); } return true; })()`);
  const st1 = await desk.evalJs(`${F("w1")}.__uappSaveState()`);
  ok(st1.lastSavedAt > t0, "a steady stream of writes still gets saved", JSON.stringify(st1));
  // Storage full: OPFS writes (and IndexedDB, should the store be that) fail.
  await desk.evalJs(`(() => { const w = ${F("w1")}; const q = () => new w.DOMException("The quota has been exceeded.", "QuotaExceededError"); w.__e2eWrite = w.FileSystemWritableFileStream.prototype.write; w.FileSystemWritableFileStream.prototype.write = () => Promise.reject(q()); w.__e2ePut = w.IDBObjectStore.prototype.put; w.IDBObjectStore.prototype.put = () => { throw q(); }; return true; })()`);
  await desk.evalJs(`${inner("w1")}.defaultView.uapp.exec("insert into e2e_tick values(1)")`);
  await desk.waitFor(`${F("w1")}.__uappSaveState().state === "failed"`, 15000);
  const failText = await desk.evalJs(`(() => { const b = ${F("w1")}.document.getElementById("uapp-save-failed"); return b ? b.innerText : null; })()`);
  console.log("  notice:", JSON.stringify(failText));
  ok(failText && /could not be saved/.test(failText) && /quota/i.test(failText) && /Download a copy/.test(failText), "a failed save shows a notice with the reason and a download");
  // Closing the document while its last save fails keeps it open.
  await desk.evalJs(`${inner("w1")}.defaultView.uapp.rpc("app.quit", {}).catch(() => 0)`);
  await sleep(2500);
  const closing = await desk.evalJs(`({ href: ${F("w1")}.location.href, notice: (${F("w1")}.document.getElementById("uapp-save-failed") || {}).innerText || "" })`);
  ok(closing.href.includes("open=") && /Close anyway/.test(closing.notice), "closing with a failed save stays put and offers Close anyway", JSON.stringify(closing));
  if (SHOTS) await desk.shot(`${SHOTS}save-failed.png`);
  // Storage works again: Try again saves and the notice goes away.
  await desk.evalJs(`(() => { const w = ${F("w1")}; w.FileSystemWritableFileStream.prototype.write = w.__e2eWrite; w.IDBObjectStore.prototype.put = w.__e2ePut; [...w.document.querySelectorAll("#uapp-save-failed button")].find((b) => b.textContent === "Try again").click(); return true; })()`);
  await desk.waitFor(`${F("w1")}.__uappSaveState().state === "saved" && !${F("w1")}.document.getElementById("uapp-save-failed")`, 15000);
  ok(true, "Try again saves once storage works, and the notice goes away");

  // ---- the same document opened in another tab ------------------------------
  console.log("\nhanding a document to another tab:");
  // Not waited for: the old tab must save it while handing over.
  await desk.evalJs(`${inner("w1")}.defaultView.uapp.exec("create table e2e_handover(x)")`);
  const tab2 = await connect(CDP, `${ORIGIN}/uapp/demo/?open=/uapp/apps/kanban-board.uapp`);
  await tab2.waitFor(`(() => { try { const d = document.getElementById("appframe").contentDocument; return d && d.title === "Kanban Board" && document.getElementById("boot-splash").classList.contains("gone"); } catch { return false; } })()`, 60000);
  const handed = await tab2.evalJs(`(async () => { const r = await document.getElementById("appframe").contentWindow.uapp.query("select count(*) n from sqlite_master where name='e2e_handover'"); return (Array.isArray(r) ? r : r.rows)[0].n; })()`);
  ok(handed === 1, "the new tab opens the copy with the old tab's last edit in it", handed);
  ok(await desk.evalJs(`${F("w1")}.document.body.innerText.includes("Demo opened in another tab")`), "the old tab went dormant");
  await tab2.close();
  await desk.close();

  // ---- an app from another site --------------------------------------------
  console.log("\nan app from another site asks first:");
  const foreign = await connect(CDP, `${ORIGIN}/uapp/demo/?open=${encodeURIComponent(`http://127.0.0.1:${LIB}/kanban-board.uapp`)}`);
  await foreign.waitFor(`!!document.getElementById("boot-ask")`, 60000);
  const ask = await foreign.evalJs(`document.getElementById("boot-status").textContent`);
  ok(ask === `Open an app from 127.0.0.1:${LIB}?`, "a ?open= URL on another origin asks before downloading", ask);
  if (SHOTS) await foreign.shot(`${SHOTS}foreign-ask.png`);
  await foreign.evalJs(`[...document.querySelectorAll("#boot-ask button")].find((b) => b.textContent === "Cancel").click()`);
  await foreign.waitFor(`!location.search.includes("open=") && (() => { try { return document.getElementById("appframe").contentDocument.body.innerText.includes("Drag a sample"); } catch { return false; } })()`, 60000);
  ok(true, "Cancel goes to the launcher instead");
  await foreign.close();

  // ---- a runtime that fails to load, and one from another deploy -------------
  console.log("\nboot failures:");
  const broken = `${TMP}/uapp-e2e-broken-${process.pid}`, skewed = `${TMP}/uapp-e2e-skewed-${process.pid}`;
  cpSync(`${REPO}/dist-web`, broken, { recursive: true, filter: (f) => !f.includes("/examples") });
  cpSync(`${REPO}/dist-web`, skewed, { recursive: true, filter: (f) => !f.includes("/examples") });
  writeFileSync(`${broken}/worker.js`, "this is not javascript (\n");
  // worker.js from "another deploy": its build no longer matches the page's.
  writeFileSync(`${skewed}/worker.js`, readFileSync(`${skewed}/worker.js`, "utf8").replace(/const BUILD = "[^"]*"/, 'const BUILD = "0000000000ff"'));
  cleanup.push(broken, skewed);
  run(`${REPO}/target/debug/uapp-server`, ["serve", "--chrome", broken, "--port", String(BAD)]);
  run(`${REPO}/target/debug/uapp-server`, ["serve", "--chrome", skewed, "--port", String(SKEW)]);
  await waitHttp(`http://127.0.0.1:${BAD}/health`);
  await waitHttp(`http://127.0.0.1:${SKEW}/health`);
  const badPage = await connect(CDP, `http://127.0.0.1:${BAD}/`);
  await badPage.waitFor(`!!document.getElementById("uapp-error")`, 60000);
  const badText = await badPage.evalJs(`document.getElementById("uapp-error").innerText`);
  console.log("  error card:", JSON.stringify(badText));
  ok(/could not start/.test(badText) && /runtime could not load/i.test(badText) && /Try again/.test(badText), "a worker that fails to load shows an error with Try again");
  if (SHOTS) await badPage.shot(`${SHOTS}boot-failed.png`);
  await badPage.close();
  const skew = await connect(CDP, `http://127.0.0.1:${SKEW}/`);
  await skew.waitFor(`!!document.getElementById("uapp-error")`, 60000);
  const skewText = await skew.evalJs(`document.getElementById("uapp-error").innerText`);
  ok(/uApp was updated/.test(skewText), "a worker from another build is reported as an update", skewText);
  if (SHOTS) await skew.shot(`${SHOTS}boot-updated.png`);
  await skew.close();

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
  try {
    await demo.waitFor(`location.search.includes("app=") && (() => { try { const d = ${innerTop}; return d && d.title === "Habit Tracker" && document.getElementById("boot-splash").classList.contains("gone"); } catch { return false; } })()`, 60000);
  } catch (e) {
    console.log("  state:", JSON.stringify(await demo.evalJs(`({ href: location.href, status: document.getElementById("boot-status")?.textContent, detail: document.getElementById("boot-detail")?.textContent, error: document.getElementById("uapp-error")?.innerText, title: (() => { try { return ${innerTop}.title; } catch { return "?"; } })() })`).catch((x) => String(x))));
    if (SHOTS) await demo.shot(`${SHOTS}demo-fail.png`);
    throw e;
  }
  const loc = await demo.evalJs(`location.pathname + location.search`);
  ok(/^\/uapp\/demo\/\?app=[a-z0-9]+$/.test(loc), "the frame navigated itself to ?app=<id>", loc);
  ok((await demo.evalJs(`${innerTop}.title`)) === "Habit Tracker", "and runs the sample");
  // Closing the document goes back to the launcher — still in this window.
  await demo.evalJs(`(async () => { const { rpc } = await import("/uapp/demo/shell/core.js"); return rpc("app.quit", {}).catch((e) => String(e)); })()`);
  await demo.waitFor(`!location.search.includes("app=") && (() => { try { const d = ${innerTop}; return !!(d && d.body && d.body.innerText.includes("Drag a sample")); } catch { return false; } })()`, 60000);
  ok(true, "closing returns to the launcher in the same window");
  if (SHOTS) await demo.shot(`${SHOTS}demo.png`);
  await demo.close();
} catch (e) {
  failed++;
  console.log("  ✗ FAIL:", e.stack || e);
} finally {
  for (const p of procs) { try { p.kill("SIGKILL"); } catch {} }
  killProfile(profile);
  try { rmSync(profile, { recursive: true, force: true }); } catch {}
  try { rmSync(APPS, { recursive: true, force: true }); } catch {}
  for (const d of cleanup) { try { rmSync(d, { recursive: true, force: true }); } catch {} }
}
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
