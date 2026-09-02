// web/sw.js routing decisions, without a browser.
//
// The worker's scope has to be "/" (that is the only place /sw.js can be
// registered from to control /app/*), but the origin is usually NOT all the
// uapp's: thederf.com also serves /keenet, /winston/, /photobooth/ from other
// backends behind the same proxy. Anything the archive does not serve has to
// be handed back to the browser — NOT answered by respondWith() — because
// only the native navigation pipeline follows redirects correctly. Re-issuing
// a navigation through fetch() enforces CORS across the redirect, and
// /keenet's 301 to keenet.thederf.com (no ACAO) died as
// "TypeError: Failed to fetch".
//
// So what is asserted here is precisely WHICH requests the worker claims.
import { readFileSync } from "node:fs";
import vm from "node:vm";

let passed = 0, failed = 0;
const ok = (c, n, x = "") => { c ? (passed++, console.log(`  ✓ ${n}`)) : (failed++, console.log(`  ✗ FAIL: ${n} ${x}`)); };

// ---- a minimal ServiceWorkerGlobalScope ----------------------------------
function load(scope = "/") {
  const handlers = {};
  const calls = [];
  const clients = new Map(); // id -> {id, url, frameType}
  const self_ = {
    addEventListener: (t, f) => { (handlers[t] ||= []).push(f); },
    skipWaiting: () => {},
    registration: { scope: "https://thederf.com" + scope },
    clients: {
      get: async (id) => clients.get(id),
      // postMessage is a no-op: no fake shell answers, so askShell times out
      // and the caller sees a definite miss — which is the case under test.
      matchAll: async () => [...clients.values()].map((c) => ({ ...c, postMessage() {} })),
      claim: async () => {},
    },
  };
  const ctx = {
    self: self_, location: { origin: "https://thederf.com" },
    URL, Response, Headers, Request, setTimeout, clearTimeout, console, Map, Set, Promise,
    btoa, atob, Uint8Array, Date, isNaN, parseInt, JSON, String,
    fetch: async (u, init) => {
      const url = typeof u === "string" ? u : (u && u.url) || String(u);
      // fetch(request) vs fetch(url, init): the first preserves the request's
      // own mode and redirect mode, which is the whole point for navigations.
      calls.push({ url, init: init || null, fromRequest: typeof u !== "string" });
      return new Response("net:" + url, { status: 200, headers: { "content-type": "text/plain" } });
    },
  };
  vm.createContext(ctx);
  vm.runInContext(readFileSync(new URL("../web/sw.js", import.meta.url), "utf8"), ctx);
  return { handlers, clients, self_, calls };
}

// Dispatch a synthetic fetch event; report whether the worker CLAIMED it.
function dispatch(sw, { path, mode = "no-cors", destination = "", clientId = "", resultingClientId = "", method = "GET" }) {
  let claimed = false, promise = null;
  const ev = {
    request: { url: "https://thederf.com" + path, method, mode, destination, headers: new Headers(), credentials: "include", referrer: "" },
    clientId, resultingClientId,
    respondWith: (p) => { claimed = true; promise = p; },
  };
  for (const f of sw.handlers.fetch) f(ev);
  return { claimed, promise };
}
const nav = (path, extra = {}) => ({ path, mode: "navigate", destination: "document", ...extra });

// ---- 1. navigations ------------------------------------------------------
console.log("navigations:");
{
  const sw = load();
  // Handed back to the browser: not ours, so the native pipeline runs.
  for (const p of ["/keenet", "/guido", "/winston", "/winston/login", "/photobooth/", "/sqlite-rls/", "/litebase/", "/uapp/demo/"]) {
    ok(!dispatch(sw, nav(p)).claimed, `hands back ${p}`);
  }
  // Claimed: the shell's own entry document (a deployment that sends no COI
  // headers of its own relies on the worker stamping them).
  for (const p of ["/", "/index.html", "/scratch.html"]) {
    ok(dispatch(sw, nav(p)).claimed, `claims ${p}`);
  }
  // A hosted site's own pages are served by the uapp SERVER (which injects
  // chrome.js and sends its own COI headers), so handing them back is right —
  // and it is what makes /admin and /posts/x behave like real URLs.
  for (const p of ["/admin", "/posts/hello"]) {
    ok(!dispatch(sw, nav(p)).claimed, `hands back the site's own page ${p}`);
  }
  // An app-frame navigation is still ours: that frame shows the in-browser copy.
  ok(dispatch(sw, { path: "/admin", mode: "navigate", destination: "iframe" }).claimed,
     "claims an iframe navigation (the editor's frame)");
  ok(dispatch(sw, nav("/app/index.html")).claimed, "claims /app/* even at top level");
}

// ---- 2. subresources of a page that is not ours --------------------------
console.log("\nsubresources:");
{
  const sw = load();
  // The navigation that created the client was handed back, which records the
  // client — so its subresources are recognised synchronously and left alone.
  dispatch(sw, nav("/photobooth/", { resultingClientId: "c-foreign" }));
  for (const p of ["/photobooth/cat-printer.js", "/photobooth/style.css", "/vendor/thing.js", "/api/whatever"]) {
    ok(!dispatch(sw, { path: p, clientId: "c-foreign" }).claimed,
       `hands back ${p} from a foreign page`);
  }
  // The shell's own client keeps archive-first resolution for root-absolute
  // refs — that is the whole point of the worker.
  sw.clients.set("c-frame", { id: "c-frame", url: "https://thederf.com/app/index.html", frameType: "nested" });
  ok(dispatch(sw, { path: "/js/build.js", clientId: "c-frame" }).claimed,
     "claims /js/build.js from the app frame");
  ok(dispatch(sw, { path: "/app/posts/x.html", clientId: "c-frame" }).claimed,
     "claims /app/* from the app frame");
}

// ---- 3. the shell announcing itself -------------------------------------
console.log("\nshell announcement:");
{
  const sw = load();
  // A hosted site's shell page sits at an arbitrary path, so the path rule
  // alone would call it foreign. boot.js announces; the worker must believe it.
  dispatch(sw, nav("/admin", { resultingClientId: "c-shell" }));
  ok(!dispatch(sw, { path: "/js/build.js", clientId: "c-shell" }).claimed,
     "before the announcement, /admin's client is treated as foreign");
  for (const f of sw.handlers.message) f({ data: { shellClient: true }, source: { id: "c-shell" } });
  ok(dispatch(sw, { path: "/js/build.js", clientId: "c-shell" }).claimed,
     "after {shellClient}, its requests are claimed again");
  // Any reply also proves it is the shell (a worker restart loses the set).
  const sw2 = load();
  dispatch(sw2, nav("/admin", { resultingClientId: "c-s2" }));
  for (const f of sw2.handlers.message) f({ data: { swAck: true, id: 999 }, source: { id: "c-s2" } });
  ok(dispatch(sw2, { path: "/js/build.js", clientId: "c-s2" }).claimed,
     "a swAck re-registers the shell after a worker restart");
}

// ---- 4. a subpath deployment (the demo at /uapp/demo/) ------------------
console.log("\nsubpath scope (/uapp/demo/):");
{
  const sw = load("/uapp/demo/");
  ok(dispatch(sw, nav("/uapp/demo/")).claimed, "claims its own root");
  ok(dispatch(sw, nav("/uapp/demo/index.html")).claimed, "claims its own index.html");
  ok(!dispatch(sw, nav("/keenet")).claimed, "hands back /keenet");
  ok(!dispatch(sw, nav("/")).claimed, "hands back the origin root (not its scope)");
}

// ---- 5. an iframe navigation that misses the archive --------------------
console.log("\niframe navigation that misses the archive:");
{
  const sw = load();
  // It has to be claimed: only the archive knows whether /winston/ is one of
  // the site's own pages. What matters is what happens on a MISS.
  const { claimed, promise } = dispatch(sw, { path: "/winston/", mode: "navigate", destination: "iframe" });
  ok(claimed, "claimed, to check the archive first");
  const body = await (await promise).text();   // no shell answers -> askShell times out -> miss
  ok(body.startsWith("net:"), "a miss is served from the network, in place", body.slice(0, 40));
  ok(!body.includes("top.location"), "no stub that navigates the top window away");
  // That stub is what made embedding impossible: an <iframe src="/winston/">
  // used to take the whole tab with it.
}

// ---- 6. how the worker re-issues a request to the network ---------------
//
// netFetch used to build a fresh fetch() with no mode and the default
// redirect:"follow". Both are wrong the moment a same-origin path redirects
// off-origin: CORS is enforced across a redirect, and thederf.com/keenet is a
// 301 to keenet.thederf.com with no Access-Control-Allow-Origin. That is the
// "blocked by CORS policy ... Response to preflight request doesn't pass
// access control check" the console filled up with, followed by a 503.
console.log("\nre-issuing to the network:");
{
  const sw = load();
  // An iframe navigation that misses: forwarded as the request itself, so the
  // browser follows the 3xx (an opaque redirect) rather than fetch() failing.
  const a = load();
  await dispatch(a, { path: "/keenet", mode: "navigate", destination: "iframe" }).promise;
  const c = a.calls[a.calls.length - 1];
  ok(c && c.fromRequest, "an iframe miss forwards the request object itself");
  ok(c && !c.url.includes("?_="), "and does not rebuild the URL with a cache-buster", c && c.url);

  // A top-level navigation the worker does claim (its own root).
  await dispatch(sw, nav("/")).promise;
  const navCall = sw.calls.find((x) => x.init && x.init.redirect);
  ok(!!navCall && navCall.init.redirect === "manual",
     "a claimed navigation uses redirect:\"manual\"", navCall ? JSON.stringify(navCall.init.redirect) : "none");

  // A no-cors subresource must not be silently upgraded to cors.
  const b = load();
  await dispatch(b, { path: "/shell/main.js", mode: "no-cors", destination: "script", clientId: "" }).promise;
  const sub = b.calls[b.calls.length - 1];
  ok(sub && sub.init && sub.init.mode === "no-cors", "a no-cors subresource keeps mode no-cors",
     sub && sub.init ? String(sub.init.mode) : "none");
}

// ---- 7. cross-origin is never touched -----------------------------------
console.log("\ncross-origin:");
{
  const sw = load();
  let claimed = false;
  const ev = { request: { url: "https://cdn.example.com/x.js", method: "GET", mode: "no-cors", destination: "script", headers: new Headers(), credentials: "omit" },
               clientId: "", resultingClientId: "", respondWith: () => { claimed = true; } };
  for (const f of sw.handlers.fetch) f(ev);
  ok(!claimed, "hands back another origin entirely");
}

// ---- 8. several shells on one origin: instance routing -------------------
// A desktop page with two apps open in two frames (or two tabs of the demo)
// shares one worker. boot.js puts its instance id into every frame URL
// ("<scope>i/<inst>/app/..."); the worker strips it and asks exactly that
// shell — never both, since the faster (wrong) app would otherwise win.
console.log("\ninstance routing:");
{
  const sw = load("/uapp/demo/");
  const posted = [];
  const mkClient = (id, url, frameType) => ({ id, url, frameType, postMessage: (m) => posted.push({ to: id, m }) });
  // matchAll spreads clients and swaps in a no-op postMessage; give the
  // broadcast path a recorder too.
  sw.self_.clients.matchAll = async () => [...sw.clients.values()].map((c) => ({ ...c, postMessage: (m) => posted.push({ to: c.id, m }) }));
  sw.clients.set("shell-a", mkClient("shell-a", "https://thederf.com/uapp/demo/?open=/uapp/apps/a.uapp", "nested"));
  sw.clients.set("shell-b", mkClient("shell-b", "https://thederf.com/uapp/demo/?open=/uapp/apps/b.uapp", "nested"));
  sw.clients.set("frame-a", mkClient("frame-a", "https://thederf.com/uapp/demo/i/aaaa/app/index.html", "nested"));
  sw.clients.set("frame-b", mkClient("frame-b", "https://thederf.com/uapp/demo/i/bbbb/app/index.html", "nested"));
  for (const f of sw.handlers.message) f({ data: { shellClient: true, inst: "aaaa" }, source: sw.clients.get("shell-a") });
  for (const f of sw.handlers.message) f({ data: { shellClient: true, inst: "bbbb" }, source: sw.clients.get("shell-b") });

  // The instance segment is stripped, and the ask goes to that shell alone.
  let r = dispatch(sw, { path: "/uapp/demo/i/aaaa/app/js/app.js", clientId: "frame-a" });
  ok(r.claimed, "claims a frame's archive fetch under its instance prefix");
  await new Promise((res) => setTimeout(res, 20));
  let asks = posted.filter((p) => p.m.swRequest);
  ok(asks.length === 1 && asks[0].to === "shell-a", "asked shell A only", JSON.stringify(asks.map((a) => a.to)));
  ok(asks[0].m.path === "/app/js/app.js", "the shell sees the plain archive path", asks[0].m.path);
  ok(asks[0].m.inst === "aaaa", "the ask names the instance", asks[0].m.inst);
  posted.length = 0;

  r = dispatch(sw, { path: "/uapp/demo/i/bbbb/download.uapp", clientId: "shell-b" });
  ok(r.claimed, "claims a download under the other instance");
  await new Promise((res) => setTimeout(res, 20));
  asks = posted.filter((p) => p.m.swRequest);
  ok(asks.length === 1 && asks[0].to === "shell-b", "asked shell B only", JSON.stringify(asks.map((a) => a.to)));
  posted.length = 0;

  // A page's root-absolute reference carries no segment: attributed through
  // the client that made it (its frame was created under /i/bbbb/).
  dispatch(sw, nav("/uapp/demo/i/bbbb/app/index.html", { destination: "iframe", clientId: "shell-b", resultingClientId: "frame-b2" }));
  await new Promise((res) => setTimeout(res, 20));
  posted.length = 0;
  sw.clients.set("frame-b2", mkClient("frame-b2", "https://thederf.com/uapp/demo/i/bbbb/app/index.html", "nested"));
  r = dispatch(sw, { path: "/uapp/demo/vendor/lib.js", clientId: "frame-b2" });
  ok(r.claimed, "claims a segment-less archive path from a known frame");
  await new Promise((res) => setTimeout(res, 20));
  asks = posted.filter((p) => p.m.swRequest);
  ok(asks.length === 1 && asks[0].to === "shell-b", "routed by the requesting frame's instance", JSON.stringify(asks.map((a) => a.to)));
  posted.length = 0;

  // An instance nobody announced (a restarted worker): everyone is asked, and
  // the ask still names the instance so the shells can filter themselves.
  r = dispatch(sw, { path: "/uapp/demo/i/zzzz/app/x.css", clientId: "" });
  await new Promise((res) => setTimeout(res, 20));
  asks = posted.filter((p) => p.m.swRequest);
  ok(asks.length === sw.clients.size && asks.every((a) => a.m.inst === "zzzz"), "unknown instance: broadcast, tagged", `${asks.length} asks`);
  posted.length = 0;

  // Shell pages themselves: a client at /i/<inst>/app/ is recognised as ours.
  r = dispatch(sw, { path: "/uapp/demo/i/aaaa/app/style.css", clientId: "frame-a" });
  ok(r.claimed, "a frame under an instance prefix is a shell page");
  // Outside the scope entirely is still not ours.
  ok(!dispatch(sw, { path: "/i/aaaa/app/x.js", clientId: "frame-a" }).claimed, "an instance path outside the scope is not ours");

  // Root-absolute references from OUR frames: "/uapp.js" and "/js/app.js" in
  // an app page mean the bundle and the archive, as at a root deployment —
  // not the origin's root, which under a subpath is somebody else's server.
  // frame-b2 was created by a navigation this worker saw; frame-a was not
  // (a worker restart) and an unknown client stays untouched.
  r = dispatch(sw, { path: "/uapp.js", clientId: "frame-b2" });
  ok(r.claimed, "claims a root-absolute bundle reference from a known frame");
  await new Promise((res) => setTimeout(res, 20));
  const bundleFetch = sw.calls.at(-1);
  ok(bundleFetch && bundleFetch.url === "https://thederf.com/uapp/demo/uapp.js", "and fetches the bundle file under the scope", bundleFetch && bundleFetch.url);
  posted.length = 0;
  r = dispatch(sw, { path: "/js/app.js", clientId: "frame-b2" });
  ok(r.claimed, "claims a root-absolute archive reference from a known frame");
  await new Promise((res) => setTimeout(res, 20));
  asks = posted.filter((p) => p.m.swRequest);
  ok(asks.length === 1 && asks[0].to === "shell-b" && asks[0].m.path === "/js/app.js", "asks that frame's shell for it", JSON.stringify(asks.map((a) => [a.to, a.m.path])));
  ok(!dispatch(sw, { path: "/js/app.js", clientId: "stranger" }).claimed, "a root-absolute path from an unknown client is not ours");
  ok(!dispatch(sw, nav("/keenet", { destination: "iframe", clientId: "frame-b2" })).claimed, "a navigation out of the scope from our frame is still a link out");
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
