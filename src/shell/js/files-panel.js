/* <files-panel> — the app archive as a Nautilus-style file manager. Folders
 * are derived from '/' in sqlar names (the archive has no real directories).
 * Every name lives under one of exactly two roots: app/ (the app itself, what
 * a template ships) or data/ (what the users put in). A file's location IS
 * its role, so moving it between the two is the only way to change that. */

import { rpc, on } from "./core.js";
import {
  $, I, esc, div, fmtSize, fmtTime, appUrl, downloadUrl, installStyle, copyText,
  showMenu, dlgAlert, dlgConfirm, dlgInput, dlgPickRoot, makeDialog, footBtn,
  registerPanel, closePanel, panelOpen, fileKind, KIND_ICON,
} from "./ui.js";
import { openViewer } from "./viewer.js";
import { pickTemplate } from "./template-update.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div class="panel-head">
    <div id="fb-crumbs"></div>
    <div class="spacer"></div>
    <input id="fb-search" class="hidden" placeholder="${S.files.filterPlaceholder}" autocomplete="off">
    <button id="fb-searchbtn" class="icon-btn" title="${S.files.searchFiles}"></button>
    <button id="fb-viewbtn" class="icon-btn" title="${S.files.gridListView}"></button>
    <button id="fb-newbtn" class="icon-btn" title="${S.files.newFileBtn}"></button>
    <button id="fb-more" class="icon-btn" title="${S.files.more}"></button>
    <button id="fb-close" class="icon-btn" title="${S.common.closeEsc}"></button>
  </div>
  <div id="fb-filters"></div>
  <div id="fb-body">
    <div id="fb-pane">
      <div id="fb-items"></div>
      <div id="fb-drop">${S.files.dropToUpload}</div>
    </div>
  </div>
  <input type="file" id="fb-upload" multiple class="hidden">
`;

const CSS = /* css */ `
#filebrowser { width: min(940px, 96vw); background: var(--surface); }
#fb-crumbs {
  display: flex; align-items: baseline; gap: 2px; min-width: 0;
  overflow-x: auto; scrollbar-width: none; margin-left: 2px;
}
#fb-crumbs::-webkit-scrollbar { display: none; }
.crumb {
  border: none; background: none; font: inherit; font-size: 13px; cursor: pointer;
  padding: 3px 4px; border-radius: var(--r-s); color: var(--muted); white-space: nowrap;
}
.crumb:hover { color: var(--brand); background: var(--brand-wash); }
.crumb.cur { color: var(--text); font-weight: 600; }
.crumbsep { color: var(--ghost); font-size: 12px; }
#fb-search { width: 170px; padding: 5px 10px; font-size: 13px; }
/* filter chips: All · App / Data roots · folders inside the current root */
#fb-filters {
  display: flex; align-items: center; gap: 5px; flex: 0 0 auto;
  padding: 7px 10px; border-bottom: 1px solid var(--line-soft);
  overflow-x: auto; scrollbar-width: none;
}
#fb-filters::-webkit-scrollbar { display: none; }
.fbchip-sep { width: 1px; align-self: stretch; margin: 2px 3px; background: var(--line); flex: 0 0 auto; }
#fb-body { flex: 1; display: flex; min-height: 0; }
#fb-pane { flex: 1; min-width: 0; position: relative; overflow: auto; }
#fb-items { padding: 10px; }
#fb-items.grid {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(104px, 1fr)); gap: 6px;
}
#fb-items .hint { grid-column: 1 / -1; }
.tile {
  display: flex; flex-direction: column; align-items: center; gap: 3px;
  padding: 12px 6px 9px; border-radius: var(--r-m); cursor: default; position: relative;
  border: 1px solid transparent; text-align: center;
}
.tile:hover { background: var(--hover); }
.tile.sel { background: var(--brand-soft); border-color: var(--brand-line); }
.tile .tface {
  width: 52px; height: 52px; display: flex; align-items: center; justify-content: center;
  color: var(--text-2);
}
.tile .tface svg { width: 36px; height: 36px; stroke-width: 1.6; }
/* App and Data are the archive's two fixed roots — they read as places, not
   as ordinary folders. */
.tile.rootdir .tface { color: var(--brand-ink); background: var(--brand-soft); border-radius: var(--r-m); }
tr.rootdir .tname { font-weight: 600; }
.tile .thumb { max-width: 52px; max-height: 52px; border-radius: 4px; object-fit: contain; }
.tile .tlabel {
  font-size: 12px; line-height: 1.3; max-width: 100%;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.tile .tsub { font-size: 10px; color: var(--faint); }
.tile .fbmore { position: absolute; top: 4px; right: 4px; opacity: 0; }
.tile:hover .fbmore, .tile.sel .fbmore { opacity: 1; }
.fbmore {
  border: none; background: none; cursor: pointer; color: var(--faint);
  display: inline-flex; padding: 4px; border-radius: var(--r-s); margin-left: auto;
}
.fbmore:hover { background: var(--line); color: var(--text-2); }
.fbtable { border-collapse: collapse; width: 100%; font-size: 13px; }
.fbtable th {
  text-align: left; font-size: 11px; text-transform: uppercase; letter-spacing: .05em;
  color: var(--faint); font-weight: 600; padding: 4px 8px; border-bottom: 1px solid var(--line);
  position: sticky; top: 0; background: var(--surface);
}
.fbtable td { padding: 7px 8px; border-bottom: 1px solid var(--line-soft); }
.fbtable tr.sel td { background: var(--brand-soft); }
.fbtable tr:hover td { background: var(--hover); }
.fbtable tr.sel:hover td { background: var(--brand-soft); }
.fbtable td.num { text-align: right; color: var(--text-2); font-variant-numeric: tabular-nums; }
.fbname { display: flex; align-items: center; gap: 7px; min-width: 0; }
.fbname .tname { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.fbtable .fbmore { opacity: 0; }
.fbtable tr:hover .fbmore, .fbtable tr.sel .fbmore { opacity: 1; }
@media (hover: none) { .fbmore, .tile .fbmore { opacity: 1; } }
#fb-drop {
  position: absolute; inset: 0; display: none; align-items: center; justify-content: center;
  background: rgba(47,109,246,.12); border: 3px dashed var(--brand); color: var(--brand);
  font-size: 15px; font-weight: 600; pointer-events: none;
}
#fb-pane.dropping #fb-drop { display: flex; }
/* file history rows (Earlier-versions dialog) */
.fver {
  display: flex; gap: 10px; align-items: center; font-size: 12px;
  color: var(--text-2); padding: 4px 0; border-top: 1px dashed var(--hover);
}
.fver .fmeta { min-width: 130px; }
.fver span:nth-child(2) { flex: 1; }
.frestore {
  display: inline-flex; gap: 4px; align-items: center; font-size: 11px;
  border: 1px solid var(--line-strong); background: var(--surface); border-radius: 6px;
  padding: 2px 8px; cursor: pointer;
}
.frestore:hover { background: var(--brand-wash); border-color: var(--brand-line); }
/* Touch devices: taller rows and a bigger restore button. */
@media (pointer: coarse) {
  .fbtable td { padding: 11px 8px; }
  .frestore { padding: 7px 11px; }
}
@media (max-width: 700px) {
  .fbtable th:nth-child(3), .fbtable td:nth-child(3) { display: none; } /* Modified */
  #fb-items.grid { grid-template-columns: repeat(auto-fill, minmax(88px, 1fr)); }
  #fb-search { width: 120px; }
  #fb-search:not(.hidden) { flex: 1; min-width: 0; }
}
`;

const ROOTS = { "app/": S.files.rootApp, "data/": S.files.rootData };
const ROOT_ICON = { "app/": "code", "data/": "files" };
const ROOT_HINT = { "app/": S.files.rootAppHint, "data/": S.files.rootDataHint };
const rootOf = (name) => (name.startsWith("data/") ? "data/" : "app/");
const otherRoot = (name) => (rootOf(name) === "app/" ? "data/" : "app/");
const MAX_UPLOAD = 100 * 1024 * 1024;

const fb = {
  rows: [],            // files.list rows: [name, sz, mtime, role]
  cwd: "",             // current prefix, "" = root, otherwise ends with "/"
  view: localStorage.getItem("uapp.fbview") === "list" ? "list" : "grid",
  query: "",           // search filter (substring, searches the whole archive)
  sel: null,
};
/// The folder new files land in (main.js uses this for native drops).
export const fbCwd = () => fb.cwd;

export async function loadFiles() {
  try {
    fb.rows = (await rpc("files.list")).rows;
  } catch { return; }
  renderFb();
}
/// Entries for the current folder (or flat search results): folders first,
/// then files, each alphabetical.
function fbEntries() {
  const rows = fb.rows;
  if (fb.query) {
    const q = fb.query.toLowerCase();
    return rows
      .filter(([n]) => n.toLowerCase().includes(q))
      .map(([name, sz, mtime, role]) => ({ dir: false, name, label: name, sz, mtime, role }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }
  const dirs = new Map();
  const files = [];
  for (const [name, sz, mtime, role] of rows) {
    if (!name.startsWith(fb.cwd)) continue;
    const rest = name.slice(fb.cwd.length);
    const cut = rest.indexOf("/");
    if (cut > 0) {
      const label = rest.slice(0, cut);
      const d = dirs.get(label) || { count: 0, mtime: 0 };
      d.count++;
      d.mtime = Math.max(d.mtime, mtime);
      dirs.set(label, d);
    } else if (rest) {
      files.push({ dir: false, name, label: rest, sz, mtime, role });
    }
  }
  files.sort((a, b) => a.label.localeCompare(b.label));
  return [...dirs]
    .map(([label, d]) => {
      const name = fb.cwd + label + "/";
      // The two roots are the app's own furniture: named, not renameable.
      return { dir: true, name, label: ROOTS[name] || label, root: !!ROOTS[name],
               count: d.count, mtime: d.mtime };
    })
    .sort((a, b) => a.label.localeCompare(b.label))
    .concat(files);
}
/// An empty root still has to be visible — you can't drop files into a
/// folder that isn't there.
function fbEntriesWithRoots() {
  const entries = fbEntries();
  if (fb.cwd || fb.query) return entries;
  for (const [prefix, label] of Object.entries(ROOTS)) {
    if (!entries.some((e) => e.name === prefix)) {
      entries.unshift({ dir: true, name: prefix, label, root: true, count: 0, mtime: 0 });
    }
  }
  return entries.sort((a, b) => b.dir - a.dir || a.label.localeCompare(b.label));
}
function fbGo(cwd) {
  fb.cwd = cwd;
  fb.sel = null;
  renderFb();
}
// Destinations render as one horizontal chip row (scrollable on narrow
// screens) instead of a desktop-style sidebar column.
function fbChip(icon, label, active, run) {
  const b = document.createElement("button");
  b.className = "chip-btn" + (active ? " active" : "");
  b.innerHTML = `<span class="ticon">${I(icon, 12)}</span>${esc(label)}`;
  b.onclick = run;
  $("fb-filters").appendChild(b);
}
function renderFb() {
  const filters = $("fb-filters");
  filters.innerHTML = "";
  fbChip("hard-drive", S.files.rootAll, !fb.cwd, () => fbGo(""));
  for (const [prefix, label] of Object.entries(ROOTS)) {
    fbChip(ROOT_ICON[prefix], label, fb.cwd.startsWith(prefix), () => fbGo(prefix));
  }
  // Folders inside the root we're in, so one click gets anywhere near.
  const here = fb.cwd.startsWith("app/") ? "app/" : fb.cwd.startsWith("data/") ? "data/" : "";
  if (here) {
    const subs = [...new Set(fb.rows
      .map(([n]) => n.startsWith(here) && n.slice(here.length).split("/")[1] !== undefined
        ? n.slice(here.length).split("/")[0] : null)
      .filter(Boolean))].sort();
    if (subs.length) filters.insertAdjacentHTML("beforeend", `<span class="fbchip-sep"></span>`);
    for (const s of subs) {
      fbChip("folder", s, fb.cwd === here + s + "/", () => fbGo(here + s + "/"));
    }
  }

  const cr = $("fb-crumbs");
  cr.innerHTML = "";
  const segs = fb.cwd ? fb.cwd.replace(/\/$/, "").split("/") : [];
  const crumb = (label, target, cur) => {
    const b = document.createElement("button");
    b.className = "crumb" + (cur ? " cur" : "");
    b.textContent = label;
    b.onclick = () => fbGo(target);
    cr.appendChild(b);
  };
  crumb(S.files.title, "", !segs.length);
  let acc = "";
  segs.forEach((sg, k) => {
    cr.insertAdjacentHTML("beforeend", `<span class="crumbsep">/</span>`);
    acc += sg + "/";
    crumb(ROOTS[acc] || sg, acc, k === segs.length - 1);
  });

  const pane = $("fb-items");
  pane.className = fb.view;
  pane.innerHTML = "";
  const entries = fbEntriesWithRoots();
  if (!entries.length) {
    pane.innerHTML = `<p class="hint">${fb.query ? S.files.noMatch
      : S.files.emptyFolder}</p>`;
  } else if (fb.view === "grid") {
    for (const e of entries) {
      const t = div("tile");
      const kind = e.dir ? null : fileKind(e.name);
      // Images preview themselves; everything else gets its type icon.
      const face = e.dir ? I(e.root ? ROOT_ICON[e.name] : "folder", 44)
        : kind === "image" && e.sz < 2097152 ? `<img class="thumb" src="${appUrl(e.name)}" alt="">`
        : I(KIND_ICON[kind], 44);
      t.innerHTML = `<div class="tface">${face}</div><div class="tlabel">${esc(e.label)}</div>` +
        `<div class="tsub">${e.dir ? (e.root ? ROOT_HINT[e.name] + " · " : "") +
            S.files.itemCount(e.count) : fmtSize(e.sz)}</div>`;
      wireEntry(t, e);
      pane.appendChild(t);
    }
  } else {
    const table = document.createElement("table");
    table.className = "fbtable";
    table.innerHTML = `<thead><tr><th>${S.files.colName}</th><th>${S.files.colSize}</th><th>${S.files.colModified}</th></tr></thead>`;
    const body = document.createElement("tbody");
    for (const e of entries) {
      const tr = document.createElement("tr");
      const icon = I(e.dir ? (e.root ? ROOT_ICON[e.name] : "folder") : KIND_ICON[fileKind(e.name)], 15);
      tr.innerHTML = `<td class="fbname"><span class="ticon">${icon}</span><span class="tname">${esc(e.label)}</span></td>` +
        `<td class="num">${e.dir ? "—" : fmtSize(e.sz)}</td>` +
        `<td class="fmeta">${e.mtime ? fmtTime(e.mtime * 1000) : ""}</td>`;
      wireEntry(tr, e);
      body.appendChild(tr);
    }
    table.appendChild(body);
    pane.appendChild(table);
  }
}
function wireEntry(el, e) {
  el.classList.add("fbitem");
  if (e.root) el.classList.add("rootdir");
  if (fb.sel === e.name) el.classList.add("sel");
  el.tabIndex = 0;
  const open = () => (e.dir ? fbGo(e.name) : openViewer(e.name));
  el.onclick = () => {
    fb.sel = e.name;
    document.querySelectorAll("#fb-items .fbitem.sel").forEach((x) => x.classList.remove("sel"));
    el.classList.add("sel");
  };
  el.ondblclick = open;
  el.onkeydown = (ev) => { if (ev.key === "Enter") { ev.preventDefault(); open(); } };
  el.oncontextmenu = (ev) => {
    ev.preventDefault();
    fb.sel = e.name;
    entryMenu(e, ev.clientX, ev.clientY);
  };
  const more = document.createElement("button");
  more.className = "fbmore";
  more.title = "Actions";
  more.innerHTML = I("ellipsis-vertical", 14);
  more.onclick = (ev) => {
    ev.stopPropagation();
    const r = more.getBoundingClientRect();
    entryMenu(e, r.right, r.bottom);
  };
  (el.querySelector(".fbname") || el).appendChild(more);
}
function entryMenu(e, x, y) {
  if (e.dir) {
    const items = [{ icon: "folder-open", label: S.files.ctxOpen, run: () => fbGo(e.name) }];
    // The two roots are fixed; anything below them renames and moves like a
    // file does.
    if (!e.root) {
      items.push({ sep: true });
      items.push({ icon: "type", label: S.files.ctxRename, run: () => renameEntry(e.name, true) });
      items.push({ icon: "folder-input", label: S.files.ctxMoveTo(ROOTS[otherRoot(e.name)]),
                   run: () => moveToRoot(e.name, true) });
    }
    showMenu(items, x, y);
    return;
  }
  showMenu([
    { icon: "eye", label: S.files.ctxOpen, run: () => openViewer(e.name) },
    { icon: "code", label: S.files.ctxOpenRaw, run: () => openViewer(e.name, "raw") },
    { icon: "file-pen", label: S.files.ctxEdit, run: () => openViewer(e.name, "edit") },
    { icon: "external-link", label: S.files.ctxNewTab, run: () => window.open(appUrl(e.name), "_blank", "noopener") },
    { sep: true },
    { icon: "type", label: S.files.ctxRename, run: () => renameEntry(e.name, false) },
    { icon: "folder-input", label: S.files.ctxMoveTo(ROOTS[otherRoot(e.name)]),
      run: () => moveToRoot(e.name, false) },
    { icon: "download", label: S.common.download, run: () => downloadUrl(appUrl(e.name)) },
    { icon: "copy", label: S.files.ctxCopyName, run: () => copyText(e.name) },
    { icon: "history", label: S.files.ctxHistory, run: () => showHistory(e.name) },
    { sep: true },
    { icon: "trash-2", label: S.files.ctxDelete, danger: true, run: () => deleteFile(e.name) },
  ], x, y);
}
/// Rename a file, or (dir) a folder and everything under it. Both edit the
/// full path, so a rename is also how you move something.
async function renameEntry(name, dir) {
  const shown = dir ? name.replace(/\/$/, "") : name;
  const to = await dlgInput(
    dir ? S.files.renameFolderPrompt(shown) : S.files.renameFilePrompt(shown),
    shown, S.common.rename);
  if (to === null || !to.trim() || to.trim() === shown) return;
  const target = dir ? to.trim().replace(/\/+$/, "") + "/" : to.trim();
  try {
    await rpc("files.rename", { from: name, to: target });
    if (dir && fb.cwd.startsWith(name)) fb.cwd = target + fb.cwd.slice(name.length);
    await loadFiles();
  } catch (e) { dlgAlert(e.message); }
}
/// Flip an entry between the App and Data trees — that IS its role.
async function moveToRoot(name, dir) {
  const to = otherRoot(name) + name.slice(rootOf(name).length);
  try {
    await rpc("files.rename", { from: name, to });
    if (dir && fb.cwd.startsWith(name)) fb.cwd = to + fb.cwd.slice(name.length);
    await loadFiles();
  } catch (e) { dlgAlert(e.message); }
}
async function deleteFile(name) {
  if (!(await dlgConfirm(S.files.deleteConfirm(name)))) return;
  try { await rpc("files.delete", { name }); await loadFiles(); }
  catch (e) { dlgAlert(e.message); }
}
/// Where new files land. Inside one of the roots that's the current folder;
/// at the top level the choice is App vs Data, and it matters, so ask.
export async function pickDest() {
  if (fb.cwd) return fb.cwd;
  return await dlgPickRoot(S.files.rootPickerPrompt);
}
/// Uploads POST their bytes to /upload rather than going through an RPC
/// frame, so a 100MB file is never base64 in the page.
async function uploadFiles(list, dir) {
  if (dir === undefined) dir = await pickDest();
  if (!dir) return;
  for (const f of list) {
    if (f.size > MAX_UPLOAD) { dlgAlert(S.files.uploadTooLarge(f.name, MAX_UPLOAD / 1048576)); continue; }
    try {
      const r = await fetch("/upload?name=" + encodeURIComponent(dir + f.name),
        { method: "POST", body: f, headers: { "content-type": "application/octet-stream" } });
      if (!r.ok) throw new Error((await r.text()) || r.statusText);
    } catch (e) { dlgAlert(`${f.name}: ${e.message}`); }
  }
  await loadFiles();
}
async function newFile(seed) {
  if (seed === undefined) {
    const dest = await pickDest();
    if (!dest) return;
    seed = dest;
  }
  const p = await dlgInput(S.files.newFilePrompt, seed, S.common.create);
  if (p === null) return;
  const name = p.trim().replace(/^\/+/, "");
  if (!name || name.endsWith("/")) return;
  if (fb.rows.some(([n]) => n === name)) { dlgAlert(S.files.alreadyExists(name)); return; }
  try {
    const r = await rpc("files.write", { name, text: "" });
    await loadFiles();
    openViewer(r.name || name, "edit");
  } catch (e) { dlgAlert(e.message); }
}
/// sqlar has no real directories, so a folder only exists once a file sits in
/// it: this seeds the New File dialog with the prefix.
async function newFolder() {
  const dest = await pickDest();
  if (!dest) return;
  const p = await dlgInput(S.files.newFolderPrompt, dest, S.files.newFolderNext);
  if (p === null) return;
  const dir = p.trim().replace(/^\/+|\/+$/g, "");
  if (dir) newFile(dir + "/");
}
async function showHistory(name) {
  const d = makeDialog(S.files.historyTitle, "history");
  const body = d.querySelector(".dlg-body");
  body.innerHTML = `<p class="hint">${esc(name)}</p><p class="hint">${S.common.loading}</p>`;
  footBtn(d, S.common.close).onclick = () => d.close();
  d.showModal();
  const h = await rpc("files.history", { name }).catch(() => null);
  body.innerHTML = `<p class="hint">${esc(name)}</p>`;
  if (!h || !h.rows.length) {
    body.insertAdjacentHTML("beforeend",
      `<p class="hint">${S.files.noHistory}</p>`);
    return;
  }
  for (const [id, ts, , user, action, size] of h.rows) {
    const row = div("fver");
    row.innerHTML =
      `<span class="fmeta">${fmtTime(ts)}</span>` +
      `<span>${esc(user || "")} · ${action === "del" ? "deleted" : "edited"} · ${fmtSize(size)}</span>`;
    const btn = document.createElement("button");
    btn.className = "frestore";
    btn.innerHTML = I("rotate-cw", 11) + " " + S.files.restore;
    btn.onclick = async () => {
      if (!(await dlgConfirm(S.files.restoreConfirm(name, fmtTime(ts))))) return;
      btn.disabled = true;
      try {
        await rpc("files.revert", { name, id });
        await loadFiles();
        d.close();
        showHistory(name);
      } catch (e) { dlgAlert(e.message); btn.disabled = false; }
    };
    row.appendChild(btn);
    body.appendChild(row);
  }
}

// ---------- component ----------
class FilesPanel extends HTMLElement {
  connectedCallback() {
    installStyle("files-panel", CSS);
    this.innerHTML = TPL;
    wire();
  }
}

function wire() {
  registerPanel("filebrowser", "btn-files", () => loadFiles());

  $("fb-searchbtn").innerHTML = I("search", 15);
  $("fb-newbtn").innerHTML = I("plus", 15);
  $("fb-more").innerHTML = I("ellipsis-vertical", 15);
  $("fb-close").innerHTML = I("x", 15);
  const renderFbViewBtn = () => {
    $("fb-viewbtn").innerHTML = I(fb.view === "grid" ? "list" : "layout-grid", 15);
  };
  renderFbViewBtn();
  $("fb-viewbtn").onclick = () => {
    fb.view = fb.view === "grid" ? "list" : "grid";
    localStorage.setItem("uapp.fbview", fb.view);
    renderFbViewBtn();
    renderFb();
  };
  $("fb-searchbtn").onclick = () => {
    const inp = $("fb-search");
    const hiding = !inp.classList.contains("hidden");
    inp.classList.toggle("hidden", hiding);
    if (hiding) { inp.value = ""; fb.query = ""; renderFb(); } else inp.focus();
  };
  $("fb-search").addEventListener("input", (e) => { fb.query = e.target.value.trim(); renderFb(); });
  $("fb-search").addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); $("fb-searchbtn").click(); }
  });
  $("fb-newbtn").onclick = (ev) => {
    const r = ev.currentTarget.getBoundingClientRect();
    showMenu([
      { icon: "upload", label: S.files.menuUpload, run: () => $("fb-upload").click() },
      { icon: "file-plus", label: S.files.menuNewFile, run: () => newFile() },
      { icon: "folder-plus", label: S.files.menuNewFolder, run: () => newFolder() },
    ], r.right, r.bottom);
  };
  $("fb-more").onclick = (ev) => {
    const r = ev.currentTarget.getBoundingClientRect();
    showMenu([
      { icon: "refresh-cw", label: S.common.refresh, run: () => loadFiles() },
      { icon: "package", label: S.files.menuShareTemplate, run: () => downloadUrl("/template.uapp") },
      { icon: "download", label: S.files.menuDownloadApp, run: () => downloadUrl("/download.uapp") },
      { sep: true },
      // The other half of "share as template": take one in. Dropping the file
      // on the window does the same thing.
      { icon: "folder-input", label: S.files.menuUpdateFromTemplate, run: () => pickTemplate() },
    ], r.right, r.bottom);
  };
  $("fb-upload").addEventListener("change", (e) => {
    uploadFiles([...e.target.files]);
    e.target.value = "";
  });
  const fbPane = $("fb-pane");
  ["dragenter", "dragover"].forEach((ev) =>
    fbPane.addEventListener(ev, (e) => { e.preventDefault(); fbPane.classList.add("dropping"); }));
  ["dragleave", "drop"].forEach((ev) =>
    fbPane.addEventListener(ev, (e) => {
      e.preventDefault();
      if (ev === "drop" || e.target === fbPane) fbPane.classList.remove("dropping");
    }));
  fbPane.addEventListener("drop", (e) => {
    fbPane.classList.remove("dropping");
    if (e.dataTransfer.files.length) uploadFiles([...e.dataTransfer.files]);
  });
  // Right-click on empty space: the folder's own actions (entries have theirs).
  fbPane.addEventListener("contextmenu", (e) => {
    if (e.target.closest(".fbitem")) return;
    e.preventDefault();
    showMenu([
      { icon: "file-plus", label: S.files.menuNewFile, run: () => newFile() },
      { icon: "folder-plus", label: S.files.menuNewFolder, run: () => newFolder() },
      { icon: "upload", label: S.files.menuUpload, run: () => $("fb-upload").click() },
      { sep: true },
      { icon: fb.view === "grid" ? "list" : "layout-grid",
        label: fb.view === "grid" ? S.files.listView : S.files.gridView, run: () => $("fb-viewbtn").click() },
      { icon: "refresh-cw", label: S.common.refresh, run: () => loadFiles() },
    ], e.clientX, e.clientY);
  });
  $("fb-close").onclick = () => closePanel("filebrowser");

  // A files change arrived: refresh if visible.
  on("files-changed", () => {
    if (panelOpen("filebrowser")) loadFiles();
  });
}

customElements.define("files-panel", FilesPanel);
