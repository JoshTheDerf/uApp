/* Showing and hiding the toolbar.
 *
 * "The toolbar" is what a person calls the bar this shell puts around the app:
 * the top bar (topbar.js) and the panels its buttons open. Hidden, the app
 * fills the window and looks like an app rather than something being built —
 * which is what you want when you are using it, demoing it, or handing it to
 * someone else.
 *
 * Two things, kept apart on purpose (toolbar.rs makes the same split):
 *
 *   the saved default — `hidden` + the toggle `shortcut`, in the .uapp's own
 *                       config, so it travels with the file and every open
 *                       starts from it.
 *   visible right now — memory only, written NOWHERE. Revealing a hidden
 *                       toolbar to change one thing must not quietly decide
 *                       what the app looks like next time it is opened, so
 *                       there is no code here that persists it.
 *
 * Never trap anyone. Hidden, the ways back are the keyboard shortcut — which a
 * phone does not have, and which the app's own page could be holding focus for
 * — and the reveal handle. So the handle is always there, on every platform,
 * no matter who hid the bar (a keystroke, the app, the assistant).
 *
 * One consequence worth knowing in the native desktop window: the bar is also
 * that frameless window's title bar, so hiding it takes the window controls and
 * the drag area with it. That is the point — a shared app should be able to
 * look like an app — and the handle brings all of it back in one click, which
 * is why it sits exactly where the window controls were.
 */

import { rpc, on, emit } from "./core.js";
import { $, I, installStyle, closePanels, openPanel, closePanel, panelOpen } from "./ui.js";
import { S } from "./strings.js";

// Cmd on macOS, Ctrl everywhere else — what "Mod" in a shortcut means, so one
// stored string reads right on whichever machine opens the app. Exported so
// the settings panel's shortcut recorder decides the same way this matcher
// does; two copies of this test would eventually disagree.
export const MAC = /mac|iphone|ipad|ipod/i.test(navigator.platform || navigator.userAgent || "");

// The bar starts out however the page was served: the server writes
// class="hidden" onto <shell-topbar> for an app whose default is hidden (and
// boot.js does the same in the browser build) precisely so it never paints and
// then vanishes. Read that back rather than assuming, or the first apply()
// would undo it.
let visible = !$("topbar")?.classList.contains("hidden");
let def = { hidden: !visible, shortcut: "" };
let combos = [];
let booted = false;

export const toolbarVisible = () => visible;
export const toolbarDefault = () => ({ ...def });

/// The shortcut as a person should read it: "Mod" is an abstraction for us,
/// not for them.
export function shortcutLabel(spec = def.shortcut) {
  return String(spec || "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => s.replace(/\bMod\b/g, MAC ? "⌘" : "Ctrl").replace(/\bMeta\b/g, MAC ? "⌘" : "Meta")
      .replace(/\bAlt\b/g, MAC ? "⌥" : "Alt"))
    .join(" / ");
}

// ---- the reveal handle ----------------------------------------------------
// Small and faint so it does not compete with the app's own UI, but always
// hittable: a phone has no keyboard, so on a touch screen this is the ONLY way
// back and it gets a finger-sized target.
const CSS = /* css */ `
#toolbar-reveal {
  position: absolute; top: 0; right: 0; z-index: 40;
  width: 32px; height: 26px; padding: 0;
  display: flex; align-items: center; justify-content: center;
  border: none; border-radius: 0 0 0 var(--r-m);
  background: var(--ink); color: var(--chrome-text);
  opacity: .3; cursor: pointer; transition: opacity .15s;
}
#toolbar-reveal:hover, #toolbar-reveal:focus-visible { opacity: 1; }
#toolbar-reveal[hidden] { display: none; }
@media (pointer: coarse) { #toolbar-reveal { width: 44px; height: 34px; opacity: .45; } }
`;

let handleEl = null;
function handle() {
  if (handleEl?.isConnected) return handleEl;
  // Inside #main, not the body: hosted-site mode strips the body down to the
  // shell root when it takes the page over (site-chrome.js reveal()), and a
  // handle parented to the body would go with it.
  const host = $("main");
  if (!host) return null;
  installStyle("toolbar-reveal", CSS);
  handleEl = document.createElement("button");
  handleEl.id = "toolbar-reveal";
  handleEl.type = "button";
  handleEl.innerHTML = I("eye", 15);
  handleEl.setAttribute("aria-label", S.toolbar.showTitle); // the icon is all there is
  handleEl.onclick = () => setToolbar(true);
  host.appendChild(handleEl);
  return handleEl;
}

// ---- state ---------------------------------------------------------------
function apply() {
  $("topbar")?.classList.toggle("hidden", !visible);
  // Every panel was opened from a button on the bar; one left on screen with
  // no bar to close it from is a dead end.
  if (!visible) closePanels();
  const h = handle();
  if (h) {
    h.hidden = visible;
    h.title = S.toolbar.showTitle + hint();
  }
  emit("toolbar-state", { visible, ...def });
}

const hint = () => (def.shortcut ? ` (${shortcutLabel()})` : "");

/// Show or hide the toolbar for this session. Nothing is saved — see the file
/// header; that is the point.
export function setToolbar(next) {
  next = !!next;
  if (next === visible) return visible;
  visible = next;
  apply();
  return visible;
}
export const toggleToolbar = () => setToolbar(!visible);

// ---- the panels ----------------------------------------------------------
// The toolbar's other half: its buttons open these, and hiding it closes them.
// Named for what they are, not for the element ids they grew — the core uses
// the same names (toolbar.rs), so a tool call and a click mean one thing.
const PANEL_IDS = {
  chat: "sidebar",
  files: "filebrowser",
  database: "sqlpanel",
  settings: "settingspanel",
  tools: "toolspanel",
};

/// Which panel is open, by name (null if none) — panels share an edge, so it
/// is always at most one.
export const openPanelName = () =>
  Object.keys(PANEL_IDS).find((n) => $(PANEL_IDS[n]) && panelOpen(PANEL_IDS[n])) || null;

/// Open or close one panel. `open` omitted toggles. Returns whether it ended
/// up open, or null for a name this shell does not have.
///
/// Opening reveals the toolbar first: the panels hang off it and hiding it
/// closes them, so a panel on screen with no bar above it is a state nothing
/// else here can produce, and the buttons that would close it again live on
/// the bar.
export function setPanel(name, open) {
  const id = PANEL_IDS[name];
  if (!id || !$(id)) return null;
  const want = open === undefined || open === null ? !panelOpen(id) : !!open;
  if (want) {
    setToolbar(true);
    openPanel(id); // closes whichever other panel was open
  } else {
    closePanel(id);
  }
  return want;
}

// ---- the keyboard shortcut ------------------------------------------------
function parseCombo(one) {
  const parts = one.split("+").map((x) => x.trim());
  // A shortcut whose key IS "+": "Mod++" splits to ["Mod", "", ""].
  if (parts.length > 1 && parts[parts.length - 1] === "") {
    parts.pop();
    parts[parts.length - 1] = "+";
  }
  const key = (parts.pop() || "").toLowerCase();
  if (!key) return null;
  const c = { key: key === "space" ? " " : key, ctrl: false, alt: false, shift: false, meta: false };
  for (const p of parts) {
    const m = p.toLowerCase();
    if (m === "mod") c[MAC ? "meta" : "ctrl"] = true;
    else if (m === "ctrl" || m === "control") c.ctrl = true;
    else if (m === "alt" || m === "option" || m === "opt") c.alt = true;
    else if (m === "shift") c.shift = true;
    else if (m === "meta" || m === "cmd" || m === "command" || m === "super" || m === "win") c.meta = true;
    else return null; // the core validates these; an unknown one just never fires
  }
  return c;
}

function keysOf(e) {
  const out = [];
  if (e.key) out.push(e.key.toLowerCase());
  // Alt rewrites e.key on several layouts (macOS Alt+B is "∫", so is AltGr on
  // Windows), which would leave every Alt combo dead. Match the physical key
  // as well.
  const m = /^Key([A-Z])$/.exec(e.code || "") || /^Digit(\d)$/.exec(e.code || "");
  if (m) out.push(m[1].toLowerCase());
  else if (/^F\d{1,2}$/.test(e.code || "")) out.push(e.code.toLowerCase());
  return out;
}

function onKey(e) {
  // defaultPrevented: the shortcut recorder in Settings is capturing keys, or
  // an editor already claimed this one.
  if (e.defaultPrevented || !combos.length) return;
  const keys = keysOf(e);
  const hit = combos.some((c) => e.ctrlKey === c.ctrl && e.altKey === c.alt
    && e.shiftKey === c.shift && e.metaKey === c.meta && keys.includes(c.key));
  if (!hit) return;
  e.preventDefault();
  toggleToolbar();
}

function bindShortcut(spec) {
  combos = String(spec || "").split(",").map((s) => s.trim()).filter(Boolean)
    .map(parseCombo).filter(Boolean);
}

// Key events inside the app frame never reach this document, and the frame is
// where focus almost always is — without this the shortcut would only work in
// the moments nobody is using the app.
function bindFrame() {
  const f = $("appframe");
  if (!f) return;
  const attach = () => {
    let d = null;
    try { d = f.contentDocument; } catch { return; } // a cross-origin page: nothing to do
    if (!d || d.__uappToolbarKeys) return;
    d.__uappToolbarKeys = true;
    d.addEventListener("keydown", onKey, true);
  };
  f.addEventListener("load", attach);
  attach(); // already loaded (a reconnect, or a late import)
}

// ---- app pages asking, over postMessage ----------------------------------
// uapp.toolbar.show()/hide()/toggle() from app code. Deliberately NOT an RPC:
// the archive is shared, and an app hiding the toolbar in someone else's
// window on another device would be indefensible. This reaches exactly the
// shell hosting the page that asked.
function bindPages() {
  window.addEventListener("message", (e) => {
    // Same-origin only: that means a page this shell itself served out of the
    // archive. Anything else has no business driving the shell's UI.
    if (e.origin !== location.origin) return;
    const m = e.data && e.data.__uappToolbar;
    if (!m || typeof m !== "object") return;
    if (m.action === "show") setToolbar(true);
    else if (m.action === "hide") setToolbar(false);
    else if (m.action === "toggle") toggleToolbar();
    else if (m.action === "panel") setPanel(m.panel, m.open);
    else if (m.action === "state" && e.source) {
      try {
        e.source.postMessage(
          { __uappToolbarState: { id: m.id, state: { visible, ...def, panel: openPanelName() } } },
          e.origin,
        );
      } catch {}
    }
  });
}

// ---- boot ----------------------------------------------------------------
on("toolbar", (p) => {
  // The assistant's show_toolbar, in every open shell.
  if (typeof p.visible === "boolean") setToolbar(p.visible);
  // set_toolbar_default: take the new shortcut immediately, but leave
  // visibility alone — yanking the bar away from whoever just changed the
  // setting would be startling, and reopening the app shows them the effect.
  if (p.default) {
    def = { hidden: !!p.default.hidden, shortcut: p.default.shortcut || "" };
    bindShortcut(def.shortcut);
    apply();
  }
});

// show_panel, in every open shell. `open: null` on the wire means toggle —
// only a shell knows what it currently has open.
on("panel", (p) => {
  if (p && p.panel) setPanel(p.panel, p.open === null ? undefined : p.open);
});

on("ws-open", async () => {
  let saved;
  try { saved = await rpc("toolbar.get"); } catch { return; }
  if (!saved) return;
  def = { hidden: !!saved.hidden, shortcut: saved.shortcut || "" };
  bindShortcut(def.shortcut);
  // Only the FIRST connect starts from the default. A reconnect (a resumed
  // phone, a dropped socket) is the same session: re-applying would snap the
  // bar back and lose what the person had chosen a moment ago.
  if (!booted) {
    booted = true;
    visible = !def.hidden;
  }
  apply();
});

document.addEventListener("keydown", onKey, true);
bindFrame();
bindPages();
// Right away, before any RPC: an app served with the bar already hidden must
// have its reveal handle even if the connection never comes up. Waiting for
// toolbar.get would mean a hidden bar and no way at all to bring it back.
apply();
