/* uapp browser build — this page's side of the service worker: registering
 * it (and handing over to a new version at once), telling it this client is
 * a shell, and answering its asks for archive files, downloads and uploads
 * out of the engine. Also where a page learns that a deploy moved on. */

import { downloadName, flushSave, saveFailureBanner } from "./autosave.js?v=__UAPP_BUILD__";
import { BASE, BUILD, INST, b64FromBytes, bytesFromB64, enc, errText, markLeaving } from "./env.js?v=__UAPP_BUILD__";
import { showBanner, splashNote } from "./splash.js?v=__UAPP_BUILD__";
import { appIsReady, appReady, exportBytes, sendRpc, zombie } from "./worker-bridge.js?v=__UAPP_BUILD__";

const CTYPES = {
  html: "text/html; charset=utf-8", htm: "text/html; charset=utf-8",
  js: "text/javascript; charset=utf-8", mjs: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8", json: "application/json", svg: "image/svg+xml",
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", ico: "image/x-icon", avif: "image/avif", csv: "text/csv",
  txt: "text/plain; charset=utf-8", md: "text/markdown; charset=utf-8",
  markdown: "text/markdown; charset=utf-8", mp4: "video/mp4", webm: "video/webm",
  ogv: "video/ogg", mov: "video/quicktime", m4v: "video/x-m4v", mkv: "video/x-matroska",
  mp3: "audio/mpeg", wav: "audio/wav", m4a: "audio/mp4", flac: "audio/flac",
  ogg: "audio/ogg", woff2: "font/woff2", woff: "font/woff", ttf: "font/ttf",
  wasm: "application/wasm", map: "application/json", pdf: "application/pdf",
};
function contentTypeFor(name) {
  const ext = name.split(".").pop().toLowerCase();
  return CTYPES[ext] || "application/octet-stream";
}

// Same idea as the native server's inject_viewport: make sure app pages get a
// viewport and uapp.js. Nothing else — see __uappWasmHost in env.js. Stray copies
// of the marker this used to inject (persisted by a site's own build) are
// removed so the content heals on its next publish.
function injectHtml(text) {
  text = stripWasmMarker(text);
  const lower = text.toLowerCase();
  const hasVp = lower.includes('name="viewport"') || lower.includes("name='viewport'");
  const hasUapp = lower.includes("uapp.js");
  let inject = "";
  if (!hasVp) inject += '\n<meta name="viewport" content="width=device-width, initial-scale=1">';
  // Relative to the bundle, not "/uapp.js": hosted under a subpath the origin
  // root is somebody else's server.
  if (!hasUapp) inject += `\n<script src="${BASE.pathname}uapp.js"></script>`;
  if (!inject) return text;
  const pos = lower.indexOf("<head");
  if (pos >= 0) {
    const gt = text.indexOf(">", pos);
    if (gt >= 0) return text.slice(0, gt + 1) + inject + text.slice(gt + 1);
  }
  return inject + "\n" + text;
}

function stripWasmMarker(text) {
  return text.replace(/\s*<script>\s*window\.__uappWasm\s*=\s*1;?\s*<\/script>/gi, "");
}

// ---- updates --------------------------------------------------------------------
// A page left open across a deploy keeps its old boot.js while anything it
// loads later comes from the new one. Say so (once), and reload only after
// a save.
let updateShown = false;
function noteBuild(other) {
  if (!other || other === BUILD || BUILD.startsWith("__") || updateShown) return;
  updateShown = true;
  showBanner("uapp-updated", "uApp was updated", "Reload to get the new version. Your changes are saved first.",
    [["Reload", async () => {
      if (await flushSave()) { markLeaving(); location.reload(); } else saveFailureBanner();
    }], ["Later", (el) => el.remove()]]);
}
// After a failed boot: was it because the server now has another build?
export async function serverMovedOn() {
  if (BUILD.startsWith("__")) return false;
  try {
    const r = await fetch(new URL("uapp-build.json", BASE), { cache: "no-store" });
    const j = r.ok ? await r.json() : null;
    return !!(j && j.build && j.build !== BUILD);
  } catch { return false; }
}

// ---- archive requests -------------------------------------------------------------

async function handleSwRequest(m) {
  if (zombie) return; // another tab owns the demo — let it answer
  // Addressed to another shell on this page/origin: not ours to answer, not
  // even with a passthrough — silence lets the right shell's ack win.
  if (m.inst && m.inst !== INST) return;
  // controller is null until the SW claims us; registration.active is set as
  // soon as it activates, so this works on the very first load too (there is
  // no longer a reload to guarantee a controller — see the boot sequence).
  const swPost = (msg) => {
    const sw = navigator.serviceWorker.controller
      || (navigator.serviceWorker.__uappReg && navigator.serviceWorker.__uappReg.active);
    if (sw) sw.postMessage(msg);
  };
  const reply = (data) => data.swAck
    ? swPost({ swAck: true, id: m.id, inst: INST })
    : swPost({ swReply: true, id: m.id, inst: INST, ...data });
  if (m.build) noteBuild(m.build);
  // Until an app is open, anything the TOP document asks for is the server's
  // own resource — in hosted-site mode that is the page the reader is looking
  // at, still loading its scripts. Waiting on appReady here would stall them
  // (and deadlock on /site.uapp itself, which is what appReady is waiting on).
  // Nested frames only exist once the shell is up, so they keep waiting.
  if (!appIsReady && m.frameType === "top-level") {
    reply({ passthrough: true });
    return;
  }
  // Ours: say so now, so the SW waits for the body instead of giving up.
  reply({ swAck: true });
  try {
    // ...but an ack buys the worker a 30 s reply window, so never hold a
    // request open on an engine that may not arrive. Until now this was a bare
    // `await appReady`: if the archive was still downloading, or the engine
    // was wedged, or this page got frozen right after acking, every request
    // the worker asked about sat PENDING for the full 30 s — which looks
    // exactly like "site.uapp is loading and nothing happens". Hand it to the
    // network instead; the server has the published copy of every real page.
    const ready = await Promise.race([
      appReady.then(() => true, () => false),
      new Promise((r) => setTimeout(() => r(false), 8000)),
    ]);
    if (!ready) { reply({ passthrough: true }); return; }
    const url = new URL(m.path, location.origin);
    const p = url.pathname;
    if (p === "/download.uapp") {
      reply({ body: await exportBytes(true), contentType: "application/octet-stream",
              disposition: `attachment; filename="${downloadName()}"` });
      return;
    }
    if (p === "/template.uapp") {
      const r = await sendRpc("app.exportTemplate");
      const name = (r.name || "app").replace(/[^\w \-.]/g, "_");
      reply({ bodyB64: r.b64, contentType: "application/octet-stream",
              disposition: `attachment; filename="${name} template.uapp"` });
      return;
    }
    if (p === "/upload") {
      const name = url.searchParams.get("name") || "";
      const r = await sendRpc("files.write", { name, b64: m.bodyB64 || "" });
      reply({ bodyB64: b64FromBytes(enc.encode(JSON.stringify({ ok: true, name: r.name }))),
              contentType: "application/json" });
      return;
    }
    if (p === "/scratch/" || p === "/scratch") {
      const html = await (await fetch(new URL("scratch.html", BASE))).text();
      // The context flag must land before uapp.js connects.
      reply({ bodyB64: b64FromBytes(enc.encode(html)), contentType: "text/html; charset=utf-8" });
      return;
    }
    // Archive file. Like the native /app/* route, the /app/ URL prefix is a
    // ROUTE, not part of the archive name: /app/<ref> serves <ref>, where a
    // bare ref resolves under app/ (viewer URLs pass full archive names like
    // app/index.html or data/sales.csv after the prefix). Other paths
    // (/data/x, /vendor/x, root-absolute refs) are used as-is.
    let name;
    if (p === "/app" || p === "/app/") name = "app/index.html";
    else if (p.startsWith("/app/")) name = decodeURIComponent(p.slice(5));
    else name = decodeURIComponent(p.slice(1));
    // Directory and extensionless forms, so a hosted site browses in here the
    // same way it does on the server (see resolve() in src/public.rs): a page
    // linking to "../" or "how-do-bikes-work/" must land on that page's
    // index.html, not 404.
    const tries = [name];
    if (name === "" || name.endsWith("/")) tries.push(name + "index.html");
    else if (!name.split("/").pop().includes(".")) {
      tries.push(name + ".html", name + "/index.html");
    }
    let r = null, lastErr = null;
    for (const cand of tries) {
      try { r = await sendRpc("files.read", { name: cand }); name = cand; break; }
      catch (e) { lastErr = e; }
    }
    if (!r) {
      // A definite miss, as opposed to "nobody answered": the SW treats the
      // two differently (see archiveResponse in sw.js).
      reply({ error: (lastErr && lastErr.message) || ("not found: " + name), status: 404, notFound: true });
      return;
    }
    const ctype = contentTypeFor(r.name || name);
    let bodyB64 = r.b64;
    if (ctype.startsWith("text/html")) {
      bodyB64 = b64FromBytes(enc.encode(injectHtml(new TextDecoder().decode(bytesFromB64(r.b64)))));
    }
    reply({ bodyB64, contentType: ctype });
  } catch (e) {
    reply({ error: String((e && e.message) || e), status: 404 });
  }
}

// ---- registration -------------------------------------------------------------------
// Resolves once the worker is active (and, as far as it will be, in control
// of this page). Throws with a message for the visitor when it can't start.
export async function startServiceWorker() {
  if (!("serviceWorker" in navigator)) {
    throw new Error("This browser has no service-worker support (or the page is not served over HTTPS/localhost).");
  }
  splashNote("Starting the service worker…");
  try {
    navigator.serviceWorker.__uappReg =
      await navigator.serviceWorker.register(new URL("sw.js", BASE));
    await navigator.serviceWorker.ready;
  } catch (e) {
    // Blocking cookies/site data for the origin blocks this too, and without
    // the service worker there is no way to serve an app's files at all.
    throw new Error(e && e.name === "SecurityError"
      ? "This browser is blocking storage for the site, so the demo's service worker can't start. Allow cookies and site data for this site (or try a normal window) and reload."
      : "The demo's service worker could not start: " + errText(e));
  }
  // A newer sw.js must take over NOW, not when the old one happens to go
  // idle: tell the old worker to drop its pending asks and the new one to
  // skip waiting. Covers a worker already waiting at registration and any
  // update found while the page is open.
  const handoff = (reg) => {
    const w = reg.waiting;
    if (!w) return;
    try { reg.active && reg.active.postMessage({ release: true }); } catch {}
    try { w.postMessage({ skipWaiting: true }); } catch {}
  };
  {
    const reg = navigator.serviceWorker.__uappReg;
    handoff(reg);
    reg.addEventListener("updatefound", () => {
      const w = reg.installing;
      if (w) w.addEventListener("statechange", () => { if (w.state === "installed") handoff(reg); });
    });
  }
  // No reload here. sw.js claims clients on activate, so we just wait for it
  // (and carry on uncontrolled if it never lands — askShell asks
  // uncontrolled clients too, and replies go via registration.active).
  if (!navigator.serviceWorker.controller) {
    await new Promise((resolve) => {
      const done = () => resolve();
      navigator.serviceWorker.addEventListener("controllerchange", done, { once: true });
      setTimeout(done, 3000);
    });
  }
  navigator.serviceWorker.addEventListener("message", (ev) => {
    if (ev.data && ev.data.swRequest) handleSwRequest(ev.data);
    else if (ev.data && ev.data.swBuild) noteBuild(ev.data.swBuild);
  });
  // A tab left open for days never navigates, so nothing checks for a new
  // service worker; look when it comes back into view (at most twice an
  // hour). A new one takes over, and its build reaches noteBuild.
  {
    let checked = Date.now();
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState !== "visible" || Date.now() - checked < 30 * 60000) return;
      checked = Date.now();
      try { navigator.serviceWorker.__uappReg.update().catch(() => {}); } catch {}
    });
  }

  // Tell the worker this client is the shell. On a hosted site the shell's
  // pages live at the site's own arbitrary paths (thederf.com/, /admin,
  // /posts/x), which no path rule in sw.js can recognise — and it treats a
  // client it does not recognise as somebody else's page and keeps its
  // hands off it entirely. Re-announced whenever a new worker takes over,
  // since the set does not survive a worker restart.
  {
    const announce = () => {
      const sw = navigator.serviceWorker.controller
        || (navigator.serviceWorker.__uappReg && navigator.serviceWorker.__uappReg.active);
      if (sw) sw.postMessage({ shellClient: true, inst: INST, build: BUILD });
    };
    announce();
    navigator.serviceWorker.addEventListener("controllerchange", announce);
  }
}
