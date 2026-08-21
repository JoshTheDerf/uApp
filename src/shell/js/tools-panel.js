/* <tools-panel> — per-tool enable switches (synced for everyone) and MCP
 * server cards with connect / disconnect / toggle. An overlay side panel;
 * every change applies instantly, so it has no footer. */

import { rpc } from "./core.js";
import {
  $, I, esc, clip, div, installStyle, toolMeta,
  registerPanel, closePanel, dlgConfirm,
} from "./ui.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div class="panel-head">
    <span class="panel-title"><span id="tl-head-icon"></span>${S.tools.title}</span>
    <div class="spacer"></div>
    <button id="btn-tools-close" class="icon-btn" title="${S.common.closeEsc}"></button>
  </div>
  <div class="panel-body">
    <p class="hint">${S.tools.hint}</p>
    <div id="tools-list"></div>
    <div class="mcp-head">
      <h3>${S.tools.mcpTitle}</h3>
      <button id="btn-mcp-refresh" class="icon-btn" title="${S.tools.refreshStatus}"></button>
    </div>
    <div id="mcp-list"><p class="hint">${S.common.checking}</p></div>
    <div id="mcp-add">
      <input id="mcp-name" placeholder="${S.tools.mcpNamePlaceholder}" autocomplete="off">
      <input id="mcp-url" placeholder="${S.tools.mcpUrlPlaceholder}" autocomplete="off">
      <input id="mcp-token" type="password" placeholder="${S.tools.mcpTokenPlaceholder}" autocomplete="off">
      <button id="btn-mcp-add" class="btn primary sm">${S.tools.connect}</button>
    </div>
    <div id="mcp-msg" class="hint"></div>
  </div>
`;

const CSS = /* css */ `
#toolspanel { width: min(460px, 94vw); background: var(--surface); }
.toolrow { padding: 6px 2px; border-bottom: 1px solid var(--code-bg); }
.toolrow-head { display: flex; gap: 10px; align-items: center; }
.toolrow-label { display: flex; gap: 6px; align-items: center; font-size: 13px; min-width: 0; }
.gatemark { display: inline-flex; color: var(--warn-ink); }
.toolrow .tooldesc { margin-left: 48px; font-size: 11px; color: var(--faint); }

/* toggle switch */
.sw { position: relative; display: inline-block; width: 30px; height: 18px; flex: 0 0 auto; }
.sw input { opacity: 0; width: 0; height: 0; }
.sw span {
  position: absolute; inset: 0; border-radius: 999px; background: var(--line-strong);
  transition: background .15s; cursor: pointer;
}
.sw span::before {
  content: ""; position: absolute; width: 14px; height: 14px; border-radius: 50%;
  background: #fff; top: 2px; left: 2px; transition: transform .15s;
  box-shadow: 0 1px 2px rgba(0,0,0,.25);
}
.sw input:checked + span { background: var(--ok); }
.sw input:checked + span::before { transform: translateX(12px); }

/* MCP section */
.mcp-head { display: flex; align-items: center; gap: 8px; margin-top: 16px; }
.mcp-head h3 { margin: 0; font-size: 13px; text-transform: uppercase; letter-spacing: .06em; color: var(--faint); }
.mcp-card { border: 1px solid var(--line); border-radius: 10px; padding: 8px 10px; margin: 6px 0; }
.mcp-card-head { display: flex; align-items: center; gap: 10px; }
.mcp-info { flex: 1; min-width: 0; font-size: 13px; }
.mcp-url { font-size: 11px; color: var(--faint); font-family: var(--mono);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mcp-del { border: none; background: none; cursor: pointer; color: var(--ghost); display: inline-flex; padding: 4px; border-radius: 5px; }
@media (pointer: coarse) { .mcp-del { padding: 9px; } }
.mcp-del:hover { color: var(--err); background: var(--err-wash); }
.mcp-tools { margin-top: 5px; font-size: 11px; color: var(--muted); word-break: break-word; }
.chip { font-size: 10px; border-radius: 999px; padding: 1px 8px; margin-left: 6px; }
.chip.ok { background: var(--ok-soft); color: var(--ok-ink); }
.chip.err { background: var(--err-soft); color: var(--err-ink); }
.chip.off { background: var(--hover); color: var(--muted); }
#mcp-add { display: flex; gap: 6px; margin-top: 8px; flex-wrap: wrap; align-items: center; }
#mcp-add input { flex: 1; min-width: 130px; }
#mcp-msg { min-height: 14px; margin-top: 4px; }
`;

let disabledSet = new Set();
async function saveDisabled() {
  await rpc("config.set", { key: "tools_disabled", value: [...disabledSet] });
}
function switchEl(onState, onToggle) {
  const label = document.createElement("label");
  label.className = "sw";
  label.innerHTML = `<input type="checkbox" ${onState ? "checked" : ""}><span></span>`;
  label.querySelector("input").onchange = (e) => onToggle(e.target.checked);
  return label;
}
function toolRow(t) {
  const row = div("toolrow");
  const meta = toolMeta(t.name);
  const head = div("toolrow-head");
  head.appendChild(switchEl(t.enabled, async (on) => {
    on ? disabledSet.delete(t.name) : disabledSet.add(t.name);
    await saveDisabled();
  }));
  const label = div("toolrow-label");
  label.innerHTML = `<span class="ticon">${I(meta.icon, 13)}</span><b>${esc(t.name)}</b>` +
    (t.gated ? `<span class="gatemark" title="${esc(S.tools.gateMarkTitle)}">${I("shield-check", 11)}</span>` : "");
  head.appendChild(label);
  row.appendChild(head);
  if (t.description) {
    const d = div("tooldesc");
    d.textContent = clip(String(t.description), 120);
    row.appendChild(d);
  }
  return row;
}
function section(title) {
  const s = div("tsection");
  s.textContent = title;
  return s;
}
async function renderTools() {
  let tools = [];
  try { tools = await rpc("tools.list"); } catch { return; }
  disabledSet = new Set(tools.filter((t) => !t.enabled).map((t) => t.name));
  const list = $("tools-list");
  list.innerHTML = "";
  const builtins = tools.filter((t) => t.kind === "builtin");
  const apps = tools.filter((t) => t.kind === "app");
  list.appendChild(section(S.tools.sectionBuiltin));
  builtins.forEach((t) => list.appendChild(toolRow(t)));
  if (apps.length) {
    list.appendChild(section(S.tools.sectionApp));
    apps.forEach((t) => list.appendChild(toolRow(t)));
  }
}
async function renderMcp() {
  const list = $("mcp-list");
  list.innerHTML = `<p class="hint">${esc(S.common.checking)}</p>`;
  let servers = [];
  try { servers = await rpc("mcp.status"); } catch (e) {
    list.innerHTML = `<p class="hint">${esc(e.message)}</p>`; return;
  }
  list.innerHTML = "";
  if (!servers.length) {
    list.innerHTML = `<p class="hint">${esc(S.tools.noServers)}</p>`;
    return;
  }
  for (const s of servers) {
    const card = div("mcp-card");
    const head = div("mcp-card-head");
    head.appendChild(switchEl(s.enabled, async (on) => {
      await rpc("mcp.toggle", { name: s.name, enabled: on });
      renderMcp();
    }));
    const info = div("mcp-info");
    const status = !s.enabled ? `<span class="chip off">off</span>`
      : s.error ? `<span class="chip err">${esc(s.error)}</span>`
      : `<span class="chip ok">${esc(S.tools.toolCount(s.toolCount))}</span>`;
    info.innerHTML = `<b>${esc(s.name)}</b> ${status}<div class="mcp-url">${esc(s.url || "")}${s.hasAuth ? " · auth" : ""}</div>`;
    head.appendChild(info);
    const del = document.createElement("button");
    del.className = "mcp-del";
    del.title = S.tools.disconnectTitle;
    del.innerHTML = I("trash-2", 13);
    del.onclick = async () => {
      if (!(await dlgConfirm(S.tools.disconnectConfirm(s.name)))) return;
      await rpc("mcp.remove", { name: s.name });
      renderMcp();
    };
    head.appendChild(del);
    card.appendChild(head);
    if (s.enabled && s.tools && s.tools.length) {
      const tl = div("mcp-tools");
      tl.textContent = s.tools.join(", ");
      card.appendChild(tl);
    }
    list.appendChild(card);
  }
}

// ---------- component ----------
class ToolsPanel extends HTMLElement {
  connectedCallback() {
    installStyle("tools-panel", CSS);
    this.innerHTML = TPL;
    wire();
  }
}

function wire() {
  registerPanel("toolspanel", "btn-tools", () => {
    renderTools();
    renderMcp();
    $("mcp-msg").textContent = "";
  });
  $("tl-head-icon").innerHTML = I("wrench", 14);
  $("btn-tools-close").innerHTML = I("x", 15);
  $("btn-tools-close").onclick = () => closePanel("toolspanel");
  $("btn-mcp-refresh").innerHTML = I("refresh-cw", 13);
  $("btn-mcp-refresh").onclick = () => renderMcp();
  $("btn-mcp-add").onclick = async () => {
    const name = ($("mcp-name").value || "").trim(), url = ($("mcp-url").value || "").trim();
    if (!name || !url) { $("mcp-msg").textContent = S.tools.nameUrlRequired; return; }
    const btn = $("btn-mcp-add");
    btn.disabled = true; btn.textContent = S.tools.connecting; $("mcp-msg").textContent = "";
    try {
      const r = await rpc("mcp.add", { name, url, bearer_token: ($("mcp-token").value || "").trim() || undefined });
      $("mcp-msg").textContent = S.tools.connected(r.name, r.toolCount);
      $("mcp-name").value = ""; $("mcp-url").value = ""; $("mcp-token").value = "";
      renderMcp();
    } catch (err) {
      $("mcp-msg").textContent = err.message;
    } finally { btn.disabled = false; btn.textContent = S.tools.connect; }
  };
}

customElements.define("tools-panel", ToolsPanel);
