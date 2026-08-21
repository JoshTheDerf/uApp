/* <file-viewer> — the file dialog: images, video, audio, PDF, rendered
 * markdown, highlighted code, and the plain-text editor. Opened from the file
 * browser, from chat tool cards, or by a `present` event from the server. */

import { rpc, on } from "./core.js";
import {
  $, I, esc, div, fmtSize, appUrl, downloadUrl, installStyle,
  dlgConfirm, dlgAlert, fileKind, KIND_ICON, extOf,
} from "./ui.js";
import { mdRender, hlCode } from "./markdown.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div class="dlg-head" id="vw-head">
    <span id="vw-icon"></span>
    <div id="vw-titles">
      <span id="vw-name"></span>
      <span id="vw-meta" class="fmeta"></span>
    </div>
    <div class="spacer"></div>
    <span id="vw-badge" class="hidden"></span>
    <button id="vw-toggle" class="hidden"></button>
    <button id="vw-edit" class="hidden"></button>
    <button id="vw-save" class="hidden"></button>
    <button id="vw-dl" title="${S.viewer.downloadFile}"></button>
    <button id="vw-tab" title="${S.viewer.openNewTab}"></button>
    <button id="vw-close" class="icon-btn" title="${S.common.close}"></button>
  </div>
  <div id="vw-banner" class="hidden"></div>
  <div id="vw-body"></div>
`;

const CSS = /* css */ `
dialog#viewerdlg {
  width: min(1100px, 96vw); height: 92vh; max-width: 96vw; max-height: 92vh;
  overflow: hidden;
}
#vw-icon { display: inline-flex; color: var(--text-2); }
#vw-titles { display: flex; flex-direction: column; min-width: 0; }
#vw-name {
  font-weight: 600; font-size: 13px; overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
#vw-badge {
  font-size: 11px; background: var(--brand-soft); color: var(--brand-ink);
  border-radius: 999px; padding: 2px 9px; white-space: nowrap;
}
#vw-head { padding: 8px 10px; }
#vw-head button {
  background: none; border: 1px solid transparent; cursor: pointer; color: var(--text-2);
  display: inline-flex; align-items: center; gap: 5px; font: inherit; font-size: 12px;
  padding: 4px 8px; border-radius: 7px;
}
#vw-head button:hover { background: var(--hover-2); border-color: var(--line); }
#vw-head button svg { display: block; }
#vw-save.dirty { background: var(--brand); border-color: var(--brand); color: #fff; }
#vw-banner {
  flex: 0 0 auto; display: flex; align-items: center; gap: 8px;
  padding: 6px 10px; background: var(--warn-bg); border-bottom: 1px solid var(--warn-line);
  font-size: 12px; color: var(--warn-ink);
}
#vw-banner span { display: inline-flex; align-items: center; gap: 5px; }
#vw-banner button {
  border: 1px solid var(--warn-line); background: var(--surface); border-radius: 7px;
  padding: 2px 10px; font: inherit; font-size: 12px; cursor: pointer;
}
#vw-banner button:hover { background: var(--warn-bg); }
#vw-body { flex: 1; min-height: 0; overflow: auto; }
#vw-body.vw-media {
  display: flex; align-items: center; justify-content: center; padding: 12px;
  background: var(--ink);
}
#vw-body.vw-media.checker {
  background-color: var(--surface-2);
  background-image: linear-gradient(45deg, var(--line) 25%, transparent 25%, transparent 75%, var(--line) 75%),
    linear-gradient(45deg, var(--line) 25%, transparent 25%, transparent 75%, var(--line) 75%);
  background-size: 18px 18px; background-position: 0 0, 9px 9px;
}
#vw-body.vw-media img, #vw-body.vw-media video {
  max-width: 100%; max-height: 100%; object-fit: contain;
}
#vw-body.vw-frame { display: flex; }
#vw-body.vw-frame iframe { flex: 1; border: none; }
#vw-body.vw-empty {
  display: flex; align-items: center; justify-content: center;
  color: var(--muted); font-size: 13px; padding: 20px;
}
.vw-fallback { display: flex; flex-direction: column; align-items: center; gap: 8px; color: var(--faint); }
.vw-fallback p { margin: 0; color: var(--text-2); }
#vw-body.mdbody { padding: 20px 26px; max-width: 62rem; }
#vw-body.codebody, #vw-body.editbody { display: flex; align-items: stretch; }
.linenos {
  margin: 0; padding: 12px 8px 12px 12px; text-align: right; flex: 0 0 auto;
  background: var(--surface-2); border-right: 1px solid var(--line-soft); color: var(--ghost);
  font: 12px/1.55 var(--mono); user-select: none; overflow: hidden;
}
.codepane {
  margin: 0; padding: 12px; flex: 1; min-width: 0; overflow-x: auto;
  font: 12px/1.55 var(--mono); white-space: pre; tab-size: 2;
}
#ed-ta {
  flex: 1; min-width: 0; border: none !important; outline: none; resize: none;
  margin: 0; padding: 12px; font: 12px/1.55 var(--mono); tab-size: 2;
  border-radius: 0 !important; word-break: normal;
  white-space: pre; overflow: auto; background: var(--surface); color: var(--text);
}
/* Highlighted edit mode: a transparent textarea over a highlighted underlay.
 * The two stacks MUST share identical text metrics or the caret drifts. */
#ed-wrap {
  position: relative; flex: 1; min-width: 0; overflow: hidden;
  background: var(--surface);
}
#ed-hl {
  position: absolute; top: 0; left: 0; margin: 0; border: none;
  padding: 12px; font: 12px/1.55 var(--mono); tab-size: 2;
  white-space: pre; word-break: normal; width: max-content; min-width: 100%;
  pointer-events: none; will-change: transform; color: var(--text);
}
#ed-hl code { font: inherit; white-space: inherit; word-break: inherit; }
#ed-wrap #ed-ta {
  position: absolute; inset: 0; width: 100%; height: 100%;
  background: transparent; color: transparent; caret-color: var(--text);
}
#ed-wrap #ed-ta::selection {
  color: transparent;
  background: color-mix(in srgb, var(--brand) 30%, transparent);
}
@media (max-width: 700px) {
  dialog#viewerdlg {
    width: 100vw; max-width: 100vw; height: 100dvh; max-height: 100dvh;
    border-radius: 0;
  }
  #vw-body.mdbody { padding: 14px 16px; }
}
`;

const vw = {
  name: null, kind: null, mode: "auto", size: 0,
  text: null, dirty: false, ta: null, selfSave: 0,
};

export async function openViewer(name, mode = "auto", by = "") {
  if (vw.dirty && vw.name !== name &&
      !(await dlgConfirm(S.viewer.unsavedDiscard(vw.name)))) return;
  let size = 0;
  try {
    const files = await rpc("files.list");
    const row = files.rows.find(([n]) => n === name);
    if (row) size = row[1];
  } catch {}
  Object.assign(vw, {
    name, mode, size, kind: fileKind(name),
    text: null, dirty: false, ta: null,
  });
  const dlg = $("viewerdlg");
  if (!dlg.open) dlg.showModal();
  const badge = $("vw-badge");
  badge.classList.toggle("hidden", !by);
  badge.textContent = by ? S.viewer.presentedBy(by) : "";
  $("vw-banner").classList.add("hidden");
  await renderViewer();
}
async function closeViewer() {
  if (vw.dirty && !(await dlgConfirm(S.viewer.unsavedClose(vw.name)))) return;
  vw.dirty = false; vw.name = null; vw.ta = null; vw.text = null;
  $("viewerdlg").close();
}
/// Line-numbered, highlighted read-only view.
function codeView(text, lang) {
  const n = text.split("\n").length;
  const nums = Array.from({ length: n }, (_, i) => i + 1).join("\n");
  return `<pre class="linenos">${nums}</pre><pre class="codepane"><code>${hlCode(text, lang)}</code></pre>`;
}
async function renderViewer() {
  const { name, kind } = vw;
  const url = appUrl(name);
  const editable = kind === "md" || kind === "text" || kind === "code";
  let mode = vw.mode;
  if (mode === "auto") mode = kind === "md" ? "pretty" : "view";
  if (mode === "edit" && !editable) mode = "view";
  if ((mode === "pretty" || mode === "raw") && kind !== "md") mode = "view";
  vw.mode = mode;

  $("vw-name").textContent = name;
  $("vw-icon").innerHTML = I(KIND_ICON[kind] || "file", 16);
  markDirty();
  const tog = $("vw-toggle"), ed = $("vw-edit"), sv = $("vw-save");
  tog.classList.toggle("hidden", kind !== "md" || mode === "edit");
  tog.innerHTML = mode === "raw" ? I("eye", 13) + " " + S.viewer.pretty : I("code", 13) + " " + S.viewer.raw;
  ed.classList.toggle("hidden", !editable || mode === "edit");
  ed.innerHTML = I("file-pen", 13) + " " + S.viewer.edit;
  sv.classList.toggle("hidden", mode !== "edit");
  sv.innerHTML = I("save", 13) + " " + S.viewer.save;

  const body = $("vw-body");
  body.innerHTML = "";
  body.className = "";
  if (kind === "image") {
    body.className = "vw-media checker";
    body.innerHTML = `<img src="${url}" alt="${esc(name)}">`;
    return;
  }
  if (kind === "video") {
    body.className = "vw-media";
    body.innerHTML = `<video controls preload="metadata" src="${url}"></video>`;
    return;
  }
  if (kind === "audio") {
    body.className = "vw-media";
    body.innerHTML = `<audio controls src="${url}"></audio>`;
    return;
  }
  if (kind === "pdf") {
    body.className = "vw-frame";
    body.innerHTML = `<iframe src="${url}" title="${esc(name)}"></iframe>`;
    return;
  }
  if (kind === "bin") return viewerFallback(S.viewer.noPreview);
  // Text-ish. Big files are not worth rendering (or highlighting) in a panel.
  if (vw.size > 2 * 1024 * 1024) return viewerFallback(S.viewer.fileTooLarge);
  if (vw.text == null) {
    body.className = "vw-empty";
    body.textContent = S.common.loading;
    let text;
    try {
      const r = await fetch(url, { cache: "no-store" });
      if (!r.ok) throw new Error("HTTP " + r.status);
      text = await r.text();
    } catch (e) {
      if (vw.name !== name) return; // a present/open raced us
      return viewerFallback(S.viewer.loadFailed(e.message));
    }
    if (vw.name !== name) return; // a present/open raced us: don't poison vw.text
    vw.text = text;
    body.innerHTML = "";
  }
  if (mode === "edit") return renderEditor();
  if (kind === "md" && mode === "pretty") {
    body.className = "mdbody";
    // Relative images resolve against the file's own folder.
    body.innerHTML = mdRender(vw.text, { base: name.includes("/") ? name.replace(/[^/]+$/, "") : "" });
    return;
  }
  body.className = "codebody";
  body.innerHTML = codeView(vw.text, extOf(name));
}
function viewerFallback(msg) {
  const body = $("vw-body");
  body.className = "vw-empty";
  body.innerHTML = `<div class="vw-fallback">${I(KIND_ICON[vw.kind] || "file", 48)}` +
    `<p>${esc(msg)}</p><p class="fmeta">${esc(vw.name)} · ${fmtSize(vw.size || 0)}</p></div>`;
  const b = document.createElement("button");
  b.className = "btn primary";
  b.textContent = S.common.download;
  b.onclick = () => downloadUrl(appUrl(vw.name));
  body.querySelector(".vw-fallback").appendChild(b);
}
function markDirty() {
  $("vw-meta").textContent = fmtSize(vw.size || 0) + (vw.dirty ? " · " + S.viewer.unsavedMeta : "");
  $("vw-save").classList.toggle("dirty", vw.dirty);
}
function renderEditor() {
  const body = $("vw-body");
  body.className = "editbody";
  // Re-highlighting a huge file on every keystroke would jank: past this size
  // skip the underlay and keep the plain textarea.
  const withHl = vw.text.length <= 300000;
  body.innerHTML = `<pre class="linenos" id="ed-nos"></pre>` + (withHl
    ? `<div id="ed-wrap"><pre id="ed-hl" aria-hidden="true"><code></code></pre>` +
      `<textarea id="ed-ta" spellcheck="false" wrap="off"></textarea></div>`
    : `<textarea id="ed-ta" spellcheck="false" wrap="off"></textarea>`);
  const ta = $("ed-ta"), nos = $("ed-nos");
  const hlPre = withHl ? $("ed-hl") : null;
  const hlCodeEl = hlPre ? hlPre.firstElementChild : null;
  const lang = extOf(vw.name);
  ta.value = vw.text;
  vw.ta = ta;
  // Repaint the underlay at most once per frame while typing. The trailing
  // "\n" sentinel gives the last (possibly empty) line the same height as the
  // textarea's, so both reach the same max scroll offset.
  let hlRaf = 0;
  const paintHl = () => {
    hlRaf = 0;
    hlCodeEl.innerHTML = hlCode(ta.value, lang) + "\n";
  };
  const scheduleHl = () => {
    if (hlCodeEl && !hlRaf) hlRaf = requestAnimationFrame(paintHl);
  };
  const syncNos = () => {
    const n = ta.value.split("\n").length;
    nos.textContent = Array.from({ length: n }, (_, i) => i + 1).join("\n");
    nos.scrollTop = ta.scrollTop;
  };
  const refresh = () => { syncNos(); scheduleHl(); };
  if (hlCodeEl) paintHl();
  syncNos();
  ta.addEventListener("scroll", () => {
    nos.scrollTop = ta.scrollTop;
    if (hlPre) hlPre.style.transform = `translate(${-ta.scrollLeft}px, ${-ta.scrollTop}px)`;
  });
  ta.addEventListener("input", () => {
    vw.dirty = ta.value !== vw.text;
    markDirty();
    refresh();
  });
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Tab") {
      e.preventDefault();
      const a = ta.selectionStart, b = ta.selectionEnd;
      ta.value = ta.value.slice(0, a) + "  " + ta.value.slice(b);
      ta.selectionStart = ta.selectionEnd = a + 2;
      vw.dirty = ta.value !== vw.text;
      markDirty();
      refresh();
    } else if ((e.key === "s" || e.key === "S") && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      saveEditor();
    }
  });
  ta.focus();
}
async function saveEditor() {
  if (!vw.ta || !vw.name) return;
  const text = vw.ta.value, name = vw.name;
  const btn = $("vw-save");
  btn.disabled = true;
  try {
    await rpc("files.write", { name, text });
    // Our own write comes back as a files-change event; don't warn about it.
    vw.selfSave = Date.now();
    vw.text = text;
    vw.dirty = false;
    vw.size = new TextEncoder().encode(text).length;
    $("vw-banner").classList.add("hidden");
    markDirty();
  } catch (e) {
    dlgAlert(S.viewer.saveFailed(e.message));
  } finally { btn.disabled = false; }
}
/// Someone else changed the open file. Never clobber unsaved work: offer the
/// choice instead.
function remoteChangeBanner() {
  const b = $("vw-banner");
  b.classList.remove("hidden");
  b.innerHTML = `<span>${I("circle-alert", 13)} ${S.viewer.remoteChange}</span>`;
  const mk = (label, run) => {
    const btn = document.createElement("button");
    btn.textContent = label;
    btn.onclick = run;
    b.appendChild(btn);
  };
  mk(S.viewer.reload, () => {
    vw.dirty = false;
    vw.text = null;
    b.classList.add("hidden");
    renderViewer();
  });
  mk(S.viewer.keepMine, () => b.classList.add("hidden"));
}

// ---------- component ----------
class FileViewer extends HTMLElement {
  connectedCallback() {
    installStyle("file-viewer", CSS);
    const dlg = document.createElement("dialog");
    dlg.id = "viewerdlg";
    dlg.className = "dlg";
    dlg.innerHTML = TPL;
    this.appendChild(dlg);
    wire(dlg);
  }
}

function wire(dlg) {
  $("vw-dl").innerHTML = I("download", 14);
  $("vw-tab").innerHTML = I("external-link", 14);
  $("vw-close").innerHTML = I("x", 15);
  $("vw-dl").onclick = () => vw.name && downloadUrl(appUrl(vw.name));
  $("vw-tab").onclick = () => vw.name && window.open(appUrl(vw.name), "_blank", "noopener");
  $("vw-toggle").onclick = () => { vw.mode = vw.mode === "raw" ? "pretty" : "raw"; renderViewer(); };
  $("vw-edit").onclick = () => { vw.mode = "edit"; renderViewer(); };
  $("vw-save").onclick = () => saveEditor();
  $("vw-close").onclick = () => closeViewer();
  // ESC closes a <dialog> natively — intercept it while there's unsaved work.
  dlg.addEventListener("cancel", (e) => {
    if (!vw.dirty) return;
    e.preventDefault();
    closeViewer();
  });
  window.addEventListener("beforeunload", (e) => {
    if (vw.dirty) { e.preventDefault(); e.returnValue = ""; }
  });

  // A files change arrived: refresh what is shown.
  on("files-changed", () => {
    if (!vw.name || !dlg.open) return;
    if (Date.now() - vw.selfSave < 2000) return;
    if (vw.dirty || vw.mode === "edit") { remoteChangeBanner(); return; }
    vw.text = null;
    renderViewer();
  });
  // Someone (or the assistant) wants this file shown here and now.
  on("present", (p) => openViewer(p.name, p.mode || "auto", p.by || ""));
}

customElements.define("file-viewer", FileViewer);
