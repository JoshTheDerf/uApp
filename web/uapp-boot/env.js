/* uapp browser build — what this page is: its build, where the bundle lives,
 * which shell instance it is and which document it opens. Evaluated first
 * (every other module imports it), so the window flags below are set before
 * anything else runs. Also the few bits of state every module reads. */

// This page's build (build-web.sh stamps it, and writes it to
// uapp-build.json). The worker and the service worker report theirs; any
// difference means a deploy happened under this page.
export const BUILD = "__UAPP_BUILD__";
export const enc = new TextEncoder();
// How pages in our frames know they are in the browser build: uapp.js walks
// up to the window carrying this flag and talks to it. A property on THIS
// window, deliberately not a <script> injected into their HTML — a site whose
// build pipeline renders pages from fetched HTML would persist such a script
// into its own files (this happened), and a stored "I am inside the wasm
// frame" marker then breaks the very page it is served in at top level.
window.__uappWasmHost = true;
// The demo can be hosted under a subpath (e.g. /uapp/demo/) — everything we
// load or register is relative to where boot.js lives (the bundle root, one
// level up from this file).
export const BASE = new URL("../", import.meta.url);
// This shell's instance id. Several shells can share one origin and one
// service worker — a desktop page with two apps open in two frames, two tabs
// of the demo — and the worker has to know which shell an app frame's fetch
// belongs to. So every URL the shell builds for its frames goes under
// "<base>i/<inst>/" (the shell modules read window.__uappBase), the worker
// splits the segment off again (sw.js splitInst) and asks exactly this page.
export const INST = Math.random().toString(36).slice(2, 10);
// A hosted site's editing chrome (site-chrome.js) is the one shell on its
// page and its frame URLs are the SITE's URLs (/app/<page> mirrors to
// /<page> in the address bar, links resolve relative to them): no segment
// there. Everywhere else — the standalone page, a frame in someone's desktop
// — the shell may have siblings and takes one.
window.__uappBase = BASE.pathname + (window.__uappChrome ? "" : "i/" + INST + "/");

// What this page opens — decided by the URL, so that switching documents is a
// navigation of THIS window (a frame in someone's desktop stays a frame) and
// nothing has to survive in tab-wide storage:
//   site      an archive fetched from a URL: a hosted site's own /site.uapp
//             (site-chrome.js sets __uappSiteArchive before importing us),
//             ?open=<url>, or the page's default (<meta name="uapp-open">,
//             which `uapp serve --open` injects). Persisted in this browser
//             from the first edit on, publishable back to the URL.
//   app       ?app=<id>: a document of the OPFS app library.
//   launcher  nothing named: the launcher.uapp next to boot.js, ephemeral.
// `via` says who chose a site archive: the hosted site's chrome, the page's
// default (the operator), or a ?open= parameter (whoever wrote the link).
export const OPEN = (() => {
  const abs = (u) => { try { return new URL(u, location.href).href; } catch { return ""; } };
  const meta = document.querySelector('meta[name="uapp-open"]');
  const def = abs((meta && meta.getAttribute("content")) || "");
  if (window.__uappSiteArchive) return { kind: "site", url: abs(window.__uappSiteArchive), via: "chrome" };
  const q = new URL(location.href).searchParams;
  if (q.get("app")) return { kind: "app", id: q.get("app") };
  const o = q.get("open") ? abs(q.get("open")) : "";
  if (o) return { kind: "site", url: o, via: o === def ? "default" : "param" };
  if (def) return { kind: "site", url: def, via: "default" };
  return { kind: "launcher" };
})();
if (OPEN.kind === "site") window.__uappSiteArchive = OPEN.url;
// The one document that may manage this browser's app library (host.*: list,
// open, export and delete the OTHER apps stored here). It is whatever the
// operator put on this page: the bundled launcher on a bare visit, the
// server's --open default, or a hosted site's own archive. Never a document
// a link can name (?open=, ?app=), and never decided by anything inside the
// archive itself, since any app can claim to be a launcher.
export const IS_LAUNCHER = OPEN.kind === "launcher" || (OPEN.kind === "site" && OPEN.via !== "param");
// The stored document this page owns, if any (see tabs.js).
export const TAB_KEY = OPEN.kind === "app" ? "app:" + OPEN.id : OPEN.kind === "site" ? "site:" + OPEN.url : null;

// The open document's name, as app.info and later renames report it.
export let lastInfoName = "";
export function appName() { return lastInfoName; }
export function setAppName(name) { lastInfoName = name; }

// Our own navigation (a document switch, a reload button), made after
// everything was saved: the "leave this page?" prompt stays out of it.
export let leaving = false;
export function markLeaving() { leaving = true; }

// A boot that failed because the server now has another build.
export function updatedError() {
  return Object.assign(new Error("A new version of uApp was deployed while this page was loading."), { updated: true });
}

// ---- tiny helpers -----------------------------------------------------------

export const errText = (e) => String((e && e.message) || e);
export const mb = (n) => (n / 1048576).toFixed(1) + " MB";

export function b64FromBytes(bytes) {
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}
export function bytesFromB64(b64) {
  return Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
}
