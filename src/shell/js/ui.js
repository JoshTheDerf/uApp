/* uapp shell — shared UI toolkit: escaping/formatting helpers, the icon
 * accessor, context menus, the dialog skeleton (alert/confirm/prompt), the
 * overlay-panel manager, and per-component <style> installation. */

import { HL_BY_EXT } from "./markdown.js";
import { S } from "./strings.js";

export const $ = (id) => document.getElementById(id);
export const I = (name, size) => window.uappIcon(name, size);
export const div = (cls) => { const d = document.createElement("div"); d.className = cls; return d; };

// Escape-FIRST discipline: source text is HTML-escaped before any markup is
// added; only markup this code generates ever reaches innerHTML. The shell
// origin is privileged (token cookie, no CSP), so app/model-supplied text
// must never pass raw HTML through.
export function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}
export const clip = (s, n) => (s.length > n ? s.slice(0, n) + "…" : s);
export const pretty = (v) => {
  if (typeof v === "string") { try { v = JSON.parse(v); } catch { return v; } }
  return JSON.stringify(v, null, 2);
};
export const fmtSize = (n) => (n >= 1048576 ? (n / 1048576).toFixed(1) + " MB"
  : n >= 1024 ? (n / 1024).toFixed(1) + " KB" : n + " B");
export const fmtTime = (t) => new Date(t).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });

/// HTTP path for an archive file (per-segment encoding; the cookie authorizes).
// Base prefix for archive URLs: "/" natively; the wasm demo build may be
// hosted under a subpath (boot.js sets window.__uappBase).
export const appUrl = (name) => (window.__uappBase || "/") + "app/" + String(name).split("/").map(encodeURIComponent).join("/");
/// Navigate to a Content-Disposition route so the browser saves the file.
/// Android's WebView drops <a download> navigations, and the system
/// DownloadManager (the previous fallback) runs in a separate process where
/// neither the session cookie nor cleartext-to-loopback is guaranteed — and
/// without a public destination its files land in an invisible cache. So on
/// Android the shell fetches the bytes itself (same-origin, cookie-authed)
/// and hands them to the saveBlob bridge, which writes into the system
/// Downloads folder and shows a toast.
export function downloadUrl(href) {
  // Browser (wasm) demo build, possibly hosted under a subpath: an <a download>
  // is matched against the service worker's SCOPE (not the controlling page),
  // so a root-absolute href would bypass the SW and download the host site's
  // fallback page instead of the archive. Fetch it (page fetches always go
  // through the SW) and save the blob.
  if (window.__uappBase && href.startsWith("/")) {
    (async () => {
      let r;
      try { r = await fetch(window.__uappBase + href.slice(1)); } catch (e) { dlgAlert(S.ui.downloadFailed(String(e.message || e))); return; }
      if (!r.ok) { dlgAlert(S.ui.downloadFailed("HTTP " + r.status)); return; }
      const dispo = r.headers.get("content-disposition") || "";
      const name = (dispo.match(/filename="([^"]+)"/) || [])[1] ||
        href.split("/").pop().replace(/\?.*$/, "");
      const a = document.createElement("a");
      a.href = URL.createObjectURL(await r.blob());
      a.download = name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      // Revoke later: an immediate revoke can race the browser's save.
      setTimeout(() => URL.revokeObjectURL(a.href), 60000);
    })();
    return;
  }
  if (typeof window.UAppAndroid?.saveBlob === "function") {
    fetchToDownloads(href);
    return;
  }
  if (window.UAppAndroid) {
    // Older APK without saveBlob: the DownloadManager path needs the auth
    // token on the URL (it fetches outside the WebView's cookie jar).
    const tok = new URLSearchParams(location.search).get("t") ||
      document.cookie.split(";").map((s) => s.trim())
        .find((s) => s.startsWith(`uapp_t_${location.port}=`))?.slice(`uapp_t_${location.port}=`.length);
    if (tok && !/[?&]t=/.test(href)) {
      href += (href.includes("?") ? "&" : "?") + "t=" + encodeURIComponent(tok);
    }
  }
  const a = document.createElement("a");
  a.href = href;
  a.download = "";
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/// Base64 for the JS bridge (chunked: String.fromCharCode(...whole) overflows
/// the arg limit on big files).
export function bytesToB64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

async function fetchToDownloads(href) {
  // Shown only once the download has taken over a second (fast ones never
  // flash it); indeterminate until the first byte count arrives.
  const t = toast(S.ui.downloading, { duration: 0, progress: true, delay: 1000 });
  try {
    const r = await fetch(href);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const cd = r.headers.get("content-disposition") || "";
    const name = /filename="?([^";]+)"?/i.exec(cd)?.[1] ||
      decodeURIComponent(href.split("?")[0].split("/").pop() || "") || "download";
    const mime = r.headers.get("content-type") || "application/octet-stream";
    t.update(S.ui.downloadingName(name));
    let bytes;
    const total = +r.headers.get("content-length") || 0;
    if (r.body?.getReader) {
      const reader = r.body.getReader();
      const chunks = [];
      let got = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        got += value.length;
        if (total) t.progress(got / total);
      }
      bytes = new Uint8Array(got);
      let off = 0;
      for (const c of chunks) { bytes.set(c, off); off += c.length; }
    } else {
      bytes = new Uint8Array(await r.arrayBuffer());
    }
    window.UAppAndroid.saveBlob(name, mime, bytesToB64(bytes));
  } catch (e) {
    dlgAlert(S.ui.downloadFailed(e.message));
  } finally {
    t.dismiss();
  }
}

// ---- toasts ----
// Transient bottom-center notifications, stacked. toast(msg) shows one for a
// few seconds; the returned handle allows updating in place. Options:
//   duration — ms before auto-dismiss (0 = sticky until dismiss())
//   progress — include a progress bar (indeterminate until .progress(frac))
//   delay    — ms before the toast appears at all (dismiss() cancels it),
//              for operations that are usually too fast to be worth showing
export function toast(msg, { duration = 3500, progress = false, delay = 0 } = {}) {
  installStyle("shell-toast", /* css */ `
    #toast-wrap {
      position: fixed; left: 50%; bottom: 26px; transform: translateX(-50%);
      z-index: 4000; display: flex; flex-direction: column-reverse; gap: 8px;
      align-items: center; pointer-events: none;
    }
    .toast {
      display: flex; flex-direction: column; gap: 6px;
      background: var(--ink); color: var(--chrome-text);
      border-radius: 10px; padding: 10px 14px; min-width: 220px; max-width: 80vw;
      box-shadow: 0 4px 18px rgba(0,0,0,.35); font-size: 12px;
    }
    .toast-msg { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .toast-track {
      height: 4px; border-radius: 2px; background: rgba(255,255,255,.18); overflow: hidden;
    }
    .toast-fill {
      height: 100%; width: 0; border-radius: 2px; background: var(--brand, #6ea8fe);
      transition: width .15s linear;
    }
    .toast.indet .toast-fill { width: 35%; animation: toast-slide 1.1s ease-in-out infinite; }
    @keyframes toast-slide {
      0% { margin-left: 0; } 50% { margin-left: 65%; } 100% { margin-left: 0; }
    }
  `);
  let el = null;
  let hideTimer = null;
  const show = () => {
    let wrap = $("toast-wrap");
    if (!wrap) {
      wrap = document.createElement("div");
      wrap.id = "toast-wrap";
      document.body.appendChild(wrap);
    }
    el = div("toast" + (progress ? " indet" : ""));
    el.innerHTML = `<span class="toast-msg"></span>` +
      (progress ? `<div class="toast-track"><div class="toast-fill"></div></div>` : "");
    el.querySelector(".toast-msg").textContent = msg;
    wrap.appendChild(el);
    if (duration) hideTimer = setTimeout(dismiss, duration);
  };
  const showTimer = delay ? setTimeout(show, delay) : (show(), null);
  function dismiss() {
    clearTimeout(showTimer);
    clearTimeout(hideTimer);
    el?.remove();
    el = null;
  }
  return {
    update(m) {
      msg = m;
      el?.querySelector(".toast-msg")?.replaceChildren(document.createTextNode(m));
    },
    progress(frac) {
      if (!el) return;
      el.classList.remove("indet");
      el.querySelector(".toast-fill").style.width = (Math.min(1, frac) * 100).toFixed(1) + "%";
    },
    dismiss,
  };
}

export function copyText(s) {
  const fallback = () => {
    const ta = document.createElement("textarea");
    ta.value = s;
    ta.style.cssText = "position:fixed;opacity:0";
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand("copy"); } catch {}
    ta.remove();
  };
  try { navigator.clipboard.writeText(s).catch(fallback); } catch { fallback(); }
}

/// Install a component's stylesheet once (each component bundles its own CSS).
export function installStyle(key, css) {
  if (document.querySelector(`style[data-for="${key}"]`)) return;
  const s = document.createElement("style");
  s.dataset.for = key;
  s.textContent = css;
  document.head.appendChild(s);
}

// ---------- file kinds (shared by files panel, viewer, chat attachments) ----------
export const extOf = (n) => (n.includes(".") ? n.split(".").pop().toLowerCase() : "");
const KIND_EXT = {
  image: ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "ico", "bmp"],
  video: ["mp4", "webm", "ogv", "mov", "m4v", "mkv"],
  audio: ["mp3", "wav", "m4a", "flac", "ogg", "oga", "opus"],
  pdf: ["pdf"],
  md: ["md", "markdown"],
};
export const KIND_ICON = {
  image: "image", video: "film", audio: "music", pdf: "file-type",
  md: "file-text", code: "file-code", text: "file-text", bin: "file-question",
};
export function fileKind(name) {
  const e = extOf(name);
  for (const k of Object.keys(KIND_EXT)) if (KIND_EXT[k].includes(e)) return k;
  if (HL_BY_EXT[e]) return "code";
  if (["", "txt", "text", "log", "csv", "tsv", "diff", "patch", "lock"].includes(e)) return "text";
  return "bin";
}

// ---------- tool metadata (chat tool cards + the tools dialog) ----------
export function toolMeta(name, input = {}) {
  if (name.startsWith("app__")) {
    return { icon: "puzzle", title: name.slice(5), detail: JSON.stringify(input) };
  }
  if (name.startsWith("mcp__")) {
    const [, srv, tool] = name.split("__");
    return { icon: "plug-zap", title: `${srv}: ${tool}`, detail: "" };
  }
  switch (name) {
    case "sql_query": return { icon: "search", title: S.toolMeta.query, detail: input.sql || "" };
    case "sql_exec": return { icon: "pencil-line", title: S.toolMeta.sqlWrite, detail: input.sql || "" };
    case "sql_batch": return { icon: "package", title: S.toolMeta.sqlBatch, detail: input.sql || "" };
    case "get_schema": return { icon: "compass", title: S.toolMeta.inspectSchema, detail: "" };
    case "list_files": return { icon: "files", title: S.toolMeta.listFiles, detail: "" };
    case "read_file": return { icon: "book-open", title: S.toolMeta.read, detail: input.name || "" };
    case "write_file": return { icon: "file-pen", title: S.toolMeta.write, detail: input.name || "" };
    case "delete_file": return { icon: "trash-2", title: S.toolMeta.deleteFile, detail: input.name || "" };
    case "agent_run": return { icon: "bot", title: S.toolMeta.subAgent(input.description || ""), detail: input.mode === "fork" ? S.toolMeta.forkedContext : "" };
    case "agent_send": return { icon: "bot", title: S.toolMeta.subAgentFollowUp, detail: input.message || "" };
    case "present_file": return { icon: "eye", title: S.toolMeta.presented(input.name || ""), detail: input.mode || "" };
    case "ask_user": {
      const qs = input.questions || [];
      return { icon: "circle-help", title: qs.length > 1 ? S.toolMeta.askedMany(qs.length) : S.toolMeta.askedOne,
               detail: (qs[0] && qs[0].question) || "" };
    }
    case "run_js": return { icon: "square-terminal", title: input.context === "app" ? S.toolMeta.runJsApp : S.toolMeta.runJsScratch, detail: input.code || "" };
    case "web_search": return { icon: "globe", title: S.toolMeta.webSearch, detail: input.query || "" };
    case "fetch_url": return { icon: "globe", title: S.toolMeta.fetchPage, detail: input.url || "" };
    case "add_mcp_server": return { icon: "server", title: S.toolMeta.connectMcp, detail: `${input.name || ""} ${input.url || ""}` };
    case "remove_mcp_server": return { icon: "server", title: S.toolMeta.disconnectMcp, detail: input.name || "" };
    case "download_lib": return { icon: "download", title: S.toolMeta.downloadLib, detail: `${input.url || ""}${input.name ? " → " + input.name : ""}` };
    case "import_csv": return { icon: "chart-column", title: S.toolMeta.importData, detail: `${input.file || ""}${input.table ? " → " + input.table : ""}` };
    default: return { icon: "wrench", title: name, detail: "" };
  }
}

// ---------- context menus ----------
/// items: {icon, label, run, danger} or {sep: true}.
export function showMenu(items, x, y) {
  document.querySelectorAll(".ctxmenu").forEach((m) => m.remove());
  // Menus are assembled conditionally: drop leading/trailing/doubled separators.
  items = items.filter((it, i) => !it.sep || (i > 0 && !items[i - 1].sep));
  while (items.length && items[items.length - 1].sep) items.pop();
  const menu = div("ctxmenu");
  // One cleanup path no matter how the menu goes away (item click, click
  // outside, Esc, or removal by other code) — a stale capture listener would
  // otherwise eat the next Escape.
  const cleanup = () => {
    menu.remove();
    document.removeEventListener("mousedown", away, true);
    document.removeEventListener("keydown", esckey, true);
  };
  const away = (ev) => {
    if (!menu.isConnected) return cleanup();
    if (ev.target && menu.contains(ev.target)) return;
    cleanup();
  };
  const esckey = (ev) => {
    if (!menu.isConnected) return cleanup();
    if (ev.key === "Escape") { ev.preventDefault(); cleanup(); }
  };
  for (const it of items) {
    if (it.sep) { menu.appendChild(div("ctxsep")); continue; }
    const b = document.createElement("button");
    if (it.danger) b.className = "danger";
    b.innerHTML = `<span class="ticon">${I(it.icon, 13)}</span>${esc(it.label)}`;
    b.onclick = () => { cleanup(); it.run(); };
    menu.appendChild(b);
  }
  document.body.appendChild(menu);
  menu.style.left = Math.max(6, Math.min(x, innerWidth - menu.offsetWidth - 8)) + "px";
  menu.style.top = Math.max(6, Math.min(y, innerHeight - menu.offsetHeight - 8)) + "px";
  setTimeout(() => {
    document.addEventListener("mousedown", away, true);
    document.addEventListener("keydown", esckey, true);
  }, 0);
}

// ---------- dialogs ----------
// Every transient dialog is a native <dialog> with the same three parts:
// .dlg-head (icon + title + X), .dlg-body, .dlg-foot. Esc, focus trapping
// and the backdrop come free. (window.alert()/confirm() stay banned — they
// are blocked or invisible in the frameless desktop webview.)
export function makeDialog(title, icon = "circle-alert") {
  const d = document.createElement("dialog");
  d.className = "dlg dlg-s";
  d.innerHTML = `<div class="dlg-head"><h2><span class="dlg-hicon"></span><span class="dlg-title"></span></h2>` +
    `<button class="icon-btn dlg-x" title="${esc(S.common.close)}"></button></div>` +
    `<div class="dlg-body"></div><div class="dlg-foot"></div>`;
  d.querySelector(".dlg-title").textContent = title;
  d.querySelector(".dlg-hicon").innerHTML = I(icon, 15);
  d.querySelector(".dlg-x").innerHTML = I("x", 15);
  d.querySelector(".dlg-x").onclick = () => d.close();
  d.addEventListener("close", () => d.remove());
  document.body.appendChild(d);
  return d;
}
export function footBtn(d, label, opts = {}) {
  const b = document.createElement("button");
  b.className = "btn" + (opts.primary ? " primary" : "") + (opts.danger ? " danger" : "");
  b.textContent = label;
  d.querySelector(".dlg-foot").appendChild(b);
  return b;
}
const shellTitle = () => (document.title.split(" — ")[0] || "uapp").trim();
export function shellPrompt(message, buttons, opts = {}) {
  return new Promise((resolve) => {
    const d = makeDialog(opts.title || shellTitle(), opts.icon);
    const p = document.createElement("p");
    p.className = "dlg-msg";
    p.textContent = message;
    d.querySelector(".dlg-body").appendChild(p);
    let result = null; // Esc / X leave it null
    for (const b of buttons) {
      footBtn(d, b.label, b).onclick = () => { result = b.value; d.close(); };
    }
    d.addEventListener("close", () => resolve(result));
    d.showModal();
    // Focus the primary action, not the X: Enter then confirms.
    (d.querySelector(".dlg-foot .btn.primary") || d.querySelector(".dlg-foot .btn"))?.focus();
  });
}
export const dlgAlert = (msg) => shellPrompt(msg, [{ label: S.common.ok, value: "1", primary: true }]);
export const dlgConfirm = async (msg) =>
  (await shellPrompt(msg, [{ label: S.common.cancel, value: "0" }, { label: S.common.ok, value: "1", primary: true }],
    { icon: "circle-help" })) === "1";
/// Which of the archive's two roots something belongs in. Resolves "app/",
/// "data/" or null (cancelled).
export const dlgPickRoot = (msg) =>
  shellPrompt(msg, [
    { label: "Cancel", value: "" },
    { label: "App files", value: "app/" },
    { label: "Data files", value: "data/", primary: true },
  ], { icon: "folder-open" }).then((v) => v || null);
/// One-line text prompt (rename, new file). Resolves null when cancelled.
export function dlgInput(message, value = "", okLabel = "OK") {
  return new Promise((resolve) => {
    const d = makeDialog(shellTitle(), "pencil");
    const p = document.createElement("p");
    p.className = "dlg-msg";
    p.textContent = message;
    const inp = document.createElement("input");
    inp.className = "dlg-input";
    inp.value = value;
    inp.autocomplete = "off";
    d.querySelector(".dlg-body").append(p, inp);
    let result = null;
    footBtn(d, S.common.cancel).onclick = () => d.close();
    const ok = footBtn(d, okLabel, { primary: true });
    ok.onclick = () => { result = inp.value; d.close(); };
    inp.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); ok.click(); } });
    d.addEventListener("close", () => resolve(result));
    d.showModal();
    inp.focus();
    inp.select();
  });
}
/// Info dialog with pre-built (already escaped) HTML body and a Close button.
export function dlgHtml(title, html, icon) {
  const d = makeDialog(title, icon);
  d.querySelector(".dlg-body").innerHTML = html;
  footBtn(d, S.common.close, { primary: true }).onclick = () => d.close();
  d.showModal();
}

/// Show/hide toggle for a .pwfield (an input + eye button pair).
export function wirePwToggle(wrap) {
  const i = wrap.querySelector("input"), t = wrap.querySelector(".pw-toggle");
  t.innerHTML = I("eye", 14);
  t.onclick = () => {
    i.type = i.type === "password" ? "text" : "password";
    t.innerHTML = I(i.type === "password" ? "eye" : "eye-off", 14);
  };
}

// ---------- overlay panels ----------
// One paradigm for all: the topbar button toggles the panel, the X or Esc
// closes it, and opening one closes the others (they share an edge).
// Panels register themselves; the topbar only knows their ids.
// Open state is the `.open` class (a transform slide, so it animates) —
// never display:none, which can't transition.
const panels = {}; // id -> {btn, onOpen}
export function registerPanel(id, btnId, onOpen) {
  panels[id] = { btn: btnId, onOpen };
  installResizer($(id));
}
/// Every overlay panel is drag-resizable from its left edge. The width is
/// remembered per panel on this machine; double-click resets to the default.
function installResizer(el) {
  if (!el || el.querySelector(":scope > .panel-resizer")) return;
  const grip = document.createElement("div");
  grip.className = "panel-resizer";
  grip.title = S.ui.resizerTitle;
  el.prepend(grip);
  const KEY = `uapp.panelw.${el.id}`;
  const saved = parseInt(localStorage.getItem(KEY), 10);
  // maxWidth is lifted with the inline width: the component defaults cap at
  // e.g. 55vw, but a deliberate drag may go wider.
  if (saved) { el.style.width = saved + "px"; el.style.maxWidth = "96vw"; }
  grip.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const startX = e.clientX;
    const startW = el.getBoundingClientRect().width;
    try { grip.setPointerCapture(e.pointerId); } catch {}
    el.classList.add("resizing");                  // no slide transition mid-drag
    document.body.classList.add("panel-resizing"); // iframes must not eat the drag
    const move = (ev) => {
      const w = Math.round(Math.min(Math.max(startW + (startX - ev.clientX), 280), innerWidth - 48));
      el.style.width = w + "px";
      el.style.maxWidth = "96vw";
    };
    const up = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", up);
      grip.removeEventListener("pointercancel", up);
      el.classList.remove("resizing");
      document.body.classList.remove("panel-resizing");
      const w = parseInt(el.style.width, 10);
      if (w) localStorage.setItem(KEY, String(w));
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up);
    grip.addEventListener("pointercancel", up);
  });
  grip.addEventListener("dblclick", () => {
    el.style.width = "";
    el.style.maxWidth = "";
    localStorage.removeItem(KEY);
  });
}
export const panelOpen = (id) => $(id).classList.contains("open");
export function openPanel(id) {
  for (const other in panels) if (other !== id) closePanel(other);
  if (panelOpen(id)) return;
  $(id).classList.add("open");
  $(panels[id].btn).classList.add("active");
  panels[id].onOpen?.();
}
export function closePanel(id) {
  if (!panelOpen(id)) return;
  $(id).classList.remove("open");
  $(panels[id].btn).classList.remove("active");
}
export const togglePanel = (id) => (panelOpen(id) ? closePanel(id) : openPanel(id));
// Esc closes the open panel — unless a menu, a dialog, or an inner editor
// (cell edit, rename, search) already handled the key.
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || e.defaultPrevented) return;
  if (document.querySelector(".ctxmenu") || document.querySelector("dialog[open]")) return;
  for (const id in panels) {
    if (panelOpen(id)) { e.preventDefault(); closePanel(id); return; }
  }
});
