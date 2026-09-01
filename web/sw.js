/* uapp browser build — service worker.
 * Three jobs:
 *  1. Serve archive-backed URLs (/app/*, /data/*, vendored libs, downloads,
 *     uploads) by asking the shell page, which asks the wasm worker — so app
 *     iframes get REAL same-origin URLs and relative references just work.
 *  2. Stamp COOP/COEP on the shell's own responses so the page is cross-origin
 *     isolated and SharedArrayBuffer (the run_js/actions bridge) is available.
 *  3. Stay COMPLETELY out of the way of everything else on the origin. The
 *     scope has to be "/" (that is the only place /sw.js can control /app/*
 *     from), but the origin is usually NOT all ours: thederf.com also serves
 *     /winston/, /photobooth/, /keenet, ... from other backends behind the
 *     same proxy. For those, not responding at all is the only correct
 *     behaviour — see shellPath / clientIsShell below.
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
  // boot.js announcing "this client is the shell". The only way to recognise
  // a hosted site's own pages: they live at arbitrary paths (thederf.com/,
  // /admin, /posts/x are all shell pages), so no path rule can spot them.
  if (m.shellClient) { if (ev.source && ev.source.id) { shellClients.add(ev.source.id); clientKind.delete(ev.source.id); } return; }
  // Any reply at all proves the sender is the shell — free re-registration
  // after a worker restart, which loses shellClients (boot.js only re-announces
  // on controllerchange, and a restart does not fire one).
  if ((m.swAck || m.swReply) && ev.source && ev.source.id && !shellClients.has(ev.source.id)) {
    shellClients.add(ev.source.id);
    clientKind.delete(ev.source.id);
  }
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
  const init = { method: req.method === "HEAD" ? "HEAD" : "GET", cache: bust ? "no-store" : "no-cache", headers: req.headers, credentials: req.credentials };
  // Keep the request's own mode. This used to default to "cors" for
  // everything, which silently upgraded a no-cors subresource and then FAILED
  // it the moment a same-origin path redirected off-origin — CORS is enforced
  // across a redirect, and thederf.com/keenet is a 301 to keenet.thederf.com,
  // which sends no Access-Control-Allow-Origin. A navigation cannot carry its
  // mode into fetch() at all, so it gets redirect:"manual": a 3xx comes back
  // as an opaque redirect that the BROWSER performs, which is the only way a
  // cross-origin redirect survives being re-issued here.
  if (req.mode === "navigate") init.redirect = "manual";
  else if (req.mode) init.mode = req.mode;
  return fetch(u, init);
}

// `cacheable`: the shell's own versioned bundle may be kept and revalidated
// (the server ETags it, so a warm load is a run of 304s instead of ~1.5 MB of
// wasm again); everything archive-served stays no-store.
function withCoi(resp, cacheable = false) {
  // An opaque redirect (a navigation with redirect:"manual") has status 0 and
  // a null body: it cannot be rebuilt, and it has to reach the browser intact
  // so the browser follows the redirect itself. Same for an opaque no-cors
  // response. Rebuilding either throws.
  if (resp.type === "opaqueredirect" || resp.type === "opaque" || resp.status === 0) return resp;
  const h = new Headers(resp.headers);
  h.set("Cache-Control", cacheable ? "no-cache" : "no-store");
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
const BUNDLE = new Set(["/boot.js", "/sw.js", "/worker.js", "/uapp.js", "/uapp_glue.js", "/icons.js", "/shell.css", "/scratch.html",
  "/chrome.js", "/site-chrome.js", "/uapp_wasm.js", "/uapp_wasm_bg.wasm", "/index.html", "/launcher.uapp", "/site.uapp", "/health"]);

// `probe`: instead of a 404/passthrough response, return null when the
// archive definitely has no such file (or the shell says passthrough) so the
// caller can try the network — and `undefined` when NO shell answered at
// all, which is not a miss: the caller must not conclude the page doesn't
// exist from a busy or reloading tab.
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
  if (probe && !r) return undefined;
  if (probe && (r.passthrough || r.notFound)) return null;
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
  // Range requests: a <video>/<audio> from the archive must support seeking,
  // and Safari won't even start playback without a 206. We have the whole
  // body in memory, so honour a single byte range.
  const range = (r.status || 200) === 200 && req.headers.get("range");
  const m = range && /^bytes=(\d*)-(\d*)$/.exec(range.trim());
  if (m && (m[1] || m[2])) {
    const total = bytes.length;
    let start = m[1] ? parseInt(m[1], 10) : total - parseInt(m[2], 10);
    let end = m[1] && m[2] ? parseInt(m[2], 10) : total - 1;
    if (!isNaN(start) && !isNaN(end) && start <= end && start < total) {
      end = Math.min(end, total - 1);
      headers["content-range"] = `bytes ${start}-${end}/${total}`;
      headers["accept-ranges"] = "bytes";
      return withCoi(new Response(bytes.subarray(start, end + 1), { status: 206, headers }));
    }
    return withCoi(new Response(null, { status: 416, headers: { "content-range": `bytes */${total}` } }));
  }
  headers["accept-ranges"] = "bytes";
  return withCoi(new Response(bytes, { status: r.status || 200, headers }));
}

const ARCHIVE_PREFIX = /^\/(app|data|vendor|scratch)(\/|$)/;
const ARCHIVE_EXACT = new Set(["/download.uapp", "/template.uapp", "/upload"]);

// The demo may be hosted under a subpath (e.g. /uapp/demo/). The shell keeps
// its native root-absolute URLs ("/shell/main.js", "/app/..."): fetches from
// controlled pages all pass through here, so we normalize any path to be
// relative to the registration scope and serve from there.
const SCOPE = new URL(self.registration.scope).pathname; // e.g. "/uapp/demo/"

// Root-form path relative to the scope: "/uapp/demo/app/x" and "/app/x" both
// become "/app/x". A path OUTSIDE the scope is null — not ours at all. (A
// worker sees every same-origin request its clients make, scope or not: scope
// decides which CLIENTS it controls, not which URLs it is asked about. With
// scope "/" nothing is outside it and paths are judged by shellPath alone.)
const scopeRelative = (pathname) =>
  pathname.startsWith(SCOPE) ? "/" + pathname.slice(SCOPE.length) : null;

// The paths the shell actually owns: its own documents, its bundle, and the
// archive routes. Everything else on this origin is somebody else's.
function shellPath(p) {
  return p === "/" || BUNDLE.has(p) || ARCHIVE_EXACT.has(p) || ARCHIVE_PREFIX.test(p) || p.startsWith("/shell/");
}

// Is the client making this request one of the shell's own pages?
//
// Getting this wrong is expensive: a page we do not own has no in-browser
// archive behind it, so probing for one costs it the full askShell timeout on
// EVERY request — measured at ~1.6 s per subresource on thederf.com/photobooth/
// against a shell that is not even loaded — and withCoi would stamp our
// COOP/COEP onto its responses.
//
// Three sources, cheapest first: the shell announces itself ({shellClient}
// from boot.js); a navigation we passed through records the client it creates
// as foreign, so its subresources are known SYNCHRONOUSLY and can be left
// entirely alone; otherwise clients.get() (a local lookup, not a message
// round-trip) and the path rule. Service workers are restarted freely and
// this state is not durable, hence the fallback.
const shellClients = new Set();
const clientKind = new Map(); // client id -> true (shell) | false (someone else's page)

function rememberClient(id, isShell) {
  if (!id) return;
  clientKind.set(id, isShell);
  if (clientKind.size > 128) clientKind.delete(clientKind.keys().next().value);
}

async function clientIsShell(clientId) {
  if (!clientId) return true;                       // nothing to attribute it to: behave as before
  if (shellClients.has(clientId)) return true;
  const known = clientKind.get(clientId);
  if (known !== undefined) return known;
  let ours = true;                                  // conservative: if we cannot tell, act as before
  try {
    const c = await self.clients.get(clientId);
    if (c) { const rel = scopeRelative(new URL(c.url).pathname); ours = rel !== null && shellPath(rel); }
  } catch { /* keep the default */ }
  rememberClient(clientId, ours);
  return ours;
}

self.addEventListener("fetch", (ev) => {
  const url = new URL(ev.request.url);
  if (url.origin !== location.origin) return; // cross-origin: browser handles it
  const p = scopeRelative(url.pathname);
  if (p === null) return; // outside our scope: whatever else is hosted here
  // A client we already know is not ours: no respondWith at all, so the
  // browser fetches exactly as it would with no worker installed. This is the
  // synchronous fast path — the only one that can hand a request back.
  if (ev.clientId && clientKind.get(ev.clientId) === false && !shellClients.has(ev.clientId)) return;
  // Only the archive's own POST target (/upload) is ours. Every other write
  // (PUT /site.uapp publishing the local copy, an app's own POST to some
  // server API) goes to the network untouched — netFetch would turn it into
  // a bodiless GET.
  if (ev.request.method !== "GET" && ev.request.method !== "HEAD" && !ARCHIVE_EXACT.has(p)) return;
  // A navigation INSIDE the app frame ("/", "/admin", "/posts/x/" — apps and
  // generated sites link root-absolute) stays in the archive: the frame is
  // the in-browser copy, not the server's site. Top-level navigations are
  // untouched (the server's page, with its chrome).
  if (ev.request.mode === "navigate" && ev.request.destination === "iframe"
      && !ARCHIVE_PREFIX.test(p) && !ARCHIVE_EXACT.has(p) && !p.startsWith("/shell/") && !BUNDLE.has(p)) {
    const u = new URL(url);
    u.pathname = "/app" + (p === "/" ? "/" : p);
    ev.respondWith((async () => {
      const r = await archiveResponse(ev.request, u, ev.clientId, true);
      if (r) return r;
      // Not a page of this site's archive: same host, but something else
      // lives there — one of the other apps behind the same Caddy
      // (/winston/, /photobooth/, /sqlite-rls/), or a path the site never
      // built. Serve it from the network IN PLACE.
      //
      // This used to answer with a stub that sent the TOP window to the URL.
      // That made embedding any of those apps impossible: putting /winston/
      // in an <iframe> navigated the whole tab away instead of filling the
      // frame. A link that means to leave the editor still says so with
      // target="_top", which the shell honours (see shell/main.js).
      //
      // `r === undefined` (no shell answered — busy, mid-reload, or a reader
      // with #noedit and no shell at all) lands here too: the server holds
      // the published copy of every real page, which beats the 503 "the
      // editor did not answer" stub that an embedded app used to show.
      //
      // Forwarded as the navigation it IS (`fetch(ev.request)`), not rebuilt
      // by netFetch: a navigation request carries redirect:"manual", so a 3xx
      // comes back as an opaque redirect that the browser performs itself.
      // Rebuilding it as a plain cors fetch is what made `<iframe
      // src="/keenet">` fail with "blocked by CORS policy ... No
      // 'Access-Control-Allow-Origin'" and then a 503 — the 301 to
      // keenet.thederf.com cannot be followed by fetch.
      try {
        const resp = await fetch(ev.request);
        return resp.type === "opaqueredirect" || resp.status === 0 ? resp : withCoi(resp);
      } catch (e) { return new Response("offline: " + e, { status: 503 }); }
    })());
    return;
  }
  // A navigation to a path the shell does not own — the reader leaving for
  // another app on the same host (/keenet, /winston/, /photobooth/). Return
  // without responding: only the browser's own navigation pipeline gets this
  // right. Re-issuing it through fetch() cannot, because fetch enforces CORS
  // across a redirect — thederf.com/keenet is a 301 to keenet.thederf.com,
  // which sends no Access-Control-Allow-Origin, so the fetch threw and the
  // reader got our "offline: TypeError: Failed to fetch" 503 instead of the
  // page. Same-origin redirects "worked" but left the address bar on the
  // pre-redirect URL (/winston, not /winston/), and withCoi put our
  // COOP/COEP on an unrelated app's document.
  //
  // The shell's own document still goes through the branches below, so a
  // deployment serving no COI headers of its own is still isolated by us.
  if (ev.request.mode === "navigate" && !shellPath(p)) {
    // Its subresources are ours to ignore too, and now we can say so without
    // an async lookup.
    rememberClient(ev.resultingClientId, false);
    return;
  }
  if (ev.request.mode === "navigate") rememberClient(ev.resultingClientId, true);
  if (ARCHIVE_PREFIX.test(p) || ARCHIVE_EXACT.has(p)) {
    const u = new URL(url);
    u.pathname = p;
    ev.respondWith((async () => {
      // Another app on this origin that happens to use one of our prefixes
      // (its own /vendor/... say): it has no archive behind it.
      if (!(await clientIsShell(ev.clientId))) return netFetch(ev.request, u, false);
      return archiveResponse(ev.request, u, ev.clientId);
    })());
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
      // A page that is not ours, whose navigation this worker never saw (it
      // was loaded before we installed, or we were restarted since). Be the
      // network and nothing else: no archive probe, no COI stamp. The answer
      // is cached, so every later request from it takes the synchronous
      // hand-back at the top of this handler instead.
      if (!(await clientIsShell(ev.clientId))) return netFetch(ev.request, null, false);
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
      const isBundle = p.startsWith("/shell/") || BUNDLE.has(p);
      const bust = p !== "/" && !isBundle;
      const resp = await netFetch(ev.request, p === "/" ? null : staticUrl, bust);
      // (Never for a top-level navigation: that must stay the server's page —
      // its 404 page carries the chrome, which then shows the local copy's
      // version of the page in the frame.)
      if (resp.status === 404 && ev.request.method === "GET" && ev.request.mode !== "navigate" && p !== "/" && !p.startsWith("/shell/")) {
        const u = new URL(url);
        u.pathname = p;
        return archiveResponse(ev.request, u, ev.clientId);
      }
      return withCoi(resp, isBundle);
    } catch (e) {
      return new Response("offline: " + e, { status: 503 });
    }
  })());
});
