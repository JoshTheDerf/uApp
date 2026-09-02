// A stand-in for the production Caddy: one origin, three backends.
//   /uapp/demo/*  -> uapp-server serve --chrome dist-web (prefix stripped)
//   /uapp/apps/*  -> uapp-library                        (prefix stripped)
//   /desk.html    -> a fake "desktop" page with two app windows (iframes)
//   /site/*       -> optional: a uapp-server serve site.uapp (prefix stripped)
import http from "node:http";

const [demoPort, libPort, listenPort] = process.argv.slice(2).map(Number);
const DESK = `<!doctype html><html><head><meta charset="utf-8"><title>desk</title>
<meta http-equiv="Cross-Origin-Opener-Policy" content="same-origin">
<style>body{margin:0;display:flex;gap:8px;height:100vh}iframe{flex:1;border:1px solid #888;height:100%}</style></head>
<body>
<iframe id="w1" src="/uapp/demo/?open=/uapp/apps/kanban-board.uapp" sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-downloads"></iframe>
<iframe id="w2" src="/uapp/demo/?open=/uapp/apps/habit-tracker.uapp" sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-downloads"></iframe>
</body></html>`;

function forward(req, res, port, path) {
  const opts = { host: "127.0.0.1", port, method: req.method, path, headers: { ...req.headers, host: "127.0.0.1:" + port } };
  const up = http.request(opts, (r) => { res.writeHead(r.statusCode, r.headers); r.pipe(res); });
  up.on("error", (e) => { res.writeHead(502); res.end("upstream: " + e.message); });
  req.pipe(up);
}

http.createServer((req, res) => {
  const u = new URL(req.url, "http://x");
  if (u.pathname === "/desk.html") {
    res.writeHead(200, { "content-type": "text/html", "cross-origin-opener-policy": "same-origin", "cross-origin-embedder-policy": "credentialless" });
    return res.end(DESK);
  }
  if (u.pathname === "/uapp/demo") { res.writeHead(301, { location: "/uapp/demo/" }); return res.end(); }
  if (u.pathname.startsWith("/uapp/demo/")) return forward(req, res, demoPort, u.pathname.slice("/uapp/demo".length) + u.search);
  if (u.pathname.startsWith("/uapp/apps/")) return forward(req, res, libPort, u.pathname.slice("/uapp/apps".length) + u.search);
  res.writeHead(404); res.end("proxy: not here");
}).listen(listenPort, "127.0.0.1", () => console.log("proxy on", listenPort));
