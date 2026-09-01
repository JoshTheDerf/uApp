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
    fetch: async (u) => new Response("net:" + u, { status: 200, headers: { "content-type": "text/plain" } }),
  };
  vm.createContext(ctx);
  vm.runInContext(readFileSync(new URL("../web/sw.js", import.meta.url), "utf8"), ctx);
  return { handlers, clients, self_ };
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

// ---- 6. cross-origin is never touched -----------------------------------
console.log("\ncross-origin:");
{
  const sw = load();
  let claimed = false;
  const ev = { request: { url: "https://cdn.example.com/x.js", method: "GET", mode: "no-cors", destination: "script", headers: new Headers(), credentials: "omit" },
               clientId: "", resultingClientId: "", respondWith: () => { claimed = true; } };
  for (const f of sw.handlers.fetch) f(ev);
  ok(!claimed, "hands back another origin entirely");
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
