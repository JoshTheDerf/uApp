/* uapp browser build — pages in the shell's frames talking to the engine:
 * uapp.js in the app frame (and the scratch frame, the file viewer, and
 * whatever they embed) posts RPCs here, and gets replies, events and its
 * connection's messages back. Also the launcher's host.* API and switching
 * documents. */

import { currentAppId } from "./autosave.js?v=__UAPP_BUILD__";
import { BASE, IS_LAUNCHER, b64FromBytes, bytesFromB64, markLeaving } from "./env.js?v=__UAPP_BUILD__";
import { fetchWithProgress, splashHide, splashShow } from "./splash.js?v=__UAPP_BUILD__";
import { appDelete, appRead, appWrite, appsIndex, genId, indexTouch } from "./storage.js?v=__UAPP_BUILD__";
import { appReady, sendRpc } from "./worker-bridge.js?v=__UAPP_BUILD__";

const clientWindows = new Set(); // our frames' windows that spoke to us
// Page connections, as the core sees them (see App::conn_open in app.rs): one
// id per DOCUMENT. A WindowProxy survives its frame's navigation, so a reload
// closes the old id and mints a new one — that is how the core notices a
// fresh page (reload_app) and forgets the old page's actions.
let connSeq = 1;
const connOfWin = new Map(); // win -> conn id
const winOfConn = new Map(); // conn id -> win

// ---- iframe (uapp.js) routing -------------------------------------------------

// Transport messages a page sends about itself go to the core tagged with
// the page's connection id (the core opens the connection on first sight).
const CONN_METHODS = new Set(["ctx.register", "ctx.loaded", "actions.register", "actions.result", "eval.result", "log.write"]);
function connFor(win) {
  let id = connOfWin.get(win);
  if (!id) { id = connSeq++; connOfWin.set(win, id); winOfConn.set(id, win); }
  return id;
}
function connMessage(win, m, replyTo) {
  const p = sendRpc("conn.msg", { conn: connFor(win), method: m.method, params: m.params || {} });
  if (m.id != null) p.then((result) => replyTo({ id: m.id, result }), (e) => replyTo({ id: m.id, error: { message: String((e && e.message) || e) } }));
  else p.catch(() => {});
}

// Who may talk to the engine: a page in one of THIS shell's frames, at any
// depth (the app frame, the scratch frame, the file viewer, and whatever
// those embed from the archive), and on our own origin. Not the page that
// frames the shell (a desktop, or any same-site page that loads us in an
// iframe), not a sibling or a popup, and not a third-party iframe nested in
// an app (an ad, an embed): those can all reach this window with
// postMessage, and the RPC set behind it includes app.export, sql.* and
// chat.send.
function isOwnFrame(win) {
  if (!win || win === window) return false;
  try {
    let w = win;
    for (let i = 0; i < 64; i++) {
      const p = w.parent;
      if (p === window) return true;
      if (!p || p === w) return false; // reached the top without passing us
      w = p;
    }
  } catch {}
  return false;
}
// A WindowProxy outlives its document: the frame may have closed, or moved
// on to a page that is not ours (another origin, or a frame we no longer
// contain). Those stop getting events.
function stillOurs(win) {
  try { return !win.closed && win.location.origin === location.origin && isOwnFrame(win); }
  catch { return false; } // cross-origin now
}
function postToFrame(win, obj) {
  try { win.postMessage({ __uappMsg: true, m: obj }, location.origin); } catch {}
}
export function broadcastToFrames(envelope) {
  for (const win of clientWindows) {
    if (!stillOurs(win)) { clientWindows.delete(win); continue; }
    postToFrame(win, envelope);
  }
}
// A message from the core for one page connection (worker "conn.send").
export function deliverToConn(conn, msg) {
  const win = winOfConn.get(conn);
  if (win && stillOurs(win)) { try { postToFrame(win, JSON.parse(msg)); } catch {} }
}

window.addEventListener("message", (ev) => {
  const d = ev.data;
  if (!d || !d.__uappRPC || !d.m) return;
  if (ev.origin !== location.origin || !isOwnFrame(ev.source)) return;
  const m = d.m;
  if (!m || typeof m.method !== "string") return;
  const win = ev.source;
  clientWindows.add(win);
  const replyTo = (obj) => postToFrame(win, obj);
  if (m.method === "ctx.register") {
    // A new document in this frame: the previous one (same WindowProxy) is
    // gone, and with it its actions and contexts.
    const old = connOfWin.get(win);
    if (old) { connOfWin.delete(win); winOfConn.delete(old); sendRpc("conn.close", { conn: old }).catch(() => {}); }
  }
  if (m.method === "conn.bye") {
    const id = connOfWin.get(win);
    if (id) { connOfWin.delete(win); winOfConn.delete(id); sendRpc("conn.close", { conn: id }).catch(() => {}); }
    return;
  }
  if (CONN_METHODS.has(m.method)) { connMessage(win, m, replyTo); return; }
  if (m.method === "app.exportBytes" || m.method === "app.exportShareable") { // the shell's own; pages have app.export
    if (m.id != null) replyTo({ id: m.id, error: { message: "unknown method " + m.method } });
    return;
  }
  if (m.method.startsWith("host.")) {
    handleHostRpc(m).then(
      (result) => { if (m.id != null) replyTo({ id: m.id, result }); },
      (e) => { if (m.id != null) replyTo({ id: m.id, error: { message: String((e && e.message) || e) } }); },
    );
    return;
  }
  // Everything else goes to the worker; route the reply back to this iframe
  // (waiting for the app to be open, so early ctx/app.info calls never race).
  appReady.then(() => sendRpc(m.method, m.params || {})).then(
    (result) => { if (m.id != null) replyTo({ id: m.id, result }); },
    (e) => { if (m.id != null) replyTo({ id: m.id, error: { message: String((e && e.message) || e) } }); },
  );
});

// ---- demo host API (used by the launcher .uapp) --------------------------------
// Lets the launcher manage the OPFS app library and switch documents. Only
// the launcher (IS_LAUNCHER): any other app could otherwise read, export or
// delete every app stored in this browser.

// Where the browser build's own page lives on this origin: a hosted site's
// chrome runs at the site root, but its apps open in the DEMO page (the
// archive-less shell), which a site declares with <meta name="uapp-demo">;
// else the page we were loaded from.
function demoBase() {
  const meta = document.querySelector('meta[name="uapp-demo"]');
  return (meta && meta.getAttribute("content")) || BASE.pathname;
}

async function handleHostRpc(m) {
  if (!IS_LAUNCHER) throw new Error(m.method + " is only available to the launcher app");
  const p = m.params || {};
  switch (m.method) {
    case "host.apps":
      // `url` opens the app in the browser build wherever it is hosted (a
      // desktop puts it in a frame window); the library is per origin, so a
      // site and the demo on one host see the same apps.
      return { current: currentAppId, apps: appsIndex().sort((a, b) => b.updated - a.updated)
        .map((a) => ({ ...a, url: demoBase() + "?app=" + encodeURIComponent(a.id) })) };
    case "host.open": {
      const e = appsIndex().find((a) => a.id === p.id);
      if (!e) throw new Error("no stored app with that id");
      splashShow("Opening " + e.name + "…");
      setTimeout(() => switchTo(e.id), 50); // reply first, then navigate
      return { ok: true };
    }
    case "host.create": {
      const id = genId();
      const name = (p.name || "New App").slice(0, 80);
      splashShow("Creating " + name + "…");
      indexTouch(id, name);
      setTimeout(() => switchTo(id), 50);
      return { ok: true, id };
    }
    case "host.import": {
      const bytes = bytesFromB64(p.b64 || "");
      if (bytes.length < 16 || String.fromCharCode(...bytes.slice(0, 15)) !== "SQLite format 3") {
        throw new Error("not a plain .uapp file (encrypted apps can't be opened in the browser demo)");
      }
      const name = (p.name || "Imported app").replace(/\.uapp$/i, "").slice(0, 80);
      // open:false — store it in this browser's app library and stay where we
      // are (a desktop dropping a .uapp onto itself); the default opens it.
      const open = p.open !== false;
      if (open) splashShow("Opening " + name + "…", "Saving…");
      const id = genId();
      // A failed write (storage blocked) must not leave the splash covering
      // the launcher the error message is about.
      try { await appWrite(id, bytes); } catch (e) { splashHide(); throw e; }
      indexTouch(id, name);
      if (open) setTimeout(() => switchTo(id), 50);
      return { ok: true, id, name, url: demoBase() + "?app=" + encodeURIComponent(id) };
    }
    case "host.sample": {
      const name = (p.name || "Sample").slice(0, 80);
      // Several megabytes for some samples: show the bar before the request
      // goes out, and put the splash away again if it fails.
      splashShow("Opening " + name + "…", "Downloading…");
      let bytes;
      try {
        // Relative to the bundle ("examples/x.uapp"), or anywhere on the
        // origin ("/uapp/apps/x.uapp" — a uapp-library shelf).
        bytes = await fetchWithProgress(new URL(String(p.url || ""), BASE), "the sample");
      } catch (e) {
        splashHide();
        throw e;
      }
      splashShow("Opening " + name + "…", "Saving…");
      const id = genId();
      // A failed write (storage blocked) must not leave the splash covering
      // the launcher the error message is about.
      try { await appWrite(id, bytes); } catch (e) { splashHide(); throw e; }
      indexTouch(id, name);
      setTimeout(() => switchTo(id), 50);
      return { ok: true, id };
    }
    case "host.delete":
      await appDelete(p.id);
      return { ok: true };
    case "host.export": {
      const bytes = await appRead(p.id);
      if (!bytes) throw new Error("no stored app with that id");
      const e = appsIndex().find((a) => a.id === p.id);
      return { b64: b64FromBytes(bytes), name: (e && e.name) || "app" };
    }
    default:
      throw new Error("unknown host method " + m.method);
  }
}

// Switch documents by navigating THIS window to the URL that names the next
// one (?app=<id>; nothing = the page's default, usually the launcher) — the
// cleanest reset of the worker, iframes and shell state, and it stays inside
// whatever frame the shell is in: a desktop that put the shell in a window
// sees the window change, not its own page. `replace`, so the switch is not a
// history entry (a reload never was one either).
export function switchTo(id) {
  markLeaving();
  const u = new URL(location.href);
  u.searchParams.delete("app");
  u.searchParams.delete("open");
  if (id) u.searchParams.set("app", id);
  else splashShow("Returning to your apps…"); // covers the navigation gap
  location.replace(u.href);
}
