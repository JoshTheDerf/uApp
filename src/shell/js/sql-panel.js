/* <sql-panel> — the database browser. Left: table list. Right: the Browse tab
 * (filter / sort / page / edit the picked table) or the SQL tab (raw editor).
 * Filter values are always bound as parameters — user input never lands
 * inside the SQL text. Writes replicate to every device, exactly like the
 * app's own queries. */

import { rpc, on } from "./core.js";
import {
  $, I, esc, div, fmtSize, installStyle, copyText, showMenu,
  dlgAlert, dlgConfirm, makeDialog, footBtn, registerPanel, closePanel, isComposing, menuFrom,
} from "./ui.js";
import { toCsv, toTsv, toXlsx, downloadBlob } from "./export.js";
import { S, getLocale } from "./strings.js";

const TPL = /* html */ `
  <div class="panel-head">
    <span class="panel-title" id="sql-title"><span id="sql-head-icon"></span>${S.sql.title}</span>
    <span id="sql-dbinfo" class="hint"></span>
    <div class="spacer"></div>
    <div id="sql-tabs" role="tablist" aria-label="${S.sql.title}">
      <button id="tab-browse" class="sqltab active" role="tab" aria-selected="true" aria-controls="sql-browse">${S.sql.tabBrowse}</button>
      <button id="tab-sql" class="sqltab" role="tab" aria-selected="false" aria-controls="sql-query" tabindex="-1">${S.sql.tabSql}</button>
    </div>
    <button id="btn-sql-refresh" class="icon-btn" title="${S.sql.refreshTables}" aria-label="${S.sql.refreshTables}"></button>
    <button id="btn-sql-close" class="icon-btn" title="${S.common.closeEsc}" aria-label="${S.common.close}"></button>
  </div>
  <div id="sql-body">
    <nav id="sql-tables" aria-label="${S.sql.tablesLabel}"></nav>
    <div id="sql-main">
      <section id="sql-browse" role="tabpanel" aria-labelledby="tab-browse">
        <div id="br-toolbar">
          <select id="sql-table-select" class="mini" aria-label="${S.sql.tablesLabel}"></select>
          <strong id="br-title"></strong>
          <span id="br-count" class="hint"></span>
          <div class="spacer"></div>
          <button id="br-filter" class="chip-btn br-chip hidden" title="${S.sql.addFilter}"></button>
          <button id="br-insert" class="chip-btn br-chip hidden" title="${S.sql.insertRow}"></button>
          <button id="br-schema" class="chip-btn br-chip hidden" title="${S.sql.showSchema}"></button>
          <button id="br-sqlbtn" class="chip-btn br-chip hidden" title="${S.sql.openInSql}"></button>
          <button id="br-export" class="chip-btn br-chip hidden" title="${S.sql.exportRows}"></button>
          <button id="br-more" class="icon-btn hidden" title="${S.files.more}" aria-label="${S.files.more}"></button>
        </div>
        <div id="br-filters" class="hidden"></div>
        <pre id="br-schemabox" class="schemabox hidden"></pre>
        <div id="br-result"><p class="hint pad">${S.sql.pickTableHint}</p></div>
        <div id="br-pager" class="hidden">
          <button id="br-prev" class="icon-btn" title="${S.sql.prevPage}" aria-label="${S.sql.prevPage}"></button>
          <span id="br-range" class="hint"></span>
          <button id="br-next" class="icon-btn" title="${S.sql.nextPage}" aria-label="${S.sql.nextPage}"></button>
        </div>
      </section>
      <section id="sql-query" class="hidden" role="tabpanel" aria-labelledby="tab-sql">
        <div id="sql-editorbar">
          <textarea id="sql-input" rows="3" spellcheck="false" aria-label="${S.sql.tabSql}"
            placeholder="${S.sql.sqlPlaceholder}"></textarea>
          <button id="btn-sql-run" class="btn primary"></button>
        </div>
        <div id="sql-result"></div>
      </section>
    </div>
  </div>
`;

const CSS = /* css */ `
#sqlpanel { width: var(--panel-w-l); background: var(--surface); }
#sql-tabs {
  display: flex; gap: 2px; background: var(--hover-2); border-radius: var(--r-pill); padding: 2px;
  margin-right: var(--sp-1);
}
.sqltab {
  border: none; background: none; font: inherit; font-size: var(--fs-sm); cursor: pointer;
  min-height: 24px; padding: 0 var(--sp-3); border-radius: var(--r-pill); color: var(--text-2);
}
.sqltab.active { background: var(--surface); color: var(--text); font-weight: 600; box-shadow: var(--shadow-1); }
#sql-body { flex: 1; display: flex; min-height: 0; }
#sql-tables {
  width: 200px; flex: 0 0 auto; overflow-y: auto; padding: var(--sp-2);
  border-right: 1px solid var(--line-soft); background: var(--surface-3);
}
#sql-tables .tsection { margin: var(--sp-2) var(--sp-1) var(--sp-1); }
#sql-tables .tsection:first-child { margin-top: var(--sp-1); }
.tblrow {
  display: flex; align-items: center; gap: var(--sp-2); width: 100%; min-height: var(--h-m);
  border: none; background: none; cursor: pointer; font: inherit; font-size: var(--fs-md);
  padding: 0 var(--sp-2); border-radius: var(--r-s); text-align: left; color: var(--text);
}
.tblrow:hover { background: var(--hover); }
.tblrow.active { background: var(--brand-soft); color: var(--brand-ink); font-weight: 600; }
.tblrow.active .ticon { color: var(--brand); }
.tblrow .tname { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tblrow.internal { color: var(--faint); }
.tblrow.internal .ticon { color: var(--ghost); }
#sql-table-select { display: none; max-width: 50%; }
#br-more { display: none; }
#sql-main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
#sql-browse, #sql-query { flex: 1; min-height: 0; display: flex; flex-direction: column; }
#br-toolbar {
  display: flex; align-items: center; gap: var(--sp-2); flex: 0 0 auto;
  /* wrap: on narrow screens the chip buttons drop to a second row instead of
     crushing the row-count text into one-word-per-line */
  flex-wrap: wrap; min-height: var(--head-h);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--line-soft);
}
#br-toolbar strong { font-size: var(--fs-md); }
#br-count { white-space: nowrap; }
#br-filters {
  display: flex; flex-direction: column; gap: var(--sp-1); flex: 0 0 auto;
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--line-soft); background: var(--surface-3);
}
.qb-row { display: flex; align-items: center; gap: var(--sp-1); flex-wrap: wrap; }
.qb-row .qb-col { max-width: 160px; }
.qb-row .qb-op { max-width: 150px; }
.qb-row .qb-val { flex: 1; width: 80px; min-width: 60px; }
.qb-row .qb-and { width: 68px; color: var(--text-2); }
#br-result { flex: 1; min-height: 0; overflow: auto; padding: var(--sp-2) var(--sp-3); }
#br-pager {
  display: flex; align-items: center; justify-content: center; gap: var(--sp-3);
  flex: 0 0 auto; padding: var(--sp-1); border-top: 1px solid var(--line-soft);
}
.schemabox {
  flex: 0 0 auto; max-height: 30vh; overflow: auto;
  background: var(--surface-2); border-bottom: 1px solid var(--line-soft); color: var(--text);
  padding: var(--sp-2) var(--sp-3); font: var(--fs-sm)/1.5 var(--mono); white-space: pre-wrap; margin: 0;
}
#sql-editorbar { display: flex; gap: var(--sp-2); align-items: flex-start; padding: var(--sp-3) var(--sp-3) 0; }
#sql-input {
  flex: 1; resize: vertical; min-height: 72px; max-height: 40vh;
  font: var(--fs-sm)/1.5 var(--mono);
}
#sql-result { flex: 1; min-height: 0; overflow: auto; padding: var(--sp-2) var(--sp-3); }
.sqlmsg { color: var(--text-2); font-size: var(--fs-md); padding: var(--sp-1) 2px; }
.sqlmsg.err { color: var(--err-ink); white-space: pre-wrap; }
.sqlmsg svg { vertical-align: -2px; }
.sqlmsg.note-row { display: flex; align-items: center; gap: var(--sp-2); }
.rtable { border-collapse: collapse; font-size: var(--fs-sm); margin: var(--sp-1) 0; width: 100%; }
.rtable th, .rtable td {
  border: 1px solid var(--line); padding: var(--sp-1) var(--sp-2); text-align: left; box-sizing: border-box;
  max-width: 320px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
/* frozen = explicit column widths (drag-resize / cell edit); the pane scrolls */
.rtable.frozen { table-layout: fixed; }
.rtable.frozen th, .rtable.frozen td { max-width: none; }
.rtable th { background: var(--code-bg); position: sticky; top: 0; z-index: 1; font-weight: 600; }
.col-grip {
  position: absolute; top: 0; right: 0; width: 8px; height: 100%;
  cursor: col-resize; user-select: none;
}
.col-grip::after {
  content: ""; position: absolute; top: 15%; right: 2px; width: 3px; height: 70%;
  border-radius: 2px; background: transparent;
}
.col-grip:hover::after { background: var(--brand); opacity: .55; }
.rtable th.sortable { padding: 0; }
.th-sort {
  display: flex; align-items: center; gap: var(--sp-1); width: 100%; min-height: 26px;
  padding: var(--sp-1) var(--sp-2); border: none; background: none; cursor: pointer;
  font: inherit; color: inherit; text-align: left;
}
.th-sort:hover { background: var(--hover-2); }
.th-sort:focus-visible { outline-offset: -2px; }
.th-sort .sort-ico { display: inline-flex; color: var(--brand); }
.rtable tr:hover td { background: var(--surface-3); }
.rtable td.num { text-align: right; font-variant-numeric: tabular-nums; }
.rtable td.null { color: var(--ghost); font-style: italic; }
/* click-to-edit cells (Enter edits the focused one) */
.rtable td.editable { cursor: text; }
.rtable td.editable:hover { outline: 2px solid var(--brand); outline-offset: -2px; }
.rtable td:focus-visible { outline: var(--ring); outline-offset: -2px; }
.rtable td.editing { padding: 0; outline: 2px solid var(--brand); outline-offset: -2px; }
.rtable td .cell-edit {
  width: 100%; box-sizing: border-box; border: none; padding: var(--sp-1) var(--sp-2); font: inherit;
  outline: none; background: var(--warn-bg); color: var(--text);
}
.rtable td.saved { animation: cellflash 1.2s ease-out; }
@keyframes cellflash {
  0% { background: var(--ok-soft); } 100% { background: transparent; }
}
/* insert-row dialog fields */
.ins-row { display: flex !important; align-items: center; gap: var(--sp-2); margin: 6px 0 !important; }
.ins-row .ins-col {
  flex: 0 0 40%; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-size: var(--fs-md); color: var(--text);
}
.ins-row .ins-col i { color: var(--faint); font-size: var(--fs-xs); font-style: normal; margin-left: var(--sp-1); }
.ins-row input { flex: 1; margin-top: 0 !important; }
/* Phones: the table list becomes a picker in the toolbar and the action
   chips fold into one menu, so the grid gets the whole width. */
@media (max-width: 700px) {
  #sql-tables { display: none; }
  #sql-table-select { display: block; }
  #br-title { display: none; }
  #br-toolbar .br-chip { display: none; }
  #br-more:not(.hidden) { display: inline-flex; }
  #sql-dbinfo { display: none; }
}
/* Touch devices: grow the tap targets (tabs, cells, col grips). */
@media (pointer: coarse) {
  .sqltab { min-height: 34px; padding: 0 var(--sp-4); }
  .rtable td { padding: var(--sp-2) 10px; }
  .th-sort { min-height: 36px; padding: var(--sp-2) 10px; }
  .rtable td .cell-edit { padding: var(--sp-2) 10px; }
  .col-grip { width: 18px; }
}
`;

const isInternalTable = (n) => n.startsWith("uapp_") || n === "sqlar";
const qid = (name) => `"${String(name).replace(/"/g, '""')}"`;
/// "12" -> 12 for bound parameters; anything else stays text.
const coerce = (s) => {
  const t = String(s).trim();
  return t !== "" && /^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(t) ? Number(t) : s;
};
/// Inline a value as a SQL literal — only for the query the SQL tab shows;
/// live queries bind parameters instead.
const sqlLit = (v) => (v === null ? "NULL" : typeof v === "number" ? String(v)
  : "'" + String(v).replace(/'/g, "''") + "'");

const sq = {
  table: null, type: "table", ddl: "",
  tableCols: [], colTypes: {}, notNull: new Set(),
  keyCols: [], useRowid: false,
  sort: null, page: 0, pageSize: 200, total: 0, filtered: false,
  colWidths: {}, // user-dragged column widths (px, by column name), per table
};
const canEdit = () => !!sq.table && sq.type === "table" && sq.keyCols.length > 0;

function setSqlTab(tab) {
  for (const [id, name] of [["tab-browse", "browse"], ["tab-sql", "sql"]]) {
    const b = $(id), on = name === tab;
    b.classList.toggle("active", on);
    b.setAttribute("aria-selected", on ? "true" : "false");
    b.tabIndex = on ? 0 : -1; // one tab stop for the tab list; arrows move within it
  }
  $("sql-browse").classList.toggle("hidden", tab !== "browse");
  $("sql-query").classList.toggle("hidden", tab !== "sql");
  if (tab === "sql") $("sql-input").focus();
}

async function loadSqlTables(keepSelection) {
  const nav = $("sql-tables");
  const pick = $("sql-table-select");
  let r;
  try {
    r = await rpc("sql.query", {
      sql: "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' " +
           "ORDER BY (name LIKE 'uapp_%' OR name = 'sqlar'), type DESC, name",
    });
  } catch (e) { nav.innerHTML = `<p class="hint">${esc(e.message)}</p>`; return; }
  nav.innerHTML = "";
  pick.innerHTML = "";
  let lastInternal = null, group = null;
  for (const [type, name] of r.rows) {
    const internal = isInternalTable(name);
    if (internal !== lastInternal) {
      lastInternal = internal;
      const label = internal ? S.sql.sectionInternals : type === "view" ? S.sql.sectionTablesViews : S.sql.sectionTables;
      const s = div("tsection");
      s.textContent = label;
      nav.appendChild(s);
      group = document.createElement("optgroup");
      group.label = label;
      pick.appendChild(group);
    }
    const b = document.createElement("button");
    b.className = "tblrow" + (internal ? " internal" : "");
    b.dataset.tbl = name;
    b.innerHTML = `<span class="ticon">${I(type === "view" ? "book-open" : "table-2", 14)}</span>` +
      `<span class="tname" title="${esc(name)}">${esc(name)}</span>`;
    b.onclick = () => browseTable(name);
    b.oncontextmenu = (ev) => { ev.preventDefault(); tableMenu(name, type, ev.clientX, ev.clientY); };
    if (keepSelection && name === sq.table) { b.classList.add("active"); b.setAttribute("aria-current", "true"); }
    nav.appendChild(b);
    group.appendChild(new Option(name, name, false, keepSelection && name === sq.table));
  }
  if (!r.rows.length) nav.innerHTML = `<p class="hint">${esc(S.sql.noTables)}</p>`;
  $("sql-dbinfo").textContent = S.sql.objectCount(r.rows.length);
  // The browsed table can vanish (dropped here or on another device).
  if (sq.table && !r.rows.some(([, n]) => n === sq.table)) resetBrowse();
  // Nothing picked yet: start on the app's first own table instead of an
  // empty pane that only says "pick a table".
  if (!sq.table) {
    const first = r.rows.find(([, n]) => !isInternalTable(n));
    if (first) browseTable(first[1]);
    else pick.value = "";
  }
}
function resetBrowse() {
  sq.table = null;
  $("br-title").textContent = "";
  $("br-count").textContent = "";
  $("br-result").innerHTML = `<p class="hint pad">${esc(S.sql.pickTableHint)}</p>`;
  $("br-filters").innerHTML = "";
  $("br-filters").classList.add("hidden");
  $("br-schemabox").classList.add("hidden");
  $("br-pager").classList.add("hidden");
  for (const id of ["br-filter", "br-insert", "br-schema", "br-sqlbtn", "br-export", "br-more"]) $(id).classList.add("hidden");
}
const brError = (e) => { $("br-result").innerHTML = `<p class="sqlmsg err">${esc(e.message)}</p>`; };

async function browseTable(name) {
  document.querySelectorAll("#sql-tables .tblrow").forEach((b) => {
    b.classList.toggle("active", b.dataset.tbl === name);
    b.toggleAttribute("aria-current", b.dataset.tbl === name);
  });
  $("sql-table-select").value = name;
  setSqlTab("browse");
  if (sq.table !== name) {
    Object.assign(sq, { table: name, sort: null, page: 0, colWidths: {} });
    $("br-filters").innerHTML = "";
    $("br-filters").classList.add("hidden");
    $("br-schemabox").classList.add("hidden");
  }
  try {
    const schema = await rpc("sql.query", {
      sql: "SELECT type, sql FROM sqlite_master WHERE name = ?", params: [name],
    });
    sq.type = schema.rows[0]?.[0] || "table";
    sq.ddl = String(schema.rows[0]?.[1] || "");
    // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk.
    const info = await rpc("sql.query", { sql: `PRAGMA table_info(${qid(name)})`, params: [] });
    const pk = info.rows.filter((c) => c[5] > 0).sort((a, b) => a[5] - b[5]).map((c) => c[1]);
    sq.useRowid = !pk.length && sq.type === "table" && !/WITHOUT\s+ROWID/i.test(sq.ddl);
    sq.keyCols = sq.useRowid ? ["rowid"] : pk;
    sq.tableCols = info.rows.map((c) => c[1]);
    sq.colTypes = Object.fromEntries(info.rows.map((c) => [c[1], c[2] || ""]));
    sq.notNull = new Set(info.rows.filter((c) => c[3] && c[4] == null).map((c) => c[1]));
    await runBrowse();
  } catch (e) { brError(e); }
}

// ---- filters: each row is AND/OR · column · operator · value ----
const QB_OPS = [
  ["eq", "="], ["ne", "≠"], ["gt", ">"], ["lt", "<"], ["ge", "≥"], ["le", "≤"],
  ["contains", S.sql.opContains], ["starts", S.sql.opStarts], ["ends", S.sql.opEnds],
  ["like", "LIKE"], ["notlike", "NOT LIKE"], ["in", S.sql.opIn],
  ["isnull", S.sql.opEmpty], ["notnull", S.sql.opNotEmpty],
];
let filterTimer = null;
const applyFiltersSoon = () => { clearTimeout(filterTimer); filterTimer = setTimeout(applyFilters, 350); };
function applyFilters() {
  clearTimeout(filterTimer);
  sq.page = 0;
  runBrowse();
}
function addFilterRow(col) {
  if (!sq.table) return;
  const box = $("br-filters");
  box.classList.remove("hidden");
  const row = div("qb-row");
  const andSel = document.createElement("select");
  andSel.className = "mini qb-and";
  andSel.title = S.sql.filterCombine;
  andSel.append(new Option("AND", "and"), new Option("OR", "or"));
  const colSel = document.createElement("select");
  colSel.className = "mini qb-col";
  for (const c of sq.tableCols) colSel.appendChild(new Option(c, c));
  if (col) colSel.value = col;
  const opSel = document.createElement("select");
  opSel.className = "mini qb-op";
  for (const [v, label] of QB_OPS) opSel.appendChild(new Option(label, v));
  const val = document.createElement("input");
  val.className = "mini qb-val";
  val.placeholder = S.sql.filterValue;
  const del = document.createElement("button");
  del.className = "icon-btn qb-del";
  del.title = S.sql.filterRemove;
  del.innerHTML = I("x", 12);
  del.onclick = () => { row.remove(); syncFilterRows(); applyFilters(); };
  row.append(andSel, colSel, opSel, val, del);
  box.appendChild(row);
  syncFilterRows();
  for (const el of [andSel, colSel]) el.onchange = applyFilters;
  opSel.onchange = () => {
    val.classList.toggle("hidden", opSel.value === "isnull" || opSel.value === "notnull");
    applyFilters();
  };
  val.addEventListener("input", applyFiltersSoon);
  val.addEventListener("keydown", (e) => { if (e.key === "Enter" && !isComposing(e)) applyFilters(); });
  val.focus();
}
/// Exactly the non-first rows show an AND/OR connector.
function syncFilterRows() {
  const rows = [...document.querySelectorAll("#br-filters .qb-row")];
  rows.forEach((row, i) => row.querySelector(".qb-and").classList.toggle("hidden", i === 0));
  if (!rows.length) $("br-filters").classList.add("hidden");
}
function brWhere() {
  const parts = [], params = [];
  for (const row of document.querySelectorAll("#br-filters .qb-row")) {
    const col = row.querySelector(".qb-col").value;
    const op = row.querySelector(".qb-op").value;
    const v = row.querySelector(".qb-val").value;
    if (!col) continue;
    const c = qid(col);
    let cond = null;
    switch (op) {
      case "isnull": cond = `${c} IS NULL`; break;
      case "notnull": cond = `${c} IS NOT NULL`; break;
      case "in": {
        const items = v.split(",").map((s) => s.trim()).filter((s) => s !== "");
        if (items.length) {
          cond = `${c} IN (${items.map(() => "?").join(", ")})`;
          params.push(...items.map(coerce));
        }
        break;
      }
      case "contains": if (v !== "") { cond = `${c} LIKE ?`; params.push("%" + v + "%"); } break;
      case "starts": if (v !== "") { cond = `${c} LIKE ?`; params.push(v + "%"); } break;
      case "ends": if (v !== "") { cond = `${c} LIKE ?`; params.push("%" + v); } break;
      case "like": if (v !== "") { cond = `${c} LIKE ?`; params.push(v); } break;
      case "notlike": if (v !== "") { cond = `${c} NOT LIKE ?`; params.push(v); } break;
      default: {
        const o = { eq: "=", ne: "!=", gt: ">", lt: "<", ge: ">=", le: "<=" }[op] || "=";
        if (v !== "") { cond = `${c} ${o} ?`; params.push(coerce(v)); }
      }
    }
    if (cond) parts.push({ cond, and: row.querySelector(".qb-and").value !== "or" });
  }
  if (!parts.length) return { where: "", params: [] };
  // Group left-to-right so mixed AND/OR reads the way the rows do, instead
  // of following SQLite's AND-beats-OR precedence.
  const where = parts.reduce((acc, p, i) => (i === 0 ? p.cond : `(${acc} ${p.and ? "AND" : "OR"} ${p.cond})`), "");
  return { where, params };
}

async function runBrowse() {
  if (!sq.table) return;
  const { where, params } = brWhere();
  sq.filtered = !!where;
  try {
    const cnt = await rpc("sql.query", {
      sql: `SELECT COUNT(*) FROM ${qid(sq.table)}` + (where ? ` WHERE ${where}` : ""), params,
    });
    sq.total = cnt.rows[0]?.[0] ?? 0;
    const lastPage = Math.max(0, Math.ceil(sq.total / sq.pageSize) - 1);
    if (sq.page > lastPage) sq.page = lastPage;
    let sql = `SELECT ${sq.useRowid ? "rowid, *" : "*"} FROM ${qid(sq.table)}`;
    if (where) sql += ` WHERE ${where}`;
    if (sq.sort) sql += ` ORDER BY ${qid(sq.sort.col)} ${sq.sort.dir}`;
    sql += ` LIMIT ${sq.pageSize} OFFSET ${sq.page * sq.pageSize}`;
    const data = await rpc("sql.query", { sql, params });
    renderBrowse(data);
  } catch (e) { brError(e); }
}
function renderBrowse(data) {
  $("br-title").textContent = sq.table;
  $("br-count").textContent =
    S.sql.browseCount(sq.total, sq.filtered) +
    (sq.type === "view" ? S.sql.browseViewHint : canEdit() ? S.sql.browseEditHint : "");
  $("br-filter").classList.remove("hidden");
  $("br-schema").classList.remove("hidden");
  $("br-sqlbtn").classList.remove("hidden");
  $("br-export").classList.remove("hidden");
  $("br-more").classList.remove("hidden");
  $("br-insert").classList.toggle("hidden", !canEdit());
  $("br-schemabox").textContent = sq.ddl || S.sql.noSchema;
  const editCtx = canEdit() ? { table: sq.table, keyCols: sq.keyCols, cols: data.columns } : null;
  renderResultInto($("br-result"), data.columns, data.rows, editCtx, { sortable: true, refresh: runBrowse });
  const pages = Math.ceil(sq.total / sq.pageSize) || 1;
  $("br-pager").classList.toggle("hidden", pages <= 1);
  if (pages > 1) {
    const a = sq.page * sq.pageSize + 1;
    const b = Math.min(sq.total, (sq.page + 1) * sq.pageSize);
    $("br-range").textContent = S.sql.range(a, b, sq.total);
    $("br-prev").disabled = sq.page === 0;
    $("br-next").disabled = sq.page >= pages - 1;
  }
}

function tableMenu(name, type, x, y) {
  const items = [
    { icon: "table-2", label: S.sql.ctxBrowse, run: () => browseTable(name) },
    { icon: "copy", label: S.sql.ctxCopyName, run: () => copyText(name) },
    { icon: "square-terminal", label: S.sql.ctxCopySelect, run: () => copyText(`SELECT * FROM ${qid(name)} LIMIT 100;`) },
  ];
  if (!isInternalTable(name)) {
    const kind = type === "view" ? S.sql.kindView : S.sql.kindTable;
    items.push({ sep: true });
    if (type === "table") {
      items.push({
        icon: "eraser", label: S.sql.ctxDeleteAllRows, danger: true, run: async () => {
          if (!(await dlgConfirm(S.sql.deleteAllConfirm(name)))) return;
          try {
            await rpc("sql.exec", { sql: `DELETE FROM ${qid(name)}`, params: [] });
            if (sq.table === name) runBrowse();
          } catch (e) { dlgAlert(e.message); }
        },
      });
    }
    items.push({
      icon: "trash-2", label: S.sql.ctxDropTable(kind), danger: true, run: async () => {
        if (!(await dlgConfirm(S.sql.dropConfirm(kind, name)))) return;
        try {
          await rpc("sql.exec", { sql: `DROP ${type === "view" ? "VIEW" : "TABLE"} ${qid(name)}`, params: [] });
          if (sq.table === name) resetBrowse();
          loadSqlTables(true);
        } catch (e) { dlgAlert(e.message); }
      },
    });
  }
  showMenu(items, x, y);
}

async function insertRowDialog() {
  if (!canEdit()) return;
  const d = makeDialog(S.sql.insertTitle(sq.table), "table-2");
  const body = d.querySelector(".dlg-body");
  const hint = document.createElement("p");
  hint.className = "hint";
  hint.textContent = S.sql.insertHint;
  body.appendChild(hint);
  const inputs = [];
  for (const c of sq.tableCols) {
    const lab = document.createElement("label");
    lab.className = "ins-row";
    lab.innerHTML = `<span class="ins-col">${esc(c)}<i>${esc(sq.colTypes[c] || "")}</i></span>`;
    const inp = document.createElement("input");
    inp.className = "mini";
    if (sq.notNull.has(c) && !sq.keyCols.includes(c)) inp.placeholder = S.sql.required;
    lab.appendChild(inp);
    body.appendChild(lab);
    inputs.push([c, inp]);
  }
  const msg = document.createElement("p");
  msg.className = "hint err";
  body.appendChild(msg);
  footBtn(d, S.common.cancel).onclick = () => d.close();
  const ok = footBtn(d, S.sql.insertBtn, { primary: true });
  ok.onclick = async () => {
    const names = [], vals = [];
    for (const [c, inp] of inputs) {
      if (inp.value === "") continue;
      names.push(qid(c));
      vals.push(/^null$/i.test(inp.value.trim()) ? null : coerce(inp.value));
    }
    const sql = names.length
      ? `INSERT INTO ${qid(sq.table)} (${names.join(", ")}) VALUES (${names.map(() => "?").join(", ")})`
      : `INSERT INTO ${qid(sq.table)} DEFAULT VALUES`;
    ok.disabled = true;
    try {
      await rpc("sql.exec", { sql, params: vals });
      d.close();
      runBrowse();
    } catch (e) { ok.disabled = false; msg.textContent = e.message; }
  };
  d.showModal();
  if (inputs.length) inputs[0][1].focus();
}

// ---- export: CSV / TSV / XLSX (shared by Browse and the SQL tab) ----
const EXPORT_MAX = 100000; // rows per file — beyond this the export is cut short
function exportMenu(btn, run) {
  menuFrom(btn, [
    { icon: "file-text", label: S.sql.exportCsv, run: () => run("csv") },
    { icon: "file-text", label: S.sql.exportTsv, run: () => run("tsv") },
    { icon: "table-2", label: S.sql.exportXlsx, run: () => run("xlsx") },
  ], S.sql.exportLabel);
}
function exportRows(base, cols, rows, fmt) {
  if (fmt === "xlsx") {
    downloadBlob(base + ".xlsx",
      "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
      toXlsx(cols, rows, base));
  } else if (fmt === "tsv") {
    downloadBlob(base + ".tsv", "text/tab-separated-values;charset=utf-8", toTsv(cols, rows));
  } else {
    downloadBlob(base + ".csv", "text/csv;charset=utf-8", toCsv(cols, rows));
  }
}
/// Export the browsed table: ALL matching rows (current filters + sort), not
/// just the visible page — capped at EXPORT_MAX with a heads-up.
async function exportBrowse(fmt) {
  if (!sq.table) return;
  const { where, params } = brWhere();
  let sql = `SELECT * FROM ${qid(sq.table)}`;
  if (where) sql += ` WHERE ${where}`;
  if (sq.sort) sql += ` ORDER BY ${qid(sq.sort.col)} ${sq.sort.dir}`;
  sql += ` LIMIT ${EXPORT_MAX + 1}`; // one extra row = "there was more"
  try {
    const r = await rpc("sql.query", { sql, params });
    let rows = r.rows;
    if (rows.length > EXPORT_MAX) {
      rows = rows.slice(0, EXPORT_MAX);
      dlgAlert(S.sql.exportTooMany(EXPORT_MAX.toLocaleString(getLocale())));
    }
    exportRows(sq.table, r.columns, rows, fmt);
  } catch (e) { dlgAlert(S.sql.exportFailed(e.message)); }
}

// ---- result grid (shared by Browse and the SQL tab) ----
/// UPDATE one cell, addressed by the table's key columns (values taken from
/// the fetched row). Replicated to every device like any other write.
async function updateCell(editCtx, colIdx, rowVals, v) {
  const kIdx = editCtx.keyCols.map((k) => editCtx.cols.indexOf(k));
  if (kIdx.some((i) => i < 0)) throw new Error(S.sql.keyColsMissing);
  const where = editCtx.keyCols.map((k) => `${qid(k)} = ?`).join(" AND ");
  await rpc("sql.exec", {
    sql: `UPDATE ${qid(editCtx.table)} SET ${qid(editCtx.cols[colIdx])} = ? WHERE ${where}`,
    params: [v, ...kIdx.map((i) => rowVals[i])],
  });
  rowVals[colIdx] = v;
}
// ---- column sizing ----
/// Freeze the grid's layout: every column gets its current rendered width as
/// an explicit one and the table switches to fixed layout, so a drag or the
/// cell editor changes exactly one column and nothing else reflows.
/// (table-layout: fixed needs an explicit table width — Chrome ignores it
/// with width: auto — so the width is kept at the sum of the columns; the
/// pane scrolls when that exceeds it.)
function freezeCols(t) {
  if (t.classList.contains("frozen")) return;
  const ths = [...t.tHead.rows[0].cells];
  const widths = ths.map((th) => th.getBoundingClientRect().width);
  ths.forEach((th, i) => { th.style.width = widths[i] + "px"; });
  t.classList.add("frozen");
  syncTableWidth(t);
}
const syncTableWidth = (t) => {
  t.style.width = [...t.tHead.rows[0].cells]
    .reduce((s, th) => s + parseFloat(th.style.width), 0) + "px";
};
/// Drag a header grip. Browse widths land in sq.colWidths (by column name) so
/// they survive the re-renders sort / page / filter cause.
function startColDrag(ev, t, th, persist) {
  ev.stopPropagation();
  ev.preventDefault();
  freezeCols(t);
  const remember = () => {
    if (!persist) return;
    for (const h of t.tHead.rows[0].cells)
      if (h.dataset.col !== undefined) sq.colWidths[h.dataset.col] = parseFloat(h.style.width);
  };
  remember();
  const x0 = ev.clientX, w0 = parseFloat(th.style.width);
  const move = (e) => {
    th.style.width = Math.max(30, w0 + e.clientX - x0) + "px";
    syncTableWidth(t);
    remember();
  };
  const up = () => {
    window.removeEventListener("mousemove", move);
    window.removeEventListener("mouseup", up);
  };
  window.addEventListener("mousemove", move);
  window.addEventListener("mouseup", up);
}

// Click-to-edit: the cell becomes an input; blur or Enter saves, Escape cancels.
// Started from the keyboard, focus goes back to the cell afterwards.
function editCell(td, editCtx, colIdx, rowVals) {
  if (td.querySelector("input")) return;
  freezeCols(td.closest("table")); // the input must not change the column width
  const orig = rowVals[colIdx];
  const isBlob = typeof orig === "object" && orig !== null && orig.b64;
  const input = document.createElement("input");
  input.className = "cell-edit";
  input.value = orig === null || isBlob ? "" : String(orig);
  input.placeholder = orig === null ? S.sql.cellNull : isBlob ? S.sql.cellEditBlobPlaceholder : "";
  input.title = S.sql.cellEditInputTitle;
  td.textContent = "";
  td.appendChild(input);
  td.classList.add("editing");
  input.focus();
  if (!isBlob) input.select();
  let done = false;
  const restore = () => {
    td.classList.remove("editing", "saved");
    renderCell(td, orig);
  };
  const finish = async (save) => {
    if (done) return;
    done = true;
    const raw = input.value;
    if (!save || raw === (orig === null || isBlob ? "" : String(orig))) return restore();
    const v = /^null$/i.test(raw.trim()) ? null : coerce(raw);
    input.disabled = true;
    try {
      await updateCell(editCtx, colIdx, rowVals, v);
      td.classList.remove("editing");
      td.classList.add("saved");
      renderCell(td, v);
      setTimeout(() => td.classList.remove("saved"), 1200);
    } catch (e) {
      input.disabled = false;
      dlgAlert(S.sql.updateFailed(e.message));
      restore();
    }
  };
  input.onblur = () => finish(true);
  input.onkeydown = (e) => {
    if (e.key === "Enter" && !isComposing(e)) { e.preventDefault(); input.blur(); td.focus(); }
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); done = true; restore(); td.focus(); }
  };
}
async function setCellNull(td, editCtx, colIdx, rowVals) {
  try {
    await updateCell(editCtx, colIdx, rowVals, null);
    renderCell(td, null);
    td.classList.add("saved");
    setTimeout(() => td.classList.remove("saved"), 1200);
  } catch (e) { dlgAlert(S.sql.updateFailed(e.message)); }
}
async function deleteRow(editCtx, rowVals, refresh) {
  const kIdx = editCtx.keyCols.map((k) => editCtx.cols.indexOf(k));
  if (kIdx.some((i) => i < 0)) return;
  const label = editCtx.keyCols.map((k, j) => `${k} = ${rowVals[kIdx[j]]}`).join(", ");
  if (!(await dlgConfirm(S.sql.deleteRowConfirm(label)))) return;
  const where = editCtx.keyCols.map((k) => `${qid(k)} = ?`).join(" AND ");
  try {
    await rpc("sql.exec", {
      sql: `DELETE FROM ${qid(editCtx.table)} WHERE ${where}`,
      params: kIdx.map((i) => rowVals[i]),
    });
    refresh && refresh();
  } catch (e) { dlgAlert(S.sql.deleteFailed(e.message)); }
}
const cellText = (v) =>
  v === null ? S.sql.cellNull : typeof v === "object" && v && v.b64 ? S.sql.cellBlob : String(v);
function cellMenu(ev, td, cols, row, i, editCtx, refresh) {
  ev.preventDefault();
  if (ev.clientX === undefined) { const r = td.getBoundingClientRect(); ev = { clientX: r.left, clientY: r.bottom }; }
  const items = [
    { icon: "copy", label: S.sql.ctxCopyCell, run: () => copyText(cellText(row[i])) },
    { icon: "copy", label: S.sql.ctxCopyRowJson,
      run: () => copyText(JSON.stringify(Object.fromEntries(cols.map((c, k) => [c, row[k]])), null, 2)) },
    { icon: "copy", label: S.sql.ctxCopyRowTsv,
      run: () => copyText(row.map((x) => (x === null ? "" : cellText(x))).join("\t")) },
  ];
  if (editCtx && !editCtx.keyCols.includes(cols[i])) {
    items.push({ sep: true });
    items.push({ icon: "pencil", label: S.sql.ctxEditCell, run: () => editCell(td, editCtx, i, row) });
    if (row[i] !== null) items.push({ icon: "eraser", label: S.sql.ctxSetNull, run: () => setCellNull(td, editCtx, i, row) });
  }
  if (editCtx) {
    items.push({ sep: true });
    items.push({ icon: "trash-2", label: S.sql.ctxDeleteRow, danger: true, run: () => deleteRow(editCtx, row, refresh) });
  }
  showMenu(items, ev.clientX, ev.clientY);
}
function renderCell(td, v) {
  td.textContent = "";
  td.classList.remove("null", "num", "editing", "saved"); // keep "editable"
  if (v === null) { td.textContent = S.sql.cellNull; td.classList.add("null"); }
  else if (typeof v === "object" && v.b64) {
    const n = Math.round(v.b64.length * 0.75);
    td.textContent = S.sql.cellBlobSize(fmtSize(n));
    td.title = S.sql.cellBlobTitle(n);
  } else {
    td.textContent = String(v);
    if (typeof v === "number") td.classList.add("num");
    if (td.textContent.length > 40) td.title = td.textContent;
  }
}
function renderResultInto(holder, cols, rows, editCtx, opts = {}) {
  holder.innerHTML = "";
  if (!cols || !cols.length) return;
  const t = document.createElement("table");
  t.className = "rtable";
  const hr = t.createTHead().insertRow();
  cols.forEach((c) => {
    const th = document.createElement("th");
    if (opts.sortable) {
      // The header is a real button (keyboard reachable); aria-sort says
      // which way the column is sorted.
      const dir = sq.sort && sq.sort.col === c ? sq.sort.dir : null;
      th.classList.add("sortable");
      th.dataset.col = c;
      th.setAttribute("aria-sort", dir === "ASC" ? "ascending" : dir === "DESC" ? "descending" : "none");
      const b = document.createElement("button");
      b.type = "button";
      b.className = "th-sort";
      b.title = S.sql.thSortTitle;
      b.innerHTML = `<span>${esc(c)}</span>` +
        (dir ? `<span class="sort-ico">${I(dir === "ASC" ? "arrow-up" : "arrow-down", 12)}</span>` : "");
      b.onclick = () => {
        sq.sort = !sq.sort || sq.sort.col !== c ? { col: c, dir: "ASC" }
          : sq.sort.dir === "ASC" ? { col: c, dir: "DESC" } : null;
        sq.page = 0;
        runBrowse();
      };
      th.appendChild(b);
      // Re-apply the dragged width across re-renders (sort / page / filter).
      if (sq.colWidths[c] !== undefined) th.style.width = sq.colWidths[c] + "px";
    } else {
      th.textContent = c;
    }
    const grip = document.createElement("span");
    grip.className = "col-grip";
    grip.title = S.sql.gripTitle;
    grip.onclick = (e) => e.stopPropagation(); // never sort from the grip
    grip.onmousedown = (e) => startColDrag(e, t, th, !!opts.sortable);
    th.appendChild(grip);
    hr.appendChild(th);
  });
  if (opts.sortable && cols.length && cols.every((c) => sq.colWidths[c] !== undefined)) {
    t.classList.add("frozen");
    syncTableWidth(t);
  }
  const tbody = t.createTBody();
  for (const row of rows || []) {
    const tr = tbody.insertRow();
    row.forEach((v, i) => {
      const td = tr.insertCell();
      td.tabIndex = -1;
      renderCell(td, v);
      const editable = editCtx && !editCtx.keyCols.includes(cols[i]);
      if (editable) {
        td.classList.add("editable");
        td.title = S.sql.cellEditTitle;
        td.onclick = () => editCell(td, editCtx, i, row);
      }
      td.oncontextmenu = (ev) => cellMenu(ev, td, cols, row, i, editCtx, opts.refresh);
      // Keyboard: Enter (or F2) edits, the menu key or Shift+F10 opens the
      // cell's menu.
      td.onkeydown = (ev) => {
        if (ev.target !== td) return;
        if ((ev.key === "Enter" || ev.key === "F2") && editable && !isComposing(ev)) {
          ev.preventDefault();
          editCell(td, editCtx, i, row);
        } else if (ev.key === "ContextMenu" || (ev.key === "F10" && ev.shiftKey)) {
          cellMenu(ev, td, cols, row, i, editCtx, opts.refresh);
        }
      };
    });
  }
  // One tab stop for the whole grid; arrow keys move between cells.
  const first = tbody.rows[0]?.cells[0];
  if (first) first.tabIndex = 0;
  t.addEventListener("focusin", (ev) => {
    const td = ev.target.closest?.("td");
    if (!td || td.tabIndex === 0) return;
    t.querySelectorAll('td[tabindex="0"]').forEach((c) => (c.tabIndex = -1));
    td.tabIndex = 0;
  });
  t.addEventListener("keydown", (ev) => {
    const td = ev.target;
    if (td.tagName !== "TD") return;
    const tr = td.parentElement, c = td.cellIndex, rows = tbody.rows;
    let to = null;
    if (ev.key === "ArrowRight") to = tr.cells[c + 1];
    else if (ev.key === "ArrowLeft") to = tr.cells[c - 1];
    else if (ev.key === "ArrowDown") to = rows[tr.sectionRowIndex + 1]?.cells[c];
    else if (ev.key === "ArrowUp") to = rows[tr.sectionRowIndex - 1]?.cells[c];
    else if (ev.key === "Home") to = ev.ctrlKey ? rows[0]?.cells[0] : tr.cells[0];
    else if (ev.key === "End") to = ev.ctrlKey ? rows[rows.length - 1]?.cells[c] : tr.cells[tr.cells.length - 1];
    else return;
    ev.preventDefault();
    if (to) { to.focus(); to.scrollIntoView({ block: "nearest", inline: "nearest" }); }
  });
  holder.appendChild(t);
}

// ---- the SQL tab: raw editor ----
let lastSqlResult = null; // {cols, rows} of the last successful query, for export
function renderResultBox(cols, rows, note) {
  const out = $("sql-result");
  out.innerHTML = "";
  const hasCols = !!(cols && cols.length);
  if (note || hasCols) {
    const m = div("sqlmsg note-row");
    const label = document.createElement("span");
    label.innerHTML = note || "";
    m.appendChild(label);
    if (hasCols) {
      m.appendChild(div("spacer"));
      const b = document.createElement("button");
      b.id = "sql-export";
      b.className = "chip-btn";
      b.title = S.sql.exportResult;
      b.innerHTML = I("download", 12) + " " + S.sql.exportLabel;
      b.onclick = () => exportMenu(b, (fmt) =>
        lastSqlResult && exportRows("query", lastSqlResult.cols, lastSqlResult.rows, fmt));
      m.appendChild(b);
    }
    out.appendChild(m);
  }
  if (!hasCols) return;
  const holder = div("");
  out.appendChild(holder);
  renderResultInto(holder, cols, rows, null, {});
}
const sqlError = (e) => { $("sql-result").innerHTML = `<p class="sqlmsg err">${esc(e.message)}</p>`; };
/// How many statements the editor holds (semicolons inside quotes don't count).
/// sql.exec runs exactly ONE statement — several must go through sql.batch,
/// or everything after the first semicolon would be silently ignored.
function stmtCount(sql) {
  let n = 0, cur = "", q = null;
  for (let i = 0; i < sql.length; i++) {
    const ch = sql[i];
    if (q) { if (ch === q && sql[i + 1] === q) i++; else if (ch === q) q = null; continue; }
    if (ch === "'" || ch === '"') { q = ch; continue; }
    if (ch === ";") { if (cur.replace(/--[^\n]*/g, "").trim()) n++; cur = ""; continue; }
    cur += ch;
  }
  if (cur.replace(/--[^\n]*/g, "").trim()) n++;
  return n;
}
async function runSql() {
  const sql = $("sql-input").value.trim();
  if (!sql) return;
  const btn = $("btn-sql-run");
  btn.disabled = true;
  try {
    const first = sql.replace(/--[^\n]*/g, "").trim().toLowerCase();
    const readonly = /^(select|with|explain|pragma)\b/.test(first);
    const multi = stmtCount(sql) > 1;
    const r = readonly && !multi
      ? await rpc("sql.query", { sql, params: [] })
      : multi
      ? await rpc("sql.batch", { sql })
      : await rpc("sql.exec", { sql, params: [] });
    if (r.ok && !r.columns) {
      $("sql-result").innerHTML =
        `<p class="sqlmsg">${I("check", 13)} ${esc(S.sql.batchOk(stmtCount(sql)))}</p>`;
      loadSqlTables(true);
      if (sq.table) runBrowse();
      return;
    }
    if (r.columns) {
      lastSqlResult = { cols: r.columns, rows: r.rows };
      renderResultBox(r.columns, r.rows,
        S.sql.queryRows(r.rows.length) +
        (r.changes !== undefined ? ` · ${S.sql.queryChanged(r.changes)}` : ""));
    } else {
      $("sql-result").innerHTML =
        `<p class="sqlmsg">${I("check", 13)} ${esc(S.sql.execOk(r.changes ?? 0, r.insert_id))}</p>`;
    }
    // A write may have changed schema or the browsed table — refresh both.
    if (!readonly) { loadSqlTables(true); if (sq.table) runBrowse(); }
  } catch (e) { sqlError(e); }
  finally { btn.disabled = false; }
}

// ---------- component ----------
class SqlPanel extends HTMLElement {
  connectedCallback() {
    installStyle("sql-panel", CSS);
    this.innerHTML = TPL;
    wire();
  }
}

function wire() {
  registerPanel("sqlpanel", "btn-sql", () => loadSqlTables(true), S.sql.title);

  $("sql-head-icon").innerHTML = I("database", 16);
  $("btn-sql-run").innerHTML = I("play", 12) + " " + S.sql.run;
  $("btn-sql-refresh").innerHTML = I("refresh-cw", 16);
  $("btn-sql-close").innerHTML = I("x", 16);
  $("br-more").innerHTML = I("ellipsis-vertical", 16);
  $("sql-table-select").onchange = (e) => { if (e.target.value) browseTable(e.target.value); };
  // Phones: the toolbar chips, folded into one menu (same actions, same
  // visibility rules: each item just clicks its chip).
  $("br-more").onclick = () => {
    const chip = (id, icon, label) => $(id).classList.contains("hidden") ? null
      : { icon, label, run: () => $(id).click() };
    menuFrom($("br-more"), [
      chip("br-filter", "list-filter", S.sql.addFilter),
      chip("br-insert", "plus", S.sql.insertRow),
      chip("br-schema", "file-code", S.sql.showSchema),
      chip("br-sqlbtn", "square-terminal", S.sql.openInSql),
      chip("br-export", "download", S.sql.exportRows),
    ].filter(Boolean), S.files.more);
  };
  $("br-filter").innerHTML = I("list-filter", 12) + " " + S.sql.filterBtn;
  $("br-insert").innerHTML = I("plus", 12) + " " + S.sql.rowBtn;
  $("br-schema").innerHTML = I("file-code", 12) + " " + S.sql.schemaBtn;
  $("br-sqlbtn").innerHTML = I("square-terminal", 12) + " " + S.sql.sqlBtn;
  $("br-export").innerHTML = I("download", 12) + " " + S.sql.exportBtn;
  $("br-prev").innerHTML = I("chevron-left", 16);
  $("br-next").innerHTML = I("chevron-right", 16);

  $("tab-browse").onclick = () => setSqlTab("browse");
  $("tab-sql").onclick = () => setSqlTab("sql");
  $("sql-tabs").addEventListener("keydown", (e) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return;
    e.preventDefault();
    const next = e.key === "Home" ? "browse" : e.key === "End" ? "sql"
      : $("tab-browse").classList.contains("active") ? "sql" : "browse";
    setSqlTab(next);
    $(next === "sql" ? "tab-sql" : "tab-browse").focus();
  });
  $("btn-sql-run").onclick = runSql;
  $("sql-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && !isComposing(e)) { e.preventDefault(); runSql(); }
  });
  $("btn-sql-refresh").onclick = () => loadSqlTables(true);
  $("btn-sql-close").onclick = () => closePanel("sqlpanel");
  $("br-prev").onclick = () => { if (sq.page > 0) { sq.page--; runBrowse(); } };
  $("br-next").onclick = () => { sq.page++; runBrowse(); };
  $("br-filter").onclick = () => addFilterRow();
  $("br-schema").onclick = () => $("br-schemabox").classList.toggle("hidden");
  $("br-insert").onclick = () => insertRowDialog();
  $("br-export").onclick = () => exportMenu($("br-export"), exportBrowse);
  // Hand the exact browse query to the SQL tab (literals inlined, since the
  // editor has no parameter box).
  $("br-sqlbtn").onclick = () => {
    if (!sq.table) return;
    const { where, params } = brWhere();
    let k = 0;
    const w = where.replace(/\?/g, () => sqlLit(params[k++]));
    $("sql-input").value = `SELECT * FROM ${qid(sq.table)}` +
      (w ? `\nWHERE ${w}` : "") +
      (sq.sort ? `\nORDER BY ${qid(sq.sort.col)} ${sq.sort.dir}` : "") +
      `\nLIMIT ${sq.pageSize};`;
    setSqlTab("sql");
  };
}

customElements.define("sql-panel", SqlPanel);
