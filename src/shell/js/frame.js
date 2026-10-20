/* The app frame as the shell sees it: where it lives, and how the two talk.
 *
 * On the desktop the frame has an origin of its own, http://localhost:<port>
 * (see docs/design/app-isolation.md), so this page can't read its document
 * or its location. Everything goes over postMessage: messages in are taken
 * only from exactly the app's origin and from a window inside our own frames,
 * and messages out are addressed to that origin. In the browser build and on
 * a public site the frame shares this page's origin, and the same messages
 * are used there.
 *
 * Frame -> shell:  __uappNav (a page loaded or its hash moved), __uappToolbar
 *                  (uapp.toolbar.*, uapp.panel.*, forwarded shortcuts),
 *                  __uappOpen (a link to another site), __uappToken,
 *                  __uappStorageDone.
 * Shell -> frame:  __uappShell (shortcuts and the link policy),
 *                  __uappToolbarState, __uappTokenIs, __uappStorage.
 */

import { $ } from "./ui.js";

const frameSrc = () => {
  try { return new URL($("appframe")?.getAttribute("src") || "", location.href); } catch { return null; }
};

/// The app frame's origin. Read from the frame's src each time: the browser
/// build only sets it after boot (and keeps it on this origin).
export function appOrigin() {
  const u = frameSrc();
  return u && /^https?:$/.test(u.protocol) ? u.origin : location.origin;
}

/// Is the app on an origin of its own?
export const isolated = () => appOrigin() !== location.origin;

// The app token from the server-written src. Loads the shell starts itself
// (a reload, the scratch frame, a page opened in a tab) are cross-site to the
// app origin and need it; the app's own requests after that don't.
const TOKEN = frameSrc()?.searchParams.get("a") || "";

/// A URL on the app's origin for `path` (as under /app/: "/app/x.html",
/// "/scratch/"), with the token when the app has its own origin.
export function appPageUrl(path) {
  if (!isolated()) return (window.__uappBase || "/") + String(path).replace(/^\/+/, "");
  const u = new URL(path, appOrigin());
  u.searchParams.set("a", TOKEN);
  return u.href;
}

/// Where to open an archive file (an `appUrl()`) in a tab of its own: on the
/// app's origin, so an HTML page runs as the app does. On this origin it
/// would be sandboxed (see server.rs).
export const tabUrl = (url) => (isolated() ? appPageUrl(url) : url);

/// Is `w` a window inside this shell's frames (at any depth)? Walking
/// `parent` is allowed across origins.
export function insideOurFrames(w) {
  try {
    for (let i = 0; w && i < 16; i++) {
      if (w.parent === window) return true;
      if (w === w.parent) return false;
      w = w.parent;
    }
  } catch {}
  return false;
}

export function toFrame(win, msg) {
  try { win && win.postMessage(msg, appOrigin()); } catch {}
}

const handlers = new Map(); // message key -> [fn(value, event)]

/// Handle frame messages carrying `key`.
export function onFrame(key, fn) {
  if (!handlers.has(key)) handlers.set(key, []);
  handlers.get(key).push(fn);
}

window.addEventListener("message", (e) => {
  // Exactly the app's origin, and a page in our own frames: another tab on
  // the same origin (another app in the browser build) must not reach us.
  if (e.origin !== appOrigin() || !insideOurFrames(e.source)) return;
  const d = e.data;
  if (!d || typeof d !== "object") return;
  for (const [key, fns] of handlers) {
    if (key in d) for (const fn of fns) { try { fn(d[key], e); } catch {} }
  }
});

// ---- what the frame is told ----------------------------------------------
// Shortcuts (toolbar-visibility.js) and the link policy (main.js), sent to
// every page that says hello and again whenever they change. The app frame
// loads alongside this page, so its hello may come before we listen: it is
// also sent to the frame directly, and uapp.js says hello again when it first
// hears from us.
const config = {};
const greeted = new Set();
export function setFrameConfig(part) {
  Object.assign(config, part);
  const app = $("appframe")?.contentWindow;
  if (app) greeted.add(app);
  for (const w of greeted) {
    if (w.closed) greeted.delete(w);
    else toFrame(w, { __uappShell: config });
  }
}
onFrame("__uappNav", (_, e) => {
  greeted.add(e.source);
  toFrame(e.source, { __uappShell: config });
});
onFrame("__uappToken", (_, e) => toFrame(e.source, { __uappTokenIs: TOKEN }));
