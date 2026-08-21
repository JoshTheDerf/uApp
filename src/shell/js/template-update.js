/* Drag a template .uapp onto a running app to update it.
 *
 * A .uapp dropped on the window is almost never meant to become a file inside
 * the app — it's a newer version of the app itself. So it never goes through
 * the upload path: the server stages it (`template.inspect`), we show exactly
 * what would change, and only a confirmed dialog applies it
 * (`template.apply`). The user's rows, uploads, chat and settings are never
 * part of an update; see src/template.rs for the precise rules. */

import { rpc } from "./core.js";
import {
  $, I, esc, installStyle, makeDialog, footBtn, dlgAlert, toast,
} from "./ui.js";
import { S } from "./strings.js";

const MAX_TEMPLATE = 100 * 1024 * 1024;

const CSS = /* css */ `
.tplu-lead { margin: 0 0 8px; font-size: 13.5px; line-height: 1.5; }
.tplu-lead b { font-weight: 600; }
.tplu-sum { margin: 0; padding-left: 18px; font-size: 13px; line-height: 1.7; }
.tplu-sum li::marker { color: var(--faint); }
.dlg-body label.tplu-keep {
  display: flex; gap: 8px; align-items: flex-start; margin: 12px 0 0;
  padding: 8px 10px; border: 1px solid var(--line); border-radius: var(--r-s);
  background: var(--surface-2); font-size: 13px; line-height: 1.45;
  color: var(--text); cursor: pointer;
}
.dlg-body label.tplu-keep > input { width: auto; flex: none; margin: 2px 0 0; }
.tplu-det { margin-top: 10px; font-size: 12.5px; }
.tplu-det summary { cursor: pointer; color: var(--text-2); }
.tplu-det b { display: block; margin-top: 9px; color: var(--text); }
.tplu-det ul { margin: 4px 0 0; padding-left: 18px; max-height: 190px; overflow: auto; }
.tplu-det li { font-family: var(--mono); font-size: 11.5px; line-height: 1.6; color: var(--text-2); }
.tplu-warn {
  margin-top: 12px; padding: 8px 10px; border: 1px solid var(--warn-line);
  border-radius: var(--r-s); background: var(--warn-bg); color: var(--warn-ink);
  font-size: 12.5px; line-height: 1.5;
}
.tplu-warn ul { margin: 4px 0 0; padding-left: 18px; }
`;

/// Does this file name look like an app, rather than content for one?
export const isUappFile = (name) => /\.uapp$/i.test(String(name || "").trim());

/// A dropped/picked File as base64 (the server never sees a browser path).
function fileB64(file) {
  return new Promise((resolve, reject) => {
    const rd = new FileReader();
    rd.onerror = () => reject(new Error(S.tpl.readFailed));
    rd.onload = () => resolve(String(rd.result).split(",", 2)[1] || "");
    rd.readAsDataURL(file);
  });
}

/// Entry point for a native (OS) window drop: the bytes stay server-side and
/// we only pass back the drop id we were notified with.
export function updateFromDrop(p) {
  return offer({ id: p.id, name: (p.names || [])[0] || "" });
}

/// Entry point for an HTML5 drop / the file picker in the browser shell.
export async function updateFromFile(file) {
  if (file.size > MAX_TEMPLATE) {
    dlgAlert(S.files.uploadTooLarge(file.name, MAX_TEMPLATE / 1048576));
    return false;
  }
  let b64;
  try {
    b64 = await fileB64(file);
  } catch (e) {
    dlgAlert(e.message);
    return false;
  }
  return offer({ b64, name: file.name });
}

/// Open the file picker and update from what the user chooses.
export function pickTemplate() {
  const inp = document.createElement("input");
  inp.type = "file";
  inp.accept = ".uapp";
  inp.onchange = () => {
    const f = inp.files && inp.files[0];
    if (f) updateFromFile(f);
  };
  inp.click();
}

/// Stage → confirm → apply. Resolves true when an update was applied.
async function offer(params) {
  installStyle("template-update", CSS);
  const wait = toast(S.tpl.checking, { progress: true, duration: 0, delay: 250 });
  let staged;
  try {
    staged = await rpc("template.inspect", params);
  } catch (e) {
    wait.dismiss();
    dlgAlert(S.tpl.cannotRead(e.message));
    return false;
  }
  wait.dismiss();
  const { token, plan } = staged;
  const decision = await confirmDialog(plan, params.name || plan.source?.file || "");
  if (!decision) {
    rpc("template.discard", { token }).catch(() => {});
    return false;
  }
  const busy = toast(S.tpl.updating, { progress: true, duration: 0 });
  try {
    const r = await rpc("template.apply", { token, remove_stale: decision.removeStale });
    busy.dismiss();
    const a = r.applied || {};
    toast(S.tpl.done(a.filesWritten || 0, a.filesRemoved || 0,
                     (a.tablesCreated || 0) + (a.columnsAdded || 0)));
    const warn = (r.warnings || []).filter(Boolean);
    if (warn.length) dlgAlert(S.tpl.appliedWithNotes + "\n\n• " + warn.join("\n• "));
    return true;
  } catch (e) {
    busy.dismiss();
    dlgAlert(S.tpl.failed(e.message));
    return false;
  }
}

/// Count everything the plan would do, so "nothing to change" can say so.
function totals(plan) {
  const f = plan.files || {}, s = plan.schema || {};
  return {
    changed: (f.changed || []).length,
    added: (f.added || []).length,
    stale: (f.stale || []).length,
    tables: (s.createTables || []).length,
    columns: (s.addColumns || []).length,
    objects: (s.createObjects || []).length + (s.replaceObjects || []).length,
    config: (plan.config?.added || []).length,
  };
}

/// The confirmation dialog. Resolves {removeStale} on Update, null otherwise.
function confirmDialog(plan, label) {
  const t = totals(plan);
  const nothing = !t.changed && !t.added && !t.stale && !t.tables && !t.columns &&
                  !t.objects && !t.config;
  const src = plan.source || {};
  const name = label || src.file || S.tpl.aTemplate;

  return new Promise((resolve) => {
    const d = makeDialog(S.tpl.title, "package");
    const body = d.querySelector(".dlg-body");
    const parts = [];
    parts.push(`<p class="tplu-lead">${S.tpl.lead(esc(name), esc(plan.target?.name || ""))}</p>`);
    if (nothing) {
      parts.push(`<p class="hint">${S.tpl.upToDate}</p>`);
    } else {
      const items = [];
      if (t.changed || t.added) items.push(S.tpl.sumFiles(t.changed, t.added));
      if (t.tables || t.columns) items.push(S.tpl.sumSchema(t.tables, t.columns));
      if (t.objects) items.push(S.tpl.sumObjects(t.objects));
      if (t.config) items.push(S.tpl.sumConfig(t.config));
      parts.push(`<ul class="tplu-sum">${items.map((i) => `<li>${esc(i)}</li>`).join("")}</ul>`);
      parts.push(`<p class="hint" style="margin-top:10px">${S.tpl.dataSafe}</p>`);
    }
    if (src.sameApp) parts.push(`<p class="hint">${S.tpl.sameLineage}</p>`);
    if (plan.ignored?.dataFiles || plan.ignored?.rows) {
      parts.push(`<p class="hint">${S.tpl.ignoringData(plan.ignored.dataFiles || 0, plan.ignored.rows || 0)}</p>`);
    }
    if (t.stale) {
      parts.push(
        `<label class="tplu-keep"><input type="checkbox" id="tplu-stale" checked>` +
        `<span>${S.tpl.removeStale(t.stale)}</span></label>`);
    }
    // The full lists, folded away: a summary is what the decision needs.
    const lists = [];
    const list = (title, names) => {
      if (!names || !names.length) return;
      lists.push(`<b>${esc(title)}</b><ul>${names.map((n) => `<li>${esc(n)}</li>`).join("")}</ul>`);
    };
    list(S.tpl.listChanged, plan.files?.changed);
    list(S.tpl.listAdded, plan.files?.added);
    list(S.tpl.listStale, plan.files?.stale);
    list(S.tpl.listTables, (plan.schema?.createTables || []).map((x) => x.table));
    list(S.tpl.listColumns, (plan.schema?.addColumns || []).map((x) => `${x.table}.${x.column}`));
    list(S.tpl.listObjects, [...(plan.schema?.createObjects || []), ...(plan.schema?.replaceObjects || [])]
      .map((x) => `${x.type} ${x.name}`));
    list(S.tpl.listKept, plan.schema?.keptTables);
    if (lists.length) {
      parts.push(`<details class="tplu-det"><summary>${S.tpl.details}</summary>${lists.join("")}</details>`);
    }
    const warn = (plan.warnings || []).filter(Boolean);
    if (warn.length) {
      parts.push(`<div class="tplu-warn">${esc(S.tpl.warnHead)}<ul>` +
        warn.map((w) => `<li>${esc(w)}</li>`).join("") + `</ul></div>`);
    }
    body.innerHTML = parts.join("");

    let result = null;
    if (nothing) {
      footBtn(d, S.common.close, { primary: true }).onclick = () => d.close();
    } else {
      footBtn(d, S.common.cancel).onclick = () => d.close();
      const go = footBtn(d, S.tpl.updateBtn, { primary: true });
      go.onclick = () => {
        result = { removeStale: $("tplu-stale") ? $("tplu-stale").checked : false };
        d.close();
      };
    }
    d.addEventListener("close", () => resolve(result));
    d.showModal();
    (d.querySelector(".dlg-foot .btn.primary") || d.querySelector(".dlg-foot .btn"))?.focus();
  });
}

/// Icon for the menu entries that offer this (kept here so callers don't
/// have to agree on one).
export const updateIcon = () => I("package", 13);
