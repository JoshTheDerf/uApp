/* <shell-topbar> — app title (rename), the unsaved-app banner, panel toggle
 * buttons, and the window controls for the frameless desktop window. */

import { rpc, on, emit } from "./core.js";
import { $, I, installStyle, togglePanel, openPanel, dlgAlert, downloadUrl, isComposing, menuFrom } from "./ui.js";
import { toggleToolbar, shortcutLabel, toolbarDefault } from "./toolbar-visibility.js";
import { mirrorRefresh } from "./mirror.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div id="title-wrap" title="${S.topbar.dblClickRename}">
    <img id="app-icon" alt="" width="20" height="20">
    <span id="title"></span>
    <button id="btn-rename" title="${S.topbar.renameApp}" aria-label="${S.topbar.renameApp}"></button>
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
  <button id="btn-files" class="bar-btn bar-collapse"></button>
  <button id="btn-sql" class="bar-btn bar-collapse"></button>
  <button id="btn-tools" class="bar-btn bar-collapse"></button>
  <button id="btn-settings" class="bar-btn bar-collapse"></button>
  <button id="btn-chat" class="bar-btn"></button>
  <span class="winctl-sep bar-collapse"></span>
  <button id="btn-hidebar" class="bar-btn winctl bar-collapse"></button>
  <button id="btn-reload" class="bar-btn winctl bar-collapse" title="${S.topbar.reloadApp}" aria-label="${S.topbar.reloadApp}"></button>
  <button id="btn-quit" class="bar-btn winctl winctl-close hide-native bar-collapse" title="${S.topbar.closeApp}" aria-label="${S.topbar.closeApp}"></button>
  <button id="btn-menu" class="bar-btn" title="${S.topbar.menuBtn}" aria-label="${S.topbar.menuBtn}"></button>
  <div id="winctls" class="native-only">
    <button id="win-min" class="wbtn" title="${S.topbar.minimize}" aria-label="${S.topbar.minimize}"></button>
    <button id="win-max" class="wbtn" title="${S.topbar.maximizeRestore}" aria-label="${S.topbar.maximizeRestore}"></button>
    <button id="win-close" class="wbtn wbtn-close" title="${S.topbar.closeWindow}" aria-label="${S.topbar.closeWindow}"></button>
  </div>
`;

const CSS = /* css */ `
shell-topbar {
  display: flex; align-items: center; gap: var(--sp-1);
  height: var(--bar-h); padding: 0 var(--sp-2) 0 var(--sp-3);
  background: var(--ink); color: var(--chrome-text);
  flex: 0 0 auto;
  position: relative;
  /* the brand ring is too dark to see on the chrome */
  --ring: 2px solid var(--chrome-accent);
}
shell-topbar button {
  background: transparent; border: none; color: var(--chrome-dim); font: inherit;
  cursor: pointer; border-radius: var(--r-s);
  display: inline-flex; align-items: center; justify-content: center; gap: var(--sp-1);
  transition: background var(--dur-1) var(--ease), color var(--dur-1) var(--ease);
}
shell-topbar button:hover { background: var(--chrome-hover); color: var(--chrome-text); }
shell-topbar button svg { display: block; flex: 0 0 auto; }
.bar-btn { width: var(--h-m); height: var(--h-m); padding: 0; flex: 0 0 auto; }
/* the open panel's button: tinted, so it reads as "this one is showing" */
shell-topbar .bar-btn.active {
  background: color-mix(in srgb, var(--chrome-accent) 26%, transparent);
  color: var(--chrome-text);
}
/* Chat gets a label on wide screens: it is the one people look for first. */
#btn-chat .bar-label { display: none; }
@media (min-width: 900px) {
  #btn-chat {
    width: auto; padding: 0 var(--sp-3) 0 10px; margin-left: var(--sp-1);
    border: 1px solid var(--chrome-line); border-radius: var(--r-pill);
  }
  #btn-chat .bar-label { display: inline; font-size: var(--fs-md); font-weight: 500; }
  #btn-chat.active { border-color: transparent; }
}
.winctl-sep { width: 1px; height: 18px; background: var(--chrome-line); margin: 0 var(--sp-1); flex: 0 0 auto; }
.winctl-close:hover { background: var(--err) !important; color: var(--on-err) !important; }

/* --- native (frameless desktop window) chrome --- */
.native-only { display: none !important; }
body.native .native-only { display: inline-flex !important; }
body.native .hide-native { display: none !important; }
#winctls { align-items: center; gap: 2px; margin-left: var(--sp-1); }
#winctls .wbtn { width: 36px; height: 28px; padding: 0; }
#winctls .wbtn-close:hover { background: var(--err); color: var(--on-err); }
/* In the native window the top bar doubles as the drag handle; the empty
   areas get the move cursor, interactive bits keep the pointer. */
body.native shell-topbar { cursor: move; }
body.native shell-topbar button,
body.native shell-topbar #title-wrap,
body.native shell-topbar #unsaved-bar,
body.native shell-topbar #unlinked-bar { cursor: default; }

#title-wrap { display: flex; align-items: center; gap: var(--sp-2); min-width: 0; }
#app-icon { width: 20px; height: 20px; border-radius: var(--r-xs); flex: 0 0 auto; }
#title {
  font-weight: 600; font-size: var(--fs-base);
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
#title[contenteditable="true"] {
  outline: 1px dashed var(--chrome-dim); border-radius: var(--r-xs); padding: 0 5px;
  background: var(--chrome-hover);
}
#btn-rename { width: 24px; height: 24px; opacity: 0; }
#title-wrap:hover #btn-rename, #btn-rename:focus-visible { opacity: .85; }

/* "not saved" banner for a scratch app (opened with no file), and the
   "not linked" banner for an intent-opened doc whose changes are NOT
   writing back to the original file (Android; see mirror.js). */
#unsaved-bar, #unlinked-bar {
  display: flex; align-items: center; gap: var(--sp-2); margin-left: var(--sp-2);
  background: var(--chrome-warn-bg); border: 1px solid var(--chrome-warn-line);
  border-radius: var(--r-m); padding: 2px 3px 2px 10px;
}
#unsaved-bar.hidden, #unlinked-bar.hidden { display: none; }
#unsaved-msg, #unlinked-msg {
  display: inline-flex; align-items: center; gap: 5px;
  font-size: var(--fs-sm); color: var(--chrome-warn-text); white-space: nowrap;
}
#unsaved-msg svg, #unlinked-msg svg { display: block; }
#btn-download-app, #btn-link-app {
  height: 24px; background: var(--chrome-warn-btn); color: var(--on-warn); border-radius: var(--r-s);
  padding: 0 10px; font-size: var(--fs-sm); font-weight: 600;
}
#btn-download-app:hover, #btn-link-app:hover { background: var(--chrome-warn-btn-hover); color: var(--on-warn); }
/* Secondary action in the same banner: a new app is also the moment someone
   wants to open one they already have (see #btn-open-app in wire()). Kept
   quieter than Save As so the primary action still reads first. */
#btn-open-app {
  height: 24px; border: 1px solid var(--chrome-warn-btn); color: var(--chrome-warn-text);
  border-radius: var(--r-s); padding: 0 var(--sp-2); font-size: var(--fs-sm); font-weight: 600;
}
#btn-open-app:hover { background: color-mix(in srgb, var(--chrome-warn-btn) 22%, transparent); color: var(--chrome-text); }
#btn-open-app.hidden { display: none; }
/* The banners never shrink; the title ellipsizes instead. */
#unsaved-bar, #unlinked-bar { flex-shrink: 0; }

/* --- collapsed menu (phones, skinny browser windows) --- */
/* The full row of panel/window buttons doesn't fit next to the title and the
   "Not saved" banner, so on narrow screens they collapse into one ⋮ menu.
   Chat stays out: it is what most people open the bar for. The banner keeps
   its full text, the freed space is what pays for it. */
#btn-menu { display: none; }
@media (max-width: 640px) {
  shell-topbar .bar-collapse { display: none !important; }
  #btn-menu { display: inline-flex; }
  #btn-rename { display: none; } /* rename stays reachable via double-tap */
  /* Three chips + the title don't fit a phone-width bar: the open button
     keeps its icon (and its tooltip) and drops the words. */
  #btn-open-app .bar-label { display: none; }
}
@media (max-width: 480px) { shell-topbar { padding: 0 var(--sp-1) 0 var(--sp-2); } #app-icon { display: none; } }
@media (pointer: coarse) {
  #btn-download-app, #btn-link-app, #btn-open-app { height: 32px; padding: 0 var(--sp-3); }
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
    if (b.dataset.saveas !== "1" && b.dataset.state !== "saved") {
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
    if (e.key === "Enter" && !isComposing(e)) { e.preventDefault(); finish(true); }
    if (e.key === "Escape") { e.preventDefault(); finish(false); }
  };
  titleEl.onblur = () => finish(true);
}

// The ⋮ menu holding the buttons that don't fit on narrow screens. Items
// just click their (hidden) source button, so every action and its enabled/
// active logic lives in exactly one place. Chat is not in it: it stays on
// the bar at every width.
function openBarMenu() {
  const native = document.body.classList.contains("native");
  const panel = (src, icon, label) =>
    ({ icon, label, checked: $(src).classList.contains("active"), run: () => $(src).click() });
  menuFrom($("btn-menu"), [
    panel("btn-files", "files", S.topbar.menuFiles),
    panel("btn-sql", "database", S.topbar.menuDatabase),
    panel("btn-tools", "wrench", S.topbar.menuTools),
    panel("btn-settings", "settings", S.topbar.menuSettings),
    { sep: true },
    { icon: "eye-off", label: S.toolbar.menuHide, run: () => $("btn-hidebar").click() },
    { icon: "rotate-cw", label: S.topbar.reloadApp, run: () => $("btn-reload").click() },
    ...(native ? [] : [{ icon: "x", label: S.topbar.closeApp, run: () => $("btn-quit").click() }]),
  ], S.topbar.menuBtn);
}

// Panel buttons: icon only (named for assistive tech), Chat with a label on
// wide screens.
function panelButton(id, icon, title, label) {
  const b = $(id);
  b.innerHTML = I(icon, 18) + (label ? `<span class="bar-label">${label}</span>` : "");
  b.title = title;
  b.setAttribute("aria-label", label || title);
}

function wire() {
  panelButton("btn-files", "files", S.topbar.browseFiles);
  panelButton("btn-sql", "database", S.topbar.browseDatabase);
  panelButton("btn-tools", "wrench", S.topbar.toolsBtn);
  panelButton("btn-settings", "settings", S.topbar.settingsBtn);
  panelButton("btn-chat", "message-square", S.topbar.chatBtn, S.topbar.chatLabel);
  $("btn-menu").innerHTML = I("ellipsis-vertical", 18);
  $("btn-menu").setAttribute("aria-haspopup", "menu");
  $("btn-menu").setAttribute("aria-expanded", "false");
  $("btn-menu").onclick = openBarMenu;
  $("btn-reload").innerHTML = I("rotate-cw", 18);
  $("btn-hidebar").innerHTML = I("eye-off", 18);
  $("btn-quit").innerHTML = I("x", 18);
  $("btn-rename").innerHTML = I("pencil", 12);
  // The app's icon: the window's own. A missing one just leaves the name.
  const icon = $("app-icon");
  icon.onerror = () => icon.remove();
  icon.src = (window.__uappBase || "/") + "icon-256.png";
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

  const focus = { focus: true };
  $("btn-chat").onclick = () => togglePanel("sidebar", focus);
  $("btn-sql").onclick = () => togglePanel("sqlpanel", focus);
  $("btn-files").onclick = () => togglePanel("filebrowser", focus);
  $("btn-settings").onclick = () => togglePanel("settingspanel", focus);
  $("btn-tools").onclick = () => togglePanel("toolspanel", focus);
  $("btn-reload").onclick = () => emit("reload-app");
  // Hiding leaves the app filling the window. The tooltip names the keystroke
  // that brings it back — that is the only place most people will ever read
  // it, so it is kept in step with the setting.
  $("btn-hidebar").onclick = () => toggleToolbar();
  const hideTitle = () => {
    const keys = shortcutLabel(toolbarDefault().shortcut);
    $("btn-hidebar").title = S.toolbar.hideTitle + (keys ? ` (${keys})` : "");
    $("btn-hidebar").setAttribute("aria-label", S.toolbar.hideTitle);
  };
  hideTitle();
  on("toolbar-state", hideTitle);
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
        btn.dataset.state = "saved";
        setTimeout(() => { btn.innerHTML = orig; delete btn.dataset.state; }, 1800);
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
  $("win-max").innerHTML = I("square", 14);
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
