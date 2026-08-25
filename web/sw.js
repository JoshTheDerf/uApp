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

// Explicit version handoff (driven by boot.js): the OUTGOING worker gets
// {release} and answers every pending ask with null — a fetch handler still
// waiting on a shell reply would otherwise keep it "active" and the new
// worker stuck in "installed" — and the INCOMING worker gets {skipWaiting}.
function releaseAll() {
  for (const [id, res] of waiting) { res(null); acks.delete(id); }
  waiting.clear();
}

self.addEventListener("message", (ev) => {
  const m = ev.data;
  if (!m) return;
  if (m.release) { releaseAll(); return; }
  if (m.skipWaiting) { self.skipWaiting(); return; }
  if (m.swAck && acks.has(m.id)) { acks.get(m.id)(); acks.delete(m.id); return; }
  if (m.swReply && waiting.has(m.id)) {
    waiting.get(m.id)(m);
    waiting.delete(m.id);
    acks.delete(m.id);
  }
});

// Ask every window client; only the top shell (boot.js) answers. `frameType`
// tells it whether the request came from the top document or a nested frame:
// in hosted-site mode the top document is the server's own page until the
// engine is up, and its requests must go to the network, not wait on us.
async function frameTypeOf(clientId) {
  try { const c = clientId && await self.clients.get(clientId); return c ? c.frameType : ""; } catch { return ""; }
}

// Every ask gets a SHORT wait for a first sign of life. A shell that will
// serve the request acks at once ({swAck}) and then gets the full 30s for the
// body; silence means no shell is loaded (a hosted-site page still parsing —
// its blocking <script src="vendor/…"> would otherwise deadlock on a shell
// that only exists after parsing ends), so the caller falls back to the
// network. Deliberately not keyed on the client's frameType: a document that
// is still being parsed may not resolve through clients.get() yet.
const acks = new Map();
async function askShell(req, frameType) {
  const cs = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  req = { ...req, frameType };
  const id = seq++;
  const p = new Promise((res) => {
    waiting.set(id, res);
    let timer = setTimeout(() => { if (waiting.has(id)) { waiting.delete(id); acks.delete(id); res(null); } }, 1500);
    acks.set(id, () => {
      clearTimeout(timer);
      timer = setTimeout(() => { if (waiting.has(id)) { waiting.delete(id); acks.delete(id); res(null); } }, 30000);
    });
  });
  for (const c of cs) c.postMessage({ swRequest: true, id, ...req });
  return p;
}

// Network fetch as the uapp context does it: never from the HTTP cache, and
// for files that may sit behind a CDN (anything that isn't the shell's own
// versioned bundle) with a cache-busting query so an edge cache can't hand
// back a stale copy either. Inside the editor everything is live — a stale
// build.js or stylesheet is exactly the confusion this avoids.
function netFetch(req, url, bust) {
  let u = url ? new URL(url) : new URL(req.url);
  if (bust) u.searchParams.set("_", Date.now().toString(36));
  return fetch(u, { method: req.method === "HEAD" ? "HEAD" : "GET", cache: "no-store", headers: req.headers, credentials: req.credentials });
}

function withCoi(resp) {
  const h = new Headers(resp.headers);
  h.set("Cache-Control", "no-store");
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

// Shell bundle files: never looked up in an archive.
const BUNDLE = new Set(["/boot.js", "/sw.js", "/worker.js", "/uapp.js", "/uapp_glue.js", "/icons.js", "/shell.css",
  "/chrome.js", "/site-chrome.js", "/uapp_wasm.js", "/uapp_wasm_bg.wasm", "/index.html", "/launcher.uapp", "/site.uapp", "/health"]);

// `probe`: return null (instead of a 404/passthrough response) when the
// archive has no such file or no engine answered, so the caller can try the
// network next.
async function archiveResponse(req, url, clientId, probe) {
  const frameType = await frameTypeOf(clientId);
  let bodyB64 = null;
  if (req.method === "POST") bodyB64 = b64FromBuffer(await req.arrayBuffer());
  const r = await askShell({
    path: url.pathname + url.search,
    method: req.method,
    bodyB64,
  }, frameType);
  // Nobody answered a top document, or the page declined (engine not up yet):
  // either way it is the server's own resource, fetch it from there.
  if (probe && (!r || r.passthrough || r.error)) return null;
  if ((!r && req.method === "GET") || (r && r.passthrough)) {
    try { return withCoi(await netFetch(req, null, !BUNDLE.has(url.pathname))); }
    catch (e) { return new Response("offline: " + e, { status: 503 }); }
  }
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
  // A navigation INSIDE the app frame ("/", "/admin", "/posts/x/" — apps and
  // generated sites link root-absolute) stays in the archive: the frame is
  // the in-browser copy, not the server's site. Top-level navigations are
  // untouched (the server's page, with its chrome).
  if (ev.request.mode === "navigate" && ev.request.destination === "iframe"
      && !ARCHIVE_PREFIX.test(p) && !ARCHIVE_EXACT.has(p) && !p.startsWith("/shell/") && !BUNDLE.has(p)) {
    const u = new URL(url);
    u.pathname = "/app" + (p === "/" ? "/" : p);
    ev.respondWith(archiveResponse(ev.request, u, ev.clientId));
    return;
  }
  if (ARCHIVE_PREFIX.test(p) || ARCHIVE_EXACT.has(p)) {
    const u = new URL(url);
    u.pathname = p;
    ev.respondWith(archiveResponse(ev.request, u, ev.clientId));
    return;
  }
  // Everything else: the ARCHIVE first (once an app is open), the static
  // shell files / server second. Apps reference their own files root-absolute
  // ("/js/build.js", "/styles.css"); those must resolve against the in-browser
  // copy — in hosted-site mode the server (and a CDN in front of it) has the
  // published version, and an edit the visitor just made would otherwise be
  // invisible. The shell's own bundle never lives in an archive, so its
  // lookups miss and fall through; before the engine is up the shell
  // passes straight through (see handleSwRequest in boot.js).
  ev.respondWith((async () => {
    try {
      // Never for a navigation: a top document is the SERVER's page (with
      // its injected chrome) — the shell being navigated away from is still
      // a client here and would hand over the raw archive copy instead.
      if (ev.request.method === "GET" && ev.request.mode !== "navigate" && p !== "/" && !p.startsWith("/shell/") && !BUNDLE.has(p)) {
        const u = new URL(url);
        u.pathname = p;
        const r = await archiveResponse(ev.request, u, ev.clientId, true);
        if (r) return r;
      }
      const staticUrl = new URL(SCOPE.slice(1) + p.slice(1) + url.search, url.origin);
      const bust = p !== "/" && !p.startsWith("/shell/") && !BUNDLE.has(p);
      const resp = await netFetch(ev.request, p === "/" ? null : staticUrl, bust);
      if (resp.status === 404 && ev.request.method === "GET" && p !== "/" && !p.startsWith("/shell/")) {
        const u = new URL(url);
        u.pathname = p;
        return archiveResponse(ev.request, u, ev.clientId);
      }
      return withCoi(resp);
    } catch (e) {
      return new Response("offline: " + e, { status: 503 });
    }
  })());
});
