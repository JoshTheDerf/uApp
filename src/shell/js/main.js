/* uapp shell — entry point. Owns the app iframe (reload on file changes),
 * native-window file drops, the shutdown overlay, and hash deep-links.
 * Everything visible is a component (see the imports). */

import { connect, on, rpc, closeSocket, state } from "./core.js";

// ---------- External URL handling ----------
// When running in the native Tauri window, intercept window.open and link
// clicks to external URLs so they open in the system browser instead of
// within the app's webview.
function isInternalUrl(url) {
  try {
    const u = new URL(url, location.href);
    const h = u.hostname;
    // Allow localhost and loopback addresses — everything else is external.
    return h === "localhost" || h === "127.0.0.1" || h.startsWith("127.") || h === "::1" || h === "[::1]";
  } catch {
    return false;
  }
}

async function openExternalUrl(url) {
  if (isInternalUrl(url)) return false; // Let internal URLs navigate normally
  // The click was already prevented; a swallowed failure would be a dead link.
  try { await rpc("browser.open", { url }); } catch (e) { dlgAlert("Could not open " + url + ": " + (e.message || e)); }
  return true; // Handled by system browser
}

// Override window.open to intercept external URLs
const originalWindowOpen = window.open;
window.open = function(url, ...args) {
  if (url && typeof url === "string" && !isInternalUrl(url)) {
    openExternalUrl(url);
    return null; // Prevent the webview from opening a new window
  }
  return originalWindowOpen.call(this, url, ...args);
};

// Intercept clicks on links that would open externally
document.addEventListener("click", (e) => {
  const a = e.target.closest("a");
  if (!a) return;
  const url = a.href;
  const target = a.target || "_self";

  // Handle target="_blank" or any external URL
  if ((target === "_blank" || target === "_new") && url && !isInternalUrl(url)) {
    e.preventDefault();
    openExternalUrl(url);
    return;
  }

  // For regular same-frame links to external URLs, also open externally
  // when running in the native window (browser shell handles its own navigation)
  if (url && !isInternalUrl(url) && document.body.classList.contains("native")) {
    e.preventDefault();
    openExternalUrl(url);
  }
}, true); // Capture phase to intercept before other handlers
import { $, esc, div, openPanel, panelOpen, dlgAlert } from "./ui.js";
import { S } from "./strings.js";
import "./topbar.js";
import "./chat-panel.js";
import "./sql-panel.js";
import "./files-panel.js";
import "./viewer.js";
import "./settings-panel.js";
import "./tools-panel.js";
import "./mirror.js";
import "./keepalive.js";
import { addRefs } from "./chat-panel.js";
import { fbCwd, loadFiles, pickDest } from "./files-panel.js";
import { isUappFile, updateFromDrop, updateFromFile } from "./template-update.js";

// ---------- app iframe ----------
// Reloads must not grow the session history. Assigning a cache-busted src is a
// navigation to a NEW url, so every save pushed another history entry, and
// Android WebViews keep per-entry document state alive (worst when the app
// embeds an iframe of its own: each entry pins a whole extra frame tree). Since
// everything under /app/ is served no-store, the url can stay stable and the
// reload can replace the current entry instead of stacking one on top of it.
let reloadTimer = null;
function reloadAppFrame() {
  const f = $("appframe");
  if (!f) return;
  const url = (window.__uappBase || "/") + "app/";
  // Same-origin, so we can drive the frame's own location; src= is the fallback
  // for the window not being reachable yet (frame still empty on first boot).
  // Reload the page the frame is ON (a multi-page site stays on /admin), not
  // the app root.
  try {
    const cur = f.contentWindow.location;
    if (cur.href && cur.href !== "about:blank") { cur.reload(); return; }
    cur.replace(url);
    return;
  } catch {}
  f.src = url;
}
function scheduleReload() {
  clearTimeout(reloadTimer);
  reloadTimer = setTimeout(reloadAppFrame, 400);
}
// Writes do NOT reload the frame on their own any more: a half-finished
// multi-file edit would flash a broken page, and in hosted-site mode the frame
// is the page the reader is on. Reloads are explicit — the topbar button, the
// AI's `reload_app` tool (an `app.reload` RPC → "reload" event) — and go
// through scheduleReload so a burst collapses into one.
on("reload-app", scheduleReload);
on("reload", scheduleReload);

// ---------- url mirroring ----------
// The frame is the real page; the address bar should say so. Every navigation
// inside it (link click, the reload above, a scripted location change) is
// mirrored to the top-level URL with the history API, so /admin reached from
// / is /admin in the bar, deep-linkable and shareable. The browser back/forward
// buttons then drive the frame the same way.
const APP_ROUTE = (window.__uappBase || "/") + "app/";
// The frame's path, as the page's own path: "/app/admin" and "/admin" (a
// root-absolute link followed inside the frame — the service worker serves it
// from the archive either way) are both "/admin".
function frameToTopPath(u) {
  const path = u.pathname.startsWith(APP_ROUTE) ? "/" + u.pathname.slice(APP_ROUTE.length) : u.pathname;
  if (path.startsWith("/shell/") || path === "/scratch/") return null;
  return path + u.search + u.hash;
}
function mirrorFrameUrl() {
  const f = $("appframe");
  let u;
  try { u = new URL(f.contentWindow.location.href); } catch { return; }
  if (u.origin !== location.origin) return;
  const top = frameToTopPath(u);
  if (top === null || top === location.pathname + location.search + location.hash) return;
  try { history.pushState({ uappFrame: top }, "", top); } catch {}
}
// Hosted-site mode only: there the top URL IS a page of the site and reload
// serves it from the server. In the native window or the browser demo the
// top URL is the shell's, and rewriting it would break reload.
const MIRROR_URL = !!window.__uappSiteArchive;
$("appframe")?.addEventListener("load", () => {
  if (!MIRROR_URL) return;
  mirrorFrameUrl();
  // In-page hash routing inside the app.
  try { $("appframe").contentWindow.addEventListener("hashchange", mirrorFrameUrl); } catch {}
  // Links to other origins leave the editor: the frame is this site's
  // in-browser copy, so another site has no business loading inside it (and
  // the service worker never sees a cross-origin frame navigation, so this is
  // the only place to catch it). Same-origin links stay in the frame; those
  // that aren't pages of the archive break out via sw.js instead.
  try {
    $("appframe").contentWindow.document.addEventListener("click", (e) => {
      const a = e.target && e.target.closest && e.target.closest("a[href]");
      if (!a || a.target || e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      let u;
      try { u = new URL(a.href, a.baseURI); } catch { return; }
      if (u.origin === location.origin || !/^https?:$/.test(u.protocol)) return;
      e.preventDefault();
      // Let a pending local-copy save land first (browser build).
      Promise.resolve(window.__uappFlushSave && window.__uappFlushSave()).then(() => { window.top.location.href = u.href; });
    }, true);
  } catch {}
});
window.addEventListener("popstate", () => {
  if (!MIRROR_URL) return;
  const f = $("appframe");
  const wantTop = location.pathname + location.search + location.hash;
  try {
    const cur = new URL(f.contentWindow.location.href);
    if (frameToTopPath(cur) === wantTop) return;
    f.contentWindow.location.replace(APP_ROUTE + location.pathname.replace(/^\/+/, "") + location.search + location.hash);
  } catch { f.src = APP_ROUTE + location.pathname.replace(/^\/+/, "") + location.search + location.hash; }
});

// ---------- scratch iframe ----------
// The AI's run_js scratchpad context. Loaded on demand rather than with the
// shell: it is a whole extra document, and on a phone webview that is budget
// the app itself needs. The server broadcasts "scratch-load" when a scratchpad
// call arrives with no frame connected, then waits for it to register.
function loadScratchFrame() {
  const f = $("scratchframe");
  if (!f || f.getAttribute("src")) return; // already loading or loaded
  f.src = (window.__uappBase || "/") + "scratch/";
}
on("scratch-load", loadScratchFrame);

// ---------- shutdown ----------
function shutdownShell(title, detail) {
  if (state.shuttingDown) return;
  // Native window: no overlay, no window.close() guesswork (webviews ignore
  // it) — the server closes the OS window itself before exiting; asking via
  // win.close is just a belt-and-braces fallback while the socket lives.
  if (document.body.classList.contains("native")) {
    rpc("win.close").catch(() => {});
    closeSocket();
    return;
  }
  closeSocket();
  const ov = div("");
  ov.id = "shutdown-overlay";
  ov.innerHTML = `<div class="card"><h2>${esc(title)}</h2><p>${esc(detail)}</p>
    <p class="dim">${esc(S.main.windowCloseHint)}</p></div>`;
  document.body.appendChild(ov);
  setTimeout(() => { try { window.close(); } catch {} }, 800);
}
on("shutdown", (p) => shutdownShell(p.title, p.detail));
on("sync", (p) => {
  if (p.state === "closed") {
    shutdownShell(S.main.appClosed, S.main.reopenHint);
  }
});

// ---------- native window file drops ----------
// In the desktop/mobile window the OS hands file drops to the native runtime,
// so the webview never sees dragover/drop for them (that's why plain HTML5
// drop handlers look broken there). The server forwards the real drop with
// the cursor position; we find the drop zone under it ourselves.
function dropZoneAt(x, y) {
  if (x == null) return null; // the drag left the window — no zone at all
  const el = document.elementFromPoint(x, y);
  const fbOpen = panelOpen("filebrowser");
  if (el && fbOpen && el.closest("#filebrowser")) return { kind: "files", el: $("fb-pane") };
  if (el && el.closest("#sidebar") && panelOpen("sidebar")) {
    return { kind: "chat", el: $("sidebar") };
  }
  // Dropped somewhere neutral: the file browser is the natural home when
  // it's open, the chat otherwise.
  if (fbOpen) return { kind: "files", el: $("fb-pane") };
  if (panelOpen("sidebar")) return { kind: "chat", el: $("sidebar") };
  return null;
}
// A .uapp being dragged in updates the app; it is not filed anywhere, so no
// drop zone should light up for it. Only the drag's first event carries names,
// so remember the verdict until the drag ends.
let dragIsApp = false;
function nativeDropHover(x, y, names) {
  if (x == null) dragIsApp = false;
  else if (names && names.length) dragIsApp = names.length === 1 && isUappFile(names[0]);
  const zone = dragIsApp ? null : dropZoneAt(x, y);
  for (const el of [$("fb-pane"), $("sidebar")]) {
    el.classList.toggle("dropping", !!zone && zone.el === el);
  }
}
async function nativeDropFiles(p) {
  nativeDropHover(null);
  // A .uapp is not content for this app — it's another version OF an app. Drop
  // one on the window and the offer is to update this app's code from it,
  // wherever in the window it landed (see template-update.js).
  const names = p.names || [];
  if (names.length === 1 && isUappFile(names[0])) {
    updateFromDrop(p);
    return;
  }
  const zone = dropZoneAt(p.x, p.y);
  if (!zone) {
    dlgAlert(S.main.dropHint);
    return;
  }
  try {
    if (zone.kind === "chat") {
      // Chat attachments from a native drop are stored first and attached by
      // reference — the bytes are already on this machine, not in the page.
      const r = await rpc("drop.ingest", { id: p.id, dir: "data/" });
      addRefs((r.files || []).map((f) => f.name));
      if (panelOpen("filebrowser")) loadFiles();
      if (r.skipped?.length) dlgAlert(S.main.notAttached(r.skipped.join(", ")));
    } else {
      const dir = fbCwd() || (await pickDest());
      if (!dir) return;
      const r = await rpc("drop.ingest", { id: p.id, dir });
      await loadFiles();
      if (r.skipped?.length) dlgAlert(S.main.notAdded(r.skipped.join(", ")));
    }
  } catch (e) { dlgAlert(e.message); }
}
on("native-drop", (p) => {
  if (p.type === "drop_hover") nativeDropHover(p.x, p.y, p.names);
  else if (p.type === "drop_leave") nativeDropHover(null);
  else nativeDropFiles(p);
});

// ---------- browser-shell file drops ----------
// The browser shell (unlike the native window) gets real HTML5 drop events.
// Panels handle their own uploads; a single .uapp dropped ANYWHERE in the shell
// is claimed here first, because it means "update this app", not "store this".
document.addEventListener("drop", (e) => {
  const files = [...((e.dataTransfer && e.dataTransfer.files) || [])];
  if (files.length !== 1 || !isUappFile(files[0].name)) return;
  e.preventDefault();
  e.stopPropagation();
  for (const el of [$("fb-pane"), $("sidebar")]) el?.classList.remove("dropping");
  updateFromFile(files[0]);
}, true);
// Without this the browser navigates away to the dropped file when it lands
// outside a panel, and the drop never reaches the handler above.
document.addEventListener("dragover", (e) => {
  if (e.dataTransfer && [...e.dataTransfer.types].includes("Files")) e.preventDefault();
});

// ---------- boot ----------
if (location.hash === "#tools") setTimeout(() => openPanel("toolspanel"), 400);
if (location.hash === "#files") setTimeout(() => openPanel("filebrowser"), 400);
if (location.hash === "#sql") setTimeout(() => openPanel("sqlpanel"), 400);
connect();
