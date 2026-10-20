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
    <button id="btn-tools-close" class="icon-btn" title="${S.common.closeEsc}" aria-label="${S.common.close}"></button>
  </div>
  <div class="panel-body">
    <p class="hint">${S.tools.hint}</p>
    <div id="tools-list"></div>
    <div class="mcp-head">
      <h3>${S.tools.mcpTitle}</h3>
      <button id="btn-mcp-refresh" class="icon-btn" title="${S.tools.refreshStatus}" aria-label="${S.tools.refreshStatus}"></button>
    </div>
    <div id="mcp-list"><p class="hint">${S.common.checking}</p></div>
    <div id="mcp-add">
      <input id="mcp-name" placeholder="${S.tools.mcpNamePlaceholder}" aria-label="${S.tools.mcpNamePlaceholder}" autocomplete="off">
      <input id="mcp-url" placeholder="${S.tools.mcpUrlPlaceholder}" aria-label="URL" autocomplete="off">
      <input id="mcp-token" type="password" placeholder="${S.tools.mcpTokenPlaceholder}" aria-label="${S.tools.mcpTokenPlaceholder}" autocomplete="off">
      <button id="btn-mcp-add" class="btn primary sm">${S.tools.connect}</button>
    </div>
    <div id="mcp-msg" class="hint" aria-live="polite"></div>
  </div>
`;

const CSS = /* css */ `
#toolspanel { width: var(--panel-w-s); background: var(--surface); }
.toolrow { padding: var(--sp-2) 2px; border-bottom: 1px solid var(--line-soft); }
.toolrow-head { display: flex; gap: var(--sp-3); align-items: center; }
.toolrow-label { display: flex; gap: 6px; align-items: center; font-size: var(--fs-md); min-width: 0; }
.toolrow-label b { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.gatemark { display: inline-flex; color: var(--warn-ink); }
.toolrow .tooldesc { margin: 2px 0 0 46px; font-size: var(--fs-xs); color: var(--muted); }

/* toggle switch: a real (visually hidden) checkbox, so it is focusable,
   named, and reads as a switch; the ring sits on the track */
.sw { position: relative; display: inline-block; width: 34px; height: 20px; flex: 0 0 auto; }
.sw span {
  position: absolute; inset: 0; border-radius: var(--r-pill); background: var(--line-strong);
  transition: background var(--dur-1) var(--ease); cursor: pointer;
}
.sw span::before {
  content: ""; position: absolute; width: 16px; height: 16px; border-radius: 50%;
  background: var(--knob); top: 2px; left: 2px; transition: transform var(--dur-1) var(--ease);
  box-shadow: var(--shadow-1);
}
.sw input:checked + span { background: var(--ok); }
.sw input:checked + span::before { transform: translateX(14px); }
.sw input:focus-visible + span { outline: var(--ring); outline-offset: 2px; }

/* MCP section */
.mcp-head { display: flex; align-items: center; gap: var(--sp-2); margin-top: var(--sp-5); }
.mcp-head h3 { margin: 0; font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .06em; color: var(--faint); font-weight: 600; }
.mcp-card { border: 1px solid var(--line); border-radius: var(--r-m); padding: var(--sp-2) 10px; margin: 6px 0; }
.mcp-card-head { display: flex; align-items: center; gap: 10px; }
.mcp-info { flex: 1; min-width: 0; font-size: var(--fs-md); }
.mcp-url { font-size: var(--fs-xs); color: var(--faint); font-family: var(--mono);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mcp-del:hover { color: var(--err-ink); background: var(--err-wash); }
.mcp-tools { margin-top: 5px; font-size: var(--fs-xs); color: var(--muted); word-break: break-word; }
.chip { font-size: var(--fs-xs); border-radius: var(--r-pill); padding: 1px var(--sp-2); margin-left: 6px; }
.chip.ok { background: var(--ok-soft); color: var(--ok-ink); }
.chip.err { background: var(--err-soft); color: var(--err-ink); }
.chip.off { background: var(--hover); color: var(--muted); }
#mcp-add { display: flex; gap: 6px; margin-top: var(--sp-2); flex-wrap: wrap; align-items: center; }
#mcp-add input { flex: 1; min-width: 130px; }
#mcp-msg { min-height: 14px; margin-top: var(--sp-1); }
`;

let disabledSet = new Set();
async function saveDisabled() {
  await rpc("config.set", { key: "tools_disabled", value: [...disabledSet] });
}
/// name: what the switch turns on and off, for assistive tech.
function switchEl(onState, onToggle, name) {
  const label = document.createElement("label");
  label.className = "sw";
  label.innerHTML = `<input type="checkbox" role="switch" class="vh"${onState ? " checked" : ""}><span aria-hidden="true"></span>`;
  const input = label.querySelector("input");
  input.setAttribute("aria-label", name);
  input.onchange = (e) => onToggle(e.target.checked);
  return label;
}
function toolRow(t) {
  const row = div("toolrow");
  const meta = toolMeta(t.name);
  const head = div("toolrow-head");
  head.appendChild(switchEl(t.enabled, async (on) => {
    on ? disabledSet.delete(t.name) : disabledSet.add(t.name);
    await saveDisabled();
  }, t.name));
  const label = div("toolrow-label");
  label.innerHTML = `<span class="ticon">${I(meta.icon, 14)}</span><b>${esc(t.name)}</b>` +
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
    }, s.name));
    const info = div("mcp-info");
    const status = !s.enabled ? `<span class="chip off">off</span>`
      : s.error ? `<span class="chip err">${esc(s.error)}</span>`
      : `<span class="chip ok">${esc(S.tools.toolCount(s.toolCount))}</span>`;
    info.innerHTML = `<b>${esc(s.name)}</b> ${status}<div class="mcp-url">${esc(s.url || "")}${s.hasAuth ? " · auth" : ""}</div>`;
    head.appendChild(info);
    const del = document.createElement("button");
    del.className = "icon-btn mcp-del";
    del.title = S.tools.disconnectTitle;
    del.setAttribute("aria-label", S.tools.disconnectTitle);
    del.innerHTML = I("trash-2", 16);
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
  }, S.tools.title);
  $("tl-head-icon").innerHTML = I("wrench", 16);
  $("btn-tools-close").innerHTML = I("x", 16);
  $("btn-tools-close").onclick = () => closePanel("toolspanel");
  $("btn-mcp-refresh").innerHTML = I("refresh-cw", 16);
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
