/* <chat-panel> — the AI sidebar: chat sessions (switcher menu), attachments
 * (upload / paste / drop / from-app), and the composer. The conversation
 * itself (messages, tool cards, approvals, questions) is chat-messages.js. */

import { rpc, state, on, setMode } from "./core.js";
import {
  $, I, esc, fmtSize, installStyle, copyText, isComposing,
  showMenu, dlgAlert, dlgConfirm, dlgInput, makeDialog, footBtn,
  registerPanel, openPanel, closePanel, fileKind, KIND_ICON,
} from "./ui.js";
import { cs } from "./chat-state.js";
import { paintChat, paintDraft, clearChat as clearMessages, hooks } from "./chat-messages.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div class="panel-head">
    <!-- Chat switcher: the active chat's title opens a menu of chats
         (new / rename / archive / delete). -->
    <button id="chat-switch" title="${S.chat.switchChat}"><span id="chat-title">${S.chat.title}</span></button>
    <span id="ai-status"></span>
    <div class="spacer"></div>
    <button id="chat-mode" title="${S.chat.toggleMode}"></button>
    <button id="chat-compact" class="icon-btn"
      title="${S.chat.summarize}"></button>
    <button id="chat-clear" class="icon-btn" title="${S.chat.clearChat}"></button>
    <button id="chat-close" class="icon-btn" title="${S.common.closeEsc}"></button>
  </div>
  <div id="ai-setup" class="hidden">
    <span id="ai-setup-msg"></span>
    <button id="btn-ai-setup"></button>
  </div>
  <div id="agent-banner" class="hidden"></div>
  <div id="chat-messages"></div>
  <div id="chat-staged"></div>
  <div id="chat-input-row">
    <div id="composer-btns">
      <button id="chat-attach" class="icon-btn boxed" title="${S.chat.attachFiles}"></button>
      <button id="chat-fromapp" class="icon-btn boxed" title="${S.chat.attachFromApp}"></button>
    </div>
    <textarea id="chat-input" rows="2"
      placeholder="${S.chat.inputPlaceholder}"></textarea>
    <div id="chat-btns">
      <button id="chat-send" title="${S.chat.send}"></button>
      <button id="chat-stop" title="${S.chat.stop}" class="hidden"></button>
    </div>
  </div>
  <input type="file" id="chat-file-input" multiple class="hidden">
  <div id="chat-dropzone">${S.chat.dropToAttach}</div>
`;

const CSS = /* css */ `
#sidebar { width: 380px; max-width: 55vw; }
#chat-switch {
  display: inline-flex; align-items: center; gap: 5px; max-width: 45%;
  font: inherit; font-weight: 600; color: var(--text); background: none;
  border: none; cursor: pointer; padding: 3px 6px; border-radius: 6px;
}
#chat-switch::after { content: "▾"; font-size: 10px; opacity: .55; }
#chat-switch:hover { background: var(--hover); }
#chat-switch #chat-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* another chat is still working */
#chat-switch.working::before {
  content: ""; width: 6px; height: 6px; border-radius: 50%; background: var(--brand);
  animation: pulse 1.4s ease-in-out infinite;
}
#ai-status { font-size: 12px; color: var(--muted); }
/* approval-mode pill (auto → manual → plan) */
#chat-mode {
  display: inline-flex; align-items: center; gap: 5px; flex: 0 0 auto;
  font: inherit; font-size: 12px; color: var(--text-2); cursor: pointer;
  border: 1px solid var(--line-strong); background: var(--surface);
  border-radius: var(--r-pill); padding: 3px 10px;
}
#chat-mode:hover { background: var(--hover); }
#chat-mode svg { display: block; }
/* ---- provider setup banner (no AI provider configured yet) ---- */
#ai-setup {
  display: flex; align-items: center; gap: 8px; flex: 0 0 auto;
  padding: 8px 10px; font-size: 12px; color: var(--warn-ink);
  background: var(--warn-bg); border-bottom: 1px solid var(--warn-line);
}
#ai-setup span { display: inline-flex; align-items: center; gap: 6px; flex: 1; }
#ai-setup button {
  display: inline-flex; align-items: center; gap: 5px; white-space: nowrap;
  background: var(--warn); color: #3a2c05; border: none; border-radius: 6px;
  padding: 3px 10px; font-size: 12px; font-weight: 600; cursor: pointer;
}
#ai-setup button:hover { filter: brightness(1.06); }
/* ---- agent conversation banner (read-only view) ---- */
#agent-banner {
  display: flex; align-items: center; gap: 8px; flex: 0 0 auto;
  padding: 6px 10px; font-size: 12px; color: var(--warn-ink);
  background: var(--warn-bg); border-bottom: 1px solid var(--warn-line);
}
#agent-banner span { display: inline-flex; align-items: center; gap: 6px; }
#agent-banner button {
  margin-left: auto; font: inherit; cursor: pointer; color: var(--brand);
  background: none; border: none; text-decoration: underline;
}
#chat-input-row.readonly { opacity: .5; pointer-events: none; }
.refchip { background: var(--ok-soft); border-color: var(--ok-line); }
.clearall {
  border: none; background: none; color: var(--muted); font-size: 11px;
  cursor: pointer; text-decoration: underline;
}
#chat-staged { padding: 0 10px; flex: 0 0 auto; }

#composer-btns { display: flex; flex-direction: column; gap: 4px; }
#chat-input-row {
  display: flex; gap: 6px; padding: 8px 10px; border-top: 1px solid var(--line);
  flex: 0 0 auto; align-items: flex-end; background: var(--surface);
}
#chat-input { flex: 1; resize: none; min-width: 0; }
#chat-btns button {
  width: 34px; height: 34px; border-radius: 8px; border: none;
  background: var(--brand); color: #fff; cursor: pointer;
  display: inline-flex; align-items: center; justify-content: center;
}
#chat-btns button:hover { background: var(--brand-strong); }
#chat-stop { background: var(--err) !important; }
/* Touch devices: grow the panel's bespoke tap targets. */
@media (pointer: coarse) {
  #chat-btns button { width: 42px; height: 42px; }
  #chat-switch { padding: 8px 10px; }
  #chat-mode { padding: 7px 12px; }
  #ai-setup button { padding: 7px 12px; }
}
#chat-dropzone {
  position: absolute; inset: 0; display: none; align-items: center; justify-content: center;
  background: rgba(47,109,246,.12); border: 3px dashed var(--brand); color: var(--brand);
  font-size: 16px; font-weight: 600; pointer-events: none; z-index: 5;
}
#sidebar.dropping #chat-dropzone { display: flex; }
/* attach-from-app dialog rows */
.filerow {
  display: flex; align-items: center; gap: 8px; width: 100%; padding: 7px 8px;
  border: none; background: none; cursor: pointer; font: inherit; font-size: 13px;
  border-radius: 6px; text-align: left;
}
.filerow:hover { background: var(--hover); }
.filerow .fname { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.filerow .fsz { color: var(--faint); font-size: 11px; }
`;

// ---------- state ----------
let staged = [];          // {name, b64} uploads
let refs = [];            // archive file names referenced
let sidebarInit = false;  // has the initial open/closed state been applied?

const STAGE_INLINE_MAX = 16 * 1024 * 1024;
const MAX_UPLOAD = 100 * 1024 * 1024;

function setStatus(s) { $("ai-status").textContent = s; }
/// Busy state of THIS chat. The conversation only repaints when it actually
/// flips: app.info lands every few seconds, and a repaint per poll is what
/// used to collapse open tool cards mid-read.
function setBusy(b) {
  b = !!b;
  const changed = b !== cs.aiBusy;
  cs.aiBusy = b;
  $("chat-stop").classList.toggle("hidden", !b);
  $("chat-send").classList.toggle("hidden", b);
  // Sending stays enabled while the assistant works — the run answers late
  // messages after the current turn. Say so instead of looking stuck.
  $("chat-input").placeholder = b
    ? S.chat.inputBusy
    : S.chat.inputPlaceholder;
  if (changed) renderChatSoon();
}
let renderTimer = null;
function renderChatSoon() { clearTimeout(renderTimer); renderTimer = setTimeout(refreshChat, 30); }

// Re-parsing markdown for the whole draft on every stream tick is what makes
// long replies janky: throttle to 150ms, leading edge + trailing flush.
let draftLast = 0, draftTimer = null;
function renderDraftThrottled() {
  const paint = () => {
    draftLast = Date.now();
    if (!paintDraft()) renderChatSoon();
  };
  const wait = draftLast + 150 - Date.now();
  clearTimeout(draftTimer);
  if (wait <= 0) paint();
  else draftTimer = setTimeout(paint, wait);
}

// ---------- the conversation ----------
// Requests can overlap (an event lands while a refresh is in flight); only
// the newest answer is painted, so a slow old one never overwrites it.
let chatSeq = 0;
async function refreshChat() {
  const seq = ++chatSeq, session = cs.session;
  let r;
  try { r = await rpc("chat.list", { session }); } catch { return; }
  if (seq !== chatSeq || session !== cs.session) return;
  renderAgentBanner();
  paintChat(r.rows);
}

// ---------- chat sessions (switcher) ----------
const sessionMeta = (id) => cs.sessions.find((s) => s.id === id) || null;
const sessionTitle = (s) =>
  (s && s.title) || (s && s.id === "main" ? S.chat.mainChat : s ? (s.kind === "agent" ? S.chat.agentTitle : S.chat.chatTitle) : S.chat.chatTitle);

let sessTimer = null;
/// Chat ops arrive in bursts (assistant + tool rows) — coalesce the list refresh.
function refreshSessionsSoon() { clearTimeout(sessTimer); sessTimer = setTimeout(refreshSessions, 300); }
async function refreshSessions() {
  try { cs.sessions = await rpc("sessions.list"); } catch { return; }
  // The active chat can vanish (deleted on another device) — fall back to main.
  if (cs.session !== "main" && !sessionMeta(cs.session)) { showSession("main"); refreshChat(); }
  renderSwitcher();
}
function persistSession() {
  if (state.appId) localStorage.setItem(`uapp.session.${state.appId}`, cs.session);
}
/// Point the panel at another chat. Its rows share nothing with the old one.
function showSession(id) {
  cs.session = id;
  cs.draftText = "";
  persistSession();
  clearMessages();
}
function renderSwitcher() {
  const cur = sessionMeta(cs.session);
  const label = $("chat-title");
  if (label) label.textContent = sessionTitle(cur) || S.chat.title;
  const btn = $("chat-switch");
  if (!btn) return;
  // Chats other than this one still working: a subtle dot on the switcher.
  const others = cs.busySessions.filter((s) => s !== cs.session).length;
  btn.classList.toggle("working", others > 0);
  btn.title = others ? S.chat.otherChatWorking(others) : S.chat.switchChat;
  renderAgentBanner();
}
/// Agent conversations are read-only: they belong to an autonomous agent.
function renderAgentBanner() {
  const b = $("agent-banner");
  if (!b) return;
  const cur = sessionMeta(cs.session);
  const isAgent = !!cur && cur.kind === "agent";
  b.classList.toggle("hidden", !isAgent);
  if (isAgent) {
    b.innerHTML = `<span>${I("bot", 13)} ${S.chat.agentBanner}</span>`;
    const back = document.createElement("button");
    back.textContent = S.chat.backTo(sessionMeta(cur.parent) ? sessionTitle(sessionMeta(cur.parent)) : S.chat.mainChat);
    back.onclick = () => switchSession(cur.parent || "main");
    b.appendChild(back);
  }
  const row = $("chat-input-row");
  if (row) row.classList.toggle("readonly", isAgent);
  const input = $("chat-input");
  if (input) input.disabled = isAgent;
}
async function switchSession(id) {
  if (!id || id === cs.session) { if (id) openPanel("sidebar"); return; }
  showSession(id);
  openPanel("sidebar");
  await refreshSessions();
  // This chat's own busy state (another chat's run must not show here).
  try { const info = await rpc("app.info"); setBusy((info.aiSessions || []).includes(cs.session)); } catch {}
  refreshChat();
}
hooks.switchSession = switchSession;
function chatMenu(x, y) {
  const items = [];
  for (const s of cs.sessions.filter((s) => s.kind !== "agent" && !s.archived)) {
    const working = cs.busySessions.includes(s.id) ? " ●" : "";
    items.push({
      icon: s.id === cs.session ? "check" : "message-square",
      label: `${sessionTitle(s)} (${s.messages || 0})${working}`,
      run: () => switchSession(s.id),
    });
  }
  // Agent conversations stay out of the list, but archived chats can return.
  const archived = cs.sessions.filter((s) => s.kind !== "agent" && s.archived);
  if (archived.length) {
    items.push({ sep: true });
    for (const s of archived) {
      items.push({ icon: "archive", label: `${sessionTitle(s)} (archived)`, run: () => switchSession(s.id) });
    }
  }
  items.push({ sep: true });
  items.push({ icon: "plus", label: S.chat.menuNewChat, run: newChat });
  const cur = sessionMeta(cs.session);
  if (cur && cur.kind !== "agent") {
    items.push({ icon: "pencil", label: S.chat.menuRenameChat, run: renameChat });
    items.push({
      icon: "archive", label: cur.archived ? S.chat.menuUnarchive : S.chat.menuArchive,
      run: async () => { await rpc("sessions.archive", { id: cs.session, archived: !cur.archived }); refreshSessions(); },
    });
    items.push({ icon: "trash-2", label: S.chat.menuDeleteChat, danger: true, run: deleteChat });
  }
  showMenu(items, x, y);
}
async function newChat() {
  const title = await dlgInput(S.chat.newChatPrompt, S.chat.newChatDefault, S.common.create);
  if (title === null) return;
  try {
    const r = await rpc("sessions.create", { title: title.trim() || S.chat.newChatDefault });
    await switchSession(r.id);
    $("chat-input").focus();
  } catch (e) { dlgAlert(S.chat.newChatFailed(e.message)); }
}
async function renameChat() {
  const cur = sessionMeta(cs.session);
  const title = await dlgInput(S.chat.renameChatPrompt, sessionTitle(cur), S.common.rename);
  if (title === null || !title.trim()) return;
  await rpc("sessions.rename", { id: cs.session, title: title.trim() });
  refreshSessions();
}
async function deleteChat() {
  const cur = sessionMeta(cs.session);
  if (!(await dlgConfirm(S.chat.deleteConfirm(sessionTitle(cur))))) return;
  const id = cs.session;
  showSession("main");
  await rpc("sessions.delete", { id });
  await refreshSessions();
  refreshChat();
}
async function compactChat() {
  if (!(await dlgConfirm(S.chat.compactConfirm))) return;
  setStatus(S.chat.statusCompacting);
  try {
    const r = await rpc("chat.compact", { session: cs.session });
    setStatus("");
    if (r.note) dlgAlert(r.note);
    refreshChat();
  } catch (e) {
    setStatus("");
    dlgAlert(S.chat.compactFailed(e.message));
  }
}

// ---------- sending & attachments ----------
function renderStaged() {
  const s = $("chat-staged");
  s.innerHTML = "";
  staged.forEach((a, i) => {
    const chip = document.createElement("span");
    chip.className = "attchip";
    chip.innerHTML = `${I("paperclip", 10)} ${esc(a.name)} <button data-i="${i}">${I("x", 10)}</button>`;
    chip.querySelector("button").onclick = () => { staged.splice(i, 1); renderStaged(); };
    s.appendChild(chip);
  });
  refs.forEach((name, i) => {
    const chip = document.createElement("span");
    chip.className = "attchip refchip";
    chip.innerHTML = `${I("link", 10)} ${esc(name)} <button data-i="${i}">${I("x", 10)}</button>`;
    chip.querySelector("button").onclick = () => { refs.splice(i, 1); renderStaged(); };
    s.appendChild(chip);
  });
  if (staged.length + refs.length > 1) {
    const clr = document.createElement("button");
    clr.className = "clearall";
    clr.textContent = S.chat.clearAll;
    clr.onclick = () => { staged = []; refs = []; renderStaged(); };
    s.appendChild(clr);
  }
}
/// Native drops land as archive references (main.js routes them here).
export function addRefs(names) {
  for (const n of names) if (!refs.includes(n)) refs.push(n);
  renderStaged();
}
async function clearChat() {
  if (await dlgConfirm(S.chat.clearConfirm)) await rpc("chat.clear", { session: cs.session });
}
async function send() {
  const input = $("chat-input");
  const text = input.value.trim();
  if (!text && staged.length === 0 && refs.length === 0) return;
  // Slash commands: a lone /word is a command, never a message.
  const cmd = text.match(/^\/(\w+)$/);
  if (cmd) {
    input.value = "";
    if (cmd[1] === "clear") return clearChat();
    if (cmd[1] === "compact") return compactChat();
    input.value = text; // unknown: keep what they typed
    dlgAlert(S.chat.unknownCommand(cmd[1]));
    return;
  }
  input.value = "";
  const atts = staged, r = refs;
  staged = []; refs = []; renderStaged();
  try {
    await rpc("chat.send", { text, attachments: atts, refs: r, mode: state.mode, session: cs.session });
  } catch (e) {
    // Give the message and its attachments back: a failed send must not eat them.
    input.value = text; staged = atts; refs = r; renderStaged();
    dlgAlert(S.chat.sendFailed(e.message));
  }
}
/// Attach a file to the next message. Small files ride along as base64 with
/// the message (nothing is written until it's sent); big ones are streamed
/// into data/ first and attached by reference, because base64 of a 100MB file
/// in the page is a memory problem, not a transfer problem.
async function stageFile(file) {
  if (file.size > MAX_UPLOAD) { dlgAlert(S.chat.uploadTooLarge(file.name, MAX_UPLOAD / 1048576)); return; }
  const name = file.name || "pasted.png";
  if (file.size > STAGE_INLINE_MAX) {
    try {
      const r = await fetch("/upload?name=" + encodeURIComponent("data/" + name),
        { method: "POST", body: file, headers: { "content-type": "application/octet-stream" } });
      if (!r.ok) throw new Error((await r.text()) || r.statusText);
      const stored = (await r.json()).name || "data/" + name;
      addRefs([stored]);
    } catch (e) { dlgAlert(`${name}: ${e.message}`); }
    return;
  }
  const rd = new FileReader();
  rd.onload = () => {
    staged.push({ name, b64: rd.result.split(",", 2)[1] });
    renderStaged();
  };
  rd.readAsDataURL(file);
}

// ---------- attach from app ----------
async function fromAppDialog() {
  const files = await rpc("files.list");
  const d = makeDialog(S.chat.fromAppTitle, "folder-open");
  const body = d.querySelector(".dlg-body");
  const hint = document.createElement("p");
  hint.className = "hint";
  hint.textContent = S.chat.fromAppHint;
  body.appendChild(hint);
  for (const [name, sz] of files.rows) {
    const b = document.createElement("button");
    b.className = "filerow";
    b.innerHTML = `<span class="ticon">${I(KIND_ICON[fileKind(name)] || "file", 14)}</span>` +
      `<span class="fname">${esc(name)}</span><span class="fsz">${fmtSize(sz)}</span>`;
    b.onclick = () => { addRefs([name]); d.close(); };
    body.appendChild(b);
  }
  footBtn(d, S.common.close).onclick = () => d.close();
  d.showModal();
}

// ---------- events from the server ----------
function onAiEvent(p) {
  // Internal helper calls (the compaction summarizer) stream under a
  // "compact:" sentinel — never a visible chat, never a busy dot.
  if (p.session && p.session.startsWith("compact:")) return;
  // Runs are per session: another chat's progress only lights up a dot on
  // the switcher, it never touches this conversation's draft or status.
  if (p.session && p.session !== cs.session) {
    if (p.state === "running" || p.state === "streaming" || p.state === "thinking" || p.state === "tool") {
      if (!cs.busySessions.includes(p.session)) { cs.busySessions.push(p.session); renderSwitcher(); }
    } else {
      cs.busySessions = cs.busySessions.filter((s) => s !== p.session);
      renderSwitcher();
      refreshSessionsSoon();
    }
    return;
  }
  if (p.state === "running") { setBusy(true); setStatus(p.note || S.chat.statusThinking); }
  else if (p.state === "thinking") {
    setBusy(true);
    setStatus(p.chars ? S.chat.statusThinkingK((p.chars / 1000).toFixed(1)) : S.chat.statusThinking);
  } else if (p.state === "streaming") {
    setBusy(true);
    cs.draftText = p.text || "";
    setStatus(S.chat.statusWriting);
    renderDraftThrottled();
  } else if (p.state === "tool") {
    // The streamed text is a real row now; the next repaint swaps the draft
    // for it.
    if (cs.draftText) { cs.draftText = ""; renderChatSoon(); }
    setBusy(true);
    setStatus(S.chat.statusRunning(p.tool));
  } else { cs.draftText = ""; setBusy(false); setStatus(""); refreshChat(); }
}

function onInfo(info) {
  const savedSession = state.appId && localStorage.getItem(`uapp.session.${state.appId}`);
  if (savedSession && savedSession !== cs.session) { showSession(savedSession); refreshChat(); }
  cs.busySessions = info.aiSessions || [];
  // Busy is per session: only this conversation's run drives the UI.
  setBusy(cs.busySessions.includes(cs.session));
  renderSwitcher();
  // Initial chat sidebar state (once): open for a new/blank app so you can
  // start building, closed for a real app. The default is closed (markup),
  // so we only need to open it. After this the user's toggle is respected.
  if (!sidebarInit) {
    sidebarInit = true;
    if (info.blank) openPanel("sidebar");
  }
}

// ---------- approval mode (auto → manual → plan) ----------
// Plan is read-only: the assistant only gets read tools and proposes changes
// instead of making them. The choice rides along with each chat.send.
const MODES = {
  auto:   { icon: "zap",          label: S.chat.modeAuto,   title: S.chat.modeAutoTitle },
  manual: { icon: "shield-check", label: S.chat.modeManual, title: S.chat.modeManualTitle },
  plan:   { icon: "compass",      label: S.chat.modePlan,   title: S.chat.modePlanTitle },
};
function renderMode() {
  const btn = $("chat-mode");
  const m = MODES[state.mode] || MODES.auto;
  btn.innerHTML = I(m.icon, 13) + `<span>${m.label}</span>`;
  btn.title = m.title;
}

// ---------- component ----------
class ChatPanel extends HTMLElement {
  connectedCallback() {
    installStyle("chat-panel", CSS);
    this.innerHTML = TPL;
    wire(this);
  }
}

function wire(host) {
  registerPanel("sidebar", "btn-chat", () => { try { $("chat-input").focus(); } catch {} });

  $("chat-compact").innerHTML = I("package", 14);
  $("chat-clear").innerHTML = I("trash-2", 14);
  $("chat-close").innerHTML = I("x", 15);
  $("chat-attach").innerHTML = I("paperclip", 14);
  $("chat-fromapp").innerHTML = I("folder-open", 14);
  $("chat-send").innerHTML = I("send-horizontal", 15);
  $("chat-stop").innerHTML = I("square", 13);

  // "Configure a provider" banner: shown while app.info reports no AI
  // provider (absent field = older core → stay hidden). config.set triggers
  // a fresh app.info push, so it hides the moment a provider is saved.
  $("ai-setup-msg").innerHTML =
    I("circle-alert", 13) + " " + S.chat.setupMsg;
  $("btn-ai-setup").innerHTML = I("settings", 12) + " " + S.chat.setupBtn;
  $("btn-ai-setup").onclick = () => openPanel("settingspanel");
  on("info", (info) => $("ai-setup").classList.toggle("hidden", info.aiConfigured !== false));

  $("chat-close").onclick = () => closePanel("sidebar");
  $("chat-send").onclick = send;
  $("chat-stop").onclick = () => rpc("ai.stop", { session: cs.session });
  $("chat-compact").onclick = compactChat;
  $("chat-clear").onclick = clearChat;
  renderMode();
  $("chat-mode").onclick = () => {
    setMode(state.mode === "auto" ? "manual" : state.mode === "manual" ? "plan" : "auto");
    renderMode();
  };
  $("chat-switch").onclick = (e) => {
    const r = e.currentTarget.getBoundingClientRect();
    chatMenu(r.left, r.bottom + 4);
  };
  $("chat-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !isComposing(e)) { e.preventDefault(); send(); }
  });
  $("chat-input").addEventListener("paste", (e) => {
    for (const item of e.clipboardData?.items || []) {
      if (item.kind === "file") { const f = item.getAsFile(); if (f) { stageFile(f); e.preventDefault(); } }
    }
  });
  $("chat-attach").onclick = () => $("chat-file-input").click();
  $("chat-file-input").addEventListener("change", (e) => {
    for (const f of e.target.files) stageFile(f);
    e.target.value = "";
  });
  $("chat-fromapp").onclick = () => fromAppDialog().catch((e) => dlgAlert(e.message));

  // Right-click a chat bubble to copy its markdown source. When text is
  // selected inside it, the browser's own copy menu is more useful — keep it.
  $("chat-messages").addEventListener("contextmenu", (e) => {
    const bubble = e.target.closest(".bubble");
    if (!bubble || bubble.dataset.src == null) return;
    const sel = window.getSelection();
    if (sel && !sel.isCollapsed && bubble.contains(sel.anchorNode)) return;
    e.preventDefault();
    showMenu([{ icon: "copy", label: S.chat.copyMessage, run: () => copyText(bubble.dataset.src) }],
      e.clientX, e.clientY);
  });

  // HTML5 drops (browser windows; native windows come via main.js instead).
  ["dragenter", "dragover"].forEach((ev) =>
    host.addEventListener(ev, (e) => { e.preventDefault(); host.classList.add("dropping"); }));
  ["dragleave", "drop"].forEach((ev) =>
    host.addEventListener(ev, (e) => { e.preventDefault(); if (ev === "drop" || e.target === host) host.classList.remove("dropping"); }));
  host.addEventListener("drop", (e) => {
    host.classList.remove("dropping");
    for (const f of e.dataTransfer.files) stageFile(f);
  });

  on("ws-open", async () => {
    refreshSessions();
    refreshChat();
    try { cs.approvals = await rpc("ai.pending"); renderChatSoon(); } catch {}
    try { cs.questions = await rpc("ai.questions"); renderChatSoon(); } catch {}
  });
  on("chat-changed", () => { cs.draftText = ""; refreshChat(); refreshSessionsSoon(); });
  on("ai", onAiEvent);
  on("approval", (pending) => { cs.approvals = pending; renderChatSoon(); });
  on("question", (pending) => { cs.questions = pending; renderChatSoon(); });
  on("info", onInfo);
  on("info", renderMode); // the saved per-app mode loads with app.info
  on("mode", renderMode);
}

customElements.define("chat-panel", ChatPanel);
