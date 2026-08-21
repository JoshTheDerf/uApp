/* uapp browser build — service worker.
 * Two jobs:
 *  1. Serve archive-backed URLs (/app/*, /data/*, vendored libs, downloads,
 *     uploads) by asking the shell page, which asks the wasm worker — so app
 *     iframes get REAL same-origin URLs and relative references just work.
 *  2. Stamp COOP/COEP on every response so the page is cross-origin isolated
 *     and SharedArrayBuffer (the run_js/actions bridge) is available.
 */

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

let seq = 1;
const waiting = new Map();

self.addEventListener("message", (ev) => {
  const m = ev.data;
  if (m && m.swReply && waiting.has(m.id)) {
    waiting.get(m.id)(m);
    waiting.delete(m.id);
  }
});

// Ask every window client; only the top shell (boot.js) answers.
async function askShell(req) {
  const cs = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  const id = seq++;
  const p = new Promise((res) => {
    waiting.set(id, res);
    setTimeout(() => { if (waiting.has(id)) { waiting.delete(id); res(null); } }, 30000);
  });
  for (const c of cs) c.postMessage({ swRequest: true, id, ...req });
  return p;
}

function withCoi(resp) {
  const h = new Headers(resp.headers);
  // credentialless (not require-corp): plain <script src="https://cdn..."> in
  // generated apps keeps working. Browsers without credentialless just don't
  // become isolated — everything still runs, minus the run_js bridge.
  h.set("Cross-Origin-Embedder-Policy", "credentialless");
  h.set("Cross-Origin-Opener-Policy", "same-origin");
  h.set("Cross-Origin-Resource-Policy", "cross-origin");
  return new Response(resp.body, { status: resp.status, statusText: resp.statusText, headers: h });
}

function b64FromBuffer(buf) {
  const bytes = new Uint8Array(buf);
  let bin = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

async function archiveResponse(req, url) {
  let bodyB64 = null;
  if (req.method === "POST") bodyB64 = b64FromBuffer(await req.arrayBuffer());
  const r = await askShell({
    path: url.pathname + url.search,
    method: req.method,
    bodyB64,
  });
  if (!r) return withCoi(new Response("uapp shell not ready", { status: 503 }));
  if (r.error) return withCoi(new Response(r.error, { status: r.status || 404 }));
  const bytes = Uint8Array.from(atob(r.bodyB64 || ""), (c) => c.charCodeAt(0));
  const headers = {
    "content-type": r.contentType || "application/octet-stream",
    "cache-control": "no-store",
  };
  if (r.disposition) headers["content-disposition"] = r.disposition;
  return withCoi(new Response(bytes, { status: r.status || 200, headers }));
}

const ARCHIVE_PREFIX = /^\/(app|data|vendor|scratch)(\/|$)/;
const ARCHIVE_EXACT = new Set(["/download.uapp", "/template.uapp", "/upload"]);

// The demo may be hosted under a subpath (e.g. /uapp/demo/). The shell keeps
// its native root-absolute URLs ("/shell/main.js", "/app/..."): fetches from
// controlled pages all pass through here, so we normalize any path to be
// relative to the registration scope and serve from there.
const SCOPE = new URL(self.registration.scope).pathname; // e.g. "/uapp/demo/"

self.addEventListener("fetch", (ev) => {
  const url = new URL(ev.request.url);
  if (url.origin !== location.origin) return; // cross-origin: browser handles it
  // Root-form path relative to the scope: "/uapp/demo/app/x" and "/app/x"
  // both become "/app/x".
  const p = url.pathname.startsWith(SCOPE)
    ? "/" + url.pathname.slice(SCOPE.length)
    : url.pathname;
  if (ARCHIVE_PREFIX.test(p) || ARCHIVE_EXACT.has(p)) {
    const u = new URL(url);
    u.pathname = p;
    ev.respondWith(archiveResponse(ev.request, u));
    return;
  }
  // Static shell files, always under the scope. On a 404, fall back to the
  // archive root (apps may use root-absolute references like /styles.css,
  // same as the native server).
  ev.respondWith((async () => {
    try {
      const staticUrl = new URL(SCOPE.slice(1) + p.slice(1) + url.search, url.origin);
      const resp = await fetch(p === "/" ? ev.request : staticUrl, { method: "GET" });
      if (resp.status === 404 && ev.request.method === "GET" && p !== "/" && !p.startsWith("/shell/")) {
        const u = new URL(url);
        u.pathname = p;
        return archiveResponse(ev.request, u);
      }
      return withCoi(resp);
    } catch (e) {
      return new Response("offline: " + e, { status: 503 });
    }
  })());
});
