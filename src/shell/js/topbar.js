/* <shell-topbar> — app title (rename), the unsaved-app banner, panel toggle
 * buttons, and the window controls for the frameless desktop window. */

import { rpc, on, emit } from "./core.js";
import { $, I, installStyle, togglePanel, openPanel, dlgAlert, downloadUrl } from "./ui.js";
import { mirrorRefresh } from "./mirror.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div id="title-wrap" title="${S.topbar.dblClickRename}">
    <span id="title"></span>
    <button id="btn-rename" title="${S.topbar.renameApp}"></button>
  </div>
  <div id="unsaved-bar" class="hidden" title="${S.topbar.unsavedBanner}">
    <span id="unsaved-msg"></span>
    <button id="btn-download-app"></button>
    <button id="btn-open-app" class="hidden" title="${S.topbar.loadUappTitle}"></button>
  </div>
  <div id="unlinked-bar" class="hidden" title="${S.topbar.notLinkedTitle}">
    <span id="unlinked-msg"></span>
    <button id="btn-link-app"></button>
  </div>
  <div class="spacer"></div>
  <button id="btn-files" class="bar-collapse" title="${S.topbar.browseFiles}"></button>
  <button id="btn-sql" class="bar-collapse" title="${S.topbar.browseDatabase}"></button>
  <button id="btn-tools" class="bar-collapse" title="${S.topbar.toolsBtn}"></button>
  <button id="btn-settings" class="bar-collapse" title="${S.topbar.settingsBtn}"></button>
  <button id="btn-chat" class="bar-collapse" title="${S.topbar.chatBtn}"></button>
  <span class="winctl-sep bar-collapse"></span>
  <button id="btn-reload" class="winctl bar-collapse" title="${S.topbar.reloadApp}"></button>
  <button id="btn-quit" class="winctl winctl-close hide-native bar-collapse" title="${S.topbar.closeApp}"></button>
  <button id="btn-menu" title="${S.topbar.menuBtn}"></button>
  <div id="topbar-menu" class="hidden"></div>
  <div id="winctls" class="native-only">
    <button id="win-min" class="wbtn" title="${S.topbar.minimize}"></button>
    <button id="win-max" class="wbtn" title="${S.topbar.maximizeRestore}"></button>
    <button id="win-close" class="wbtn wbtn-close" title="${S.topbar.closeWindow}"></button>
  </div>
`;

const CSS = /* css */ `
shell-topbar {
  display: flex; align-items: center; gap: 10px;
  padding: 6px 12px; background: var(--ink); color: var(--chrome-text);
  flex: 0 0 auto;
  position: relative; /* anchors the collapsed ⋮ dropdown */
}
shell-topbar button {
  background: transparent; border: none; color: var(--chrome-dim);
  cursor: pointer; padding: 5px 7px; border-radius: 6px;
  display: inline-flex; align-items: center; gap: 5px;
}
shell-topbar button:hover { background: rgba(255,255,255,.14); color: #fff; }
shell-topbar button.active { background: rgba(255,255,255,.2); color: #fff; }
shell-topbar button svg { display: block; }
.winctl-sep { width: 1px; height: 18px; background: rgba(255,255,255,.18); margin: 0 4px; }
.winctl-close:hover { background: var(--err) !important; color: #fff !important; }

/* --- native (frameless desktop window) chrome --- */
.native-only { display: none !important; }
body.native .native-only { display: inline-flex !important; }
body.native .hide-native { display: none !important; }
#winctls { align-items: center; gap: 2px; margin-left: 4px; }
#winctls .wbtn {
  width: 36px; height: 26px; padding: 0; border-radius: 6px; color: var(--chrome-dim);
  display: inline-flex; align-items: center; justify-content: center;
}
#winctls .wbtn:hover { background: rgba(255,255,255,.14); color: #fff; }
#winctls .wbtn-close:hover { background: var(--err); color: #fff; }
/* In the native window the top bar doubles as the drag handle; the empty
   areas get the move cursor, interactive bits keep the pointer. */
body.native shell-topbar { cursor: move; }
body.native shell-topbar button,
body.native shell-topbar #title-wrap,
body.native shell-topbar #unsaved-bar,
body.native shell-topbar #unlinked-bar { cursor: default; }

#title-wrap { display: flex; align-items: center; gap: 4px; min-width: 0; }
#title { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
#title[contenteditable="true"] {
  outline: 1px dashed rgba(255,255,255,.5); border-radius: 4px; padding: 0 5px;
  background: rgba(255,255,255,.08);
}
#btn-rename { opacity: 0; transition: opacity .15s; padding: 3px 4px !important; }
#title-wrap:hover #btn-rename { opacity: .8; }

/* "not saved" banner for a scratch app (opened with no file), and the
   "not linked" banner for an intent-opened doc whose changes are NOT
   writing back to the original file (Android; see mirror.js). */
#unsaved-bar, #unlinked-bar {
  display: flex; align-items: center; gap: 8px; margin-left: 4px;
  background: #5a4410; border: 1px solid #8a6a1e; border-radius: 8px;
  padding: 2px 6px 2px 10px;
}
#unsaved-bar.hidden, #unlinked-bar.hidden { display: none; }
#unsaved-msg, #unlinked-msg {
  display: inline-flex; align-items: center; gap: 5px;
  font-size: 12px; color: #ffd77a; white-space: nowrap;
}
#unsaved-msg svg, #unlinked-msg svg { display: block; }
#btn-download-app, #btn-link-app {
  display: inline-flex; align-items: center; gap: 4px;
  background: #f0c04a; color: #3a2c05; border-radius: 6px;
  padding: 2px 9px; font-size: 12px; font-weight: 600;
}
#btn-download-app:hover, #btn-link-app:hover { background: #ffd062; color: #3a2c05; }
/* Secondary action in the same banner: a new app is also the moment someone
   wants to open one they already have (see #btn-open-app in wire()). Kept
   quieter than Save As so the primary action still reads first. */
#btn-open-app {
  display: inline-flex; align-items: center; gap: 4px;
  border: 1px solid #f0c04a; color: #ffd77a; border-radius: 6px;
  padding: 1px 8px; font-size: 12px; font-weight: 600;
}
#btn-open-app:hover { background: rgba(240,192,74,.22); color: #fff; }
#btn-open-app.hidden { display: none; }
/* The banners never shrink; the title ellipsizes instead. */
#unsaved-bar, #unlinked-bar { flex-shrink: 0; }

/* --- collapsed menu (phones, skinny browser windows) --- */
/* The full row of panel/window buttons doesn't fit next to the title and the
   "Not saved" banner, so on narrow screens they collapse into one ⋮ menu.
   The banner keeps its full text — the freed space is what pays for it. */
#btn-menu { display: none; }
@media (max-width: 640px) {
  shell-topbar .bar-collapse { display: none !important; }
  #btn-menu { display: inline-flex; }
  /* Phone = touch: bigger targets on what remains in the bar. */
  shell-topbar button { padding: 9px 10px; }
  #btn-rename { display: none; } /* rename stays reachable via double-tap */
  #btn-download-app, #btn-link-app { padding: 6px 12px; }
  /* Three chips + the title don't fit a phone-width bar: the open button
     keeps its icon (and its tooltip) and drops the words. */
  #btn-open-app { padding: 5px 8px; }
  #btn-open-app .bar-label { display: none; }
  #unsaved-bar, #unlinked-bar { padding: 3px 7px 3px 11px; }
}
#topbar-menu {
  position: absolute; top: calc(100% - 4px); right: 8px; z-index: 60;
  display: flex; flex-direction: column; min-width: 220px;
  background: var(--ink); color: var(--chrome-text);
  border: 1px solid rgba(255,255,255,.16); border-radius: 10px;
  box-shadow: 0 8px 24px rgba(0,0,0,.35); padding: 6px; gap: 2px;
}
#topbar-menu.hidden { display: none; }
#topbar-menu button {
  justify-content: flex-start; gap: 10px; width: 100%;
  padding: 11px 12px; font-size: 14px; border-radius: 8px;
}
#topbar-menu button.active { background: rgba(255,255,255,.2); color: #fff; }
#topbar-menu .menu-sep { height: 1px; background: rgba(255,255,255,.14); margin: 4px 6px; }

@media (max-width: 480px) { shell-topbar { gap: 4px; padding: 6px 8px; } }
/* Tablets: wide enough to keep the full button row, but still touch. */
@media (pointer: coarse) {
  shell-topbar button { padding: 9px 10px; }
  #btn-download-app, #btn-link-app { padding: 6px 12px; }
  #btn-open-app { padding: 5px 11px; }
}
`;

class ShellTopbar extends HTMLElement {
  connectedCallback() {
    installStyle("shell-topbar", CSS);
    this.innerHTML = TPL;
    // Until the first app.info lands, the document title carries the name.
    $("title").textContent = document.title.split(" — ")[0] || "uapp";
    wire();
  }
}

// Icon + text label for the banner, with the text collapsible on narrow
// screens (see the .bar-label media query above).
const barLabel = (icon, text) => `${I(icon, 12)} <span class="bar-label">${text}</span>`;

let appPath = null; // the open document's working path (for the Link banner)

function renderInfo(info) {
  appPath = info.path || appPath;
  const t = $("title");
  if (info.name && !t.isContentEditable && t.textContent !== info.name) {
    t.textContent = info.name;
  }
  $("unsaved-bar").classList.toggle("hidden", !info.unsaved);
  // Running in the native desktop window? Reveal the custom window controls
  // and turn the scratch "Download" button into a native "Save…" dialog.
  // (On mobile the app is native too, but has no window controls — `desktop`
  // is absent in older cores, which still show them.)
  document.body.classList.toggle("native", !!info.native && info.desktop !== false);
  // Wording: desktop says "Save As…" (the native window opens the OS save
  // dialog; a browser tab saves by downloading a copy). Mobile keeps
  // "Download" — there is no save dialog there.
  if (info.desktop !== false) {
    const b = $("btn-download-app");
    if (b.dataset.saveas !== "1" && b.textContent.trim() !== "Saved") {
      b.dataset.saveas = "1";
      b.innerHTML = barLabel("save", S.settings.saveAs);
    }
    $("unsaved-bar").title = S.settings.unsavedTitle;
  }
  // "Load uApp file" rides along with the not-saved banner: a blank new app is
  // exactly where someone who launched the app from its icon (rather than by
  // double-clicking a file) needs a way in. Desktop-only — it starts another
  // instance from a native file dialog, which mobile and the browser shell
  // can't do (there, opening a .uapp is the OS's / the tab's job).
  $("btn-open-app").classList.toggle(
    "hidden", !(info.native && info.desktop !== false && !info.wasm),
  );
}

// ---- rename ----
function startRename() {
  const titleEl = $("title");
  if (titleEl.isContentEditable) return;
  const orig = titleEl.textContent;
  titleEl.contentEditable = "true";
  titleEl.focus();
  document.execCommand && document.execCommand("selectAll", false, null);
  const finish = async (save) => {
    titleEl.contentEditable = "false";
    const name = titleEl.textContent.trim();
    if (!save || !name || name === orig) { titleEl.textContent = orig; return; }
    try {
      await rpc("app.rename", { name });
      document.title = `${name} — uapp`;
    } catch (e) { titleEl.textContent = orig; dlgAlert(e.message); }
  };
  titleEl.onkeydown = (e) => {
    if (e.key === "Enter") { e.preventDefault(); finish(true); }
    if (e.key === "Escape") { e.preventDefault(); finish(false); }
  };
  titleEl.onblur = () => finish(true);
}

// The ⋮ dropdown holding the buttons that don't fit on narrow screens. Items
// just click their (hidden) source button, so every action and its enabled/
// active logic lives in exactly one place.
function buildMenu() {
  const menu = $("topbar-menu");
  const ITEMS = [
    ["btn-files", "files", S.topbar.menuFiles],
    ["btn-sql", "database", S.topbar.menuDatabase],
    ["btn-tools", "wrench", S.topbar.menuTools],
    ["btn-settings", "settings", S.topbar.menuSettings],
    ["btn-chat", "message-square", S.topbar.menuChat],
    null, // separator
    ["btn-reload", "rotate-cw", S.topbar.reloadApp],
    ["btn-quit", "x", S.topbar.closeApp, "hide-native"],
  ];
  for (const it of ITEMS) {
    if (!it) { menu.insertAdjacentHTML("beforeend", '<div class="menu-sep"></div>'); continue; }
    const [src, icon, label, extra] = it;
    const b = document.createElement("button");
    b.dataset.src = src;
    if (extra) b.className = extra;
    b.innerHTML = I(icon, 16) + " " + label;
    b.onclick = () => { closeMenu(); $(src).click(); };
    menu.appendChild(b);
  }
  const closeMenu = () => menu.classList.add("hidden");
  $("btn-menu").innerHTML = I("ellipsis-vertical", 18);
  $("btn-menu").onclick = (e) => {
    e.stopPropagation();
    if (!menu.classList.contains("hidden")) return closeMenu();
    // Mirror the panel buttons' active state onto the menu items.
    for (const b of menu.querySelectorAll("button[data-src]")) {
      b.classList.toggle("active", $(b.dataset.src).classList.contains("active"));
    }
    menu.classList.remove("hidden");
  };
  document.addEventListener("click", (e) => {
    if (!e.target.closest("#topbar-menu, #btn-menu")) closeMenu();
  });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeMenu(); });
}

function wire() {
  buildMenu();
  $("btn-tools").innerHTML = I("wrench");
  $("btn-settings").innerHTML = I("settings");
  $("btn-chat").innerHTML = I("message-square");
  $("btn-files").innerHTML = I("files");
  $("btn-sql").innerHTML = I("database");
  $("btn-reload").innerHTML = I("rotate-cw");
  $("btn-quit").innerHTML = I("x", 17);
  $("btn-rename").innerHTML = I("pencil", 12);
  $("unsaved-msg").innerHTML =
    I("circle-alert", 13) + ` <span class="bar-label">${S.topbar.notSaved}</span>`;
  // "Not linked" warning: an intent-opened doc whose changes stay in the
  // device's private copy. The button opens the system file picker directly
  // (same as Settings → Link); the optional all-files grant stays in
  // Settings. Driven by the "uapp-mirror" events mirror.js broadcasts.
  $("unlinked-msg").innerHTML =
    I("circle-alert", 13) + ` <span class="bar-label">${S.topbar.notLinked}</span>`;
  $("btn-link-app").innerHTML = I("link", 12) + " " + S.topbar.linkNow;
  $("btn-link-app").onclick = () => {
    const b = window.UAppAndroid;
    if (!(typeof b?.linkOriginal === "function" && appPath)) return openPanel("settingspanel");
    b.linkOriginal(appPath); // opens the system picker
    // Poll for the pick result (no callback across the bridge); mirrorRefresh
    // broadcasts, which hides this banner the moment the link lands.
    let tries = 0;
    const t = setInterval(() => {
      if (mirrorRefresh(appPath) || ++tries > 90) clearInterval(t);
    }, 1000);
  };
  window.addEventListener("uapp-mirror", (e) => {
    $("unlinked-bar").classList.toggle("hidden", !(e.detail.relevant && !e.detail.linked));
  });
  // Label set to "Download" here (what mobile keeps); renderInfo() swaps it
  // to "Save As…" on desktop.
  $("btn-download-app").innerHTML = barLabel("download", S.common.download);

  $("btn-chat").onclick = () => togglePanel("sidebar");
  $("btn-sql").onclick = () => togglePanel("sqlpanel");
  $("btn-files").onclick = () => togglePanel("filebrowser");
  $("btn-settings").onclick = () => togglePanel("settingspanel");
  $("btn-tools").onclick = () => togglePanel("toolspanel");
  $("btn-reload").onclick = () => emit("reload-app");
  $("btn-quit").onclick = async () => {
    try { await rpc("app.quit"); } catch {}
    // the "closed" sync event finishes the job; fall back locally
    setTimeout(() => emit("shutdown", {
      title: S.topbar.appClosed, detail: S.topbar.reopenHint,
    }), 1200);
  };
  $("btn-rename").onclick = startRename;
  $("title-wrap").ondblclick = startRename;

  // Scratch app download / native save-as.
  $("btn-download-app").onclick = async () => {
    if (!document.body.classList.contains("native")) return downloadUrl("/download.uapp");
    const btn = $("btn-download-app");
    const orig = btn.innerHTML;
    btn.disabled = true;
    try {
      const r = await rpc("app.save");
      if (r && r.saved) {
        btn.innerHTML = barLabel("check", S.topbar.saved);
        setTimeout(() => { btn.innerHTML = orig; }, 1800);
      } else {
        btn.innerHTML = orig; // cancelled
      }
    } catch (e) {
      btn.innerHTML = orig;
      dlgAlert(S.settings.saveFailed(e.message));
    } finally { btn.disabled = false; }
  };

  // Open another .uapp: a native file dialog, then the chosen app starts in
  // its own window (this one keeps running — one app per process).
  $("btn-open-app").innerHTML = barLabel("folder-open", S.topbar.loadUapp);
  $("btn-open-app").onclick = async () => {
    const btn = $("btn-open-app");
    btn.disabled = true;
    try {
      await rpc("app.openFile"); // {opened:false} when the dialog was cancelled
    } catch (e) {
      dlgAlert(S.settings.openFailed(e.message));
    } finally { btn.disabled = false; }
  };

  // ---- native window controls (frameless desktop window) ----
  $("win-min").innerHTML = I("minus", 16);
  $("win-max").innerHTML = I("square", 13);
  $("win-close").innerHTML = I("x", 16);
  $("win-min").onclick = () => rpc("win.minimize").catch(() => {});
  $("win-max").onclick = () => rpc("win.maximize").catch(() => {});
  $("win-close").onclick = () => rpc("win.close").catch(() => {});
  // The top bar is the OS drag handle: mousedown on an empty area starts an
  // OS window move; double-click toggles maximize. Interactive controls and
  // the title (double-click renames) are excluded.
  const DRAG_SKIP = "button, input, select, a, [contenteditable], #title-wrap, #unsaved-bar, #unlinked-bar, #winctls";
  const bar = document.querySelector("shell-topbar");
  const onDragHandle = (e) =>
    document.body.classList.contains("native") && e.button === 0 && !e.target.closest(DRAG_SKIP);
  bar.addEventListener("mousedown", (e) => { if (onDragHandle(e)) rpc("win.drag").catch(() => {}); });
  bar.addEventListener("dblclick", (e) => {
    if (document.body.classList.contains("native") && !e.target.closest(DRAG_SKIP)) rpc("win.maximize").catch(() => {});
  });

  on("info", renderInfo);
}

customElements.define("shell-topbar", ShellTopbar);
