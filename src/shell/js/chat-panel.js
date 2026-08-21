/* <chat-panel> — the AI sidebar: chat sessions (switcher menu), messages with
 * tool cards, the approval flow, ask_user question cards, attachments
 * (upload / paste / drop / from-app), and the composer. */

import { rpc, state, on, setMode } from "./core.js";
import {
  $, I, esc, clip, pretty, div, fmtSize, appUrl, installStyle, copyText,
  showMenu, dlgAlert, dlgConfirm, dlgInput, makeDialog, footBtn, toolMeta,
  registerPanel, openPanel, closePanel, togglePanel, fileKind, KIND_ICON,
} from "./ui.js";
import { mdRender } from "./markdown.js";
import { openViewer } from "./viewer.js";
import { S } from "./strings.js";

const TPL = /* html */ `
  <div class="panel-head">
    <!-- Chat switcher: the active chat's title opens a menu of chats
         (new / rename / archive / delete). -->
    <button id="chat-switch" title="${S.chat.switchChat}"><span id="chat-title">${S.chat.title}</span></button>
    <span id="ai-status"></span>
    <div class="spacer"></div>
    <button id="btn-mode" title="${S.chat.toggleMode}"></button>
    <button id="btn-chat-compact" class="icon-btn"
      title="${S.chat.summarize}"></button>
    <button id="btn-clear" class="icon-btn" title="${S.chat.clearChat}"></button>
    <button id="btn-close" class="icon-btn" title="${S.common.closeEsc}"></button>
  </div>
  <div id="ai-setup" class="hidden">
    <span id="ai-setup-msg"></span>
    <button id="btn-ai-setup"></button>
  </div>
  <div id="agent-banner" class="hidden"></div>
  <div id="messages"></div>
  <div id="staged"></div>
  <div id="chat-input-row">
    <div id="composer-btns">
      <button id="btn-attach" class="icon-btn boxed" title="${S.chat.attachFiles}"></button>
      <button id="btn-fromapp" class="icon-btn boxed" title="${S.chat.attachFromApp}"></button>
    </div>
    <textarea id="chat-input" rows="2"
      placeholder="${S.chat.inputPlaceholder}"></textarea>
    <div id="chat-btns">
      <button id="btn-send" title="${S.chat.send}"></button>
      <button id="btn-stop" title="${S.chat.stop}" class="hidden"></button>
    </div>
  </div>
  <input type="file" id="file-input" multiple class="hidden">
  <div id="dropzone">${S.chat.dropToAttach}</div>
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
#btn-mode {
  display: inline-flex; align-items: center; gap: 5px; flex: 0 0 auto;
  font: inherit; font-size: 12px; color: var(--text-2); cursor: pointer;
  border: 1px solid var(--line-strong); background: var(--surface);
  border-radius: var(--r-pill); padding: 3px 10px;
}
#btn-mode:hover { background: var(--hover); }
#btn-mode svg { display: block; }
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
/* ---- compaction summary card ---- */
.msg.summary details { border: 1px dashed var(--line-strong); border-radius: 10px; background: var(--surface-3); }
.msg.summary summary {
  display: flex; align-items: center; gap: 6px; padding: 6px 9px;
  font-size: 12px; color: var(--text-2); cursor: pointer; list-style: none;
}
.msg.summary summary::-webkit-details-marker { display: none; }
.msg.summary .bubble { background: transparent; font-size: 12px; }
/* ---- sub-agent report inside a tool card ---- */
.agentreport {
  max-height: 260px; overflow: auto; padding: 6px 8px; margin: 4px 0;
  background: var(--surface); border: 1px solid var(--line); border-radius: 8px; font-size: 12px;
}

#messages { flex: 1; overflow-y: auto; padding: 10px; }
.msg { margin-bottom: 10px; }
.msg .who { font-size: 11px; color: var(--faint); margin-bottom: 2px; }
.msg .bubble {
  padding: 7px 10px; border-radius: 10px; white-space: pre-wrap;
  word-break: break-word; overflow-wrap: anywhere;
}
.msg.user .bubble { background: var(--brand-soft); }
.msg.assistant .bubble { background: var(--surface); border: 1px solid var(--line); }
.msg.system .bubble { background: var(--err-soft); color: var(--err-ink); font-size: 12px; }
.msg .bubble code, .msg .bubble pre {
  font-family: var(--mono); font-size: 12px;
  background: var(--code-bg); border-radius: 4px;
}
.msg .bubble pre { padding: 6px; overflow-x: auto; }
.msg .bubble > :first-child { margin-top: 0; }
.msg .bubble > :last-child { margin-bottom: 0; }
.msg .bubble p { margin: 0 0 8px; }
.msg .bubble h1, .msg .bubble h2, .msg .bubble h3,
.msg .bubble h4, .msg .bubble h5, .msg .bubble h6 { margin: 10px 0 4px; font-size: 14px; }
.msg .bubble ul, .msg .bubble ol { margin: 4px 0 8px; padding-left: 20px; }
.msg .bubble blockquote {
  margin: 6px 0; padding: 2px 10px; border-left: 3px solid var(--line-strong); color: var(--text-2);
}
.msg .bubble img { max-width: 100%; border-radius: 6px; }
.msg .bubble a { color: var(--brand); }
/* ---- tool call blocks ---- */
.tool {
  margin: 4px 0 8px; border: 1px solid var(--line); border-radius: 10px;
  background: var(--surface); font-size: 12px; overflow: hidden;
}
.tool summary {
  display: flex; align-items: center; gap: 6px; padding: 6px 9px;
  cursor: pointer; list-style: none; user-select: none;
}
.tool summary::-webkit-details-marker { display: none; }
.tool .tdetail {
  color: var(--muted); font-family: var(--mono); font-size: 11px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; min-width: 0;
}
.tool .tstate {
  flex: 0 0 auto; font-size: 10px; padding: 1px 8px; border-radius: 999px;
  background: var(--hover); color: var(--muted); text-transform: uppercase; letter-spacing: .03em;
}
.tool.st-running .tstate { background: var(--brand-soft); color: var(--brand-ink); animation: pulse 1.4s infinite; }
.tool.st-done .tstate { background: var(--ok-soft); color: var(--ok-ink); }
.tool.st-error .tstate, .tool.st-interrupted .tstate { background: var(--err-soft); color: var(--err-ink); }
.tool.st-denied .tstate { background: var(--warn-soft); color: var(--warn-ink); }
.tool.st-approval { border-color: var(--warn); box-shadow: 0 0 0 2px var(--warn)66; }
.tool.st-approval .tstate { background: var(--warn-soft); color: var(--warn-ink); }
@keyframes pulse { 50% { opacity: .55; } }
.tool .tbody { border-top: 1px solid var(--line-soft); padding: 6px 9px; }
.tool pre {
  margin: 4px 0; max-height: 16rem; overflow: auto; background: var(--surface-2);
  padding: 6px; border-radius: 6px; font-size: 11px;
  font-family: var(--mono); white-space: pre-wrap; word-break: break-word;
}
.tool pre.terr { background: var(--err-wash); color: var(--err-ink); }
.approve-bar {
  display: flex; gap: 6px; align-items: center; padding: 7px 9px;
  border-top: 1px solid var(--warn-line); background: var(--warn-bg); flex-wrap: wrap;
}
/* ask_user prompt card: question groups with radio/checkbox options + Other */
.tool.question { padding: 0; }
.tool.question .qhead {
  display: flex; align-items: center; gap: 6px; padding: 7px 9px;
  font-size: 12.5px; color: var(--warn-ink); border-bottom: 1px solid var(--warn-line);
}
.tool.question .qitem { padding: 8px 10px; border-bottom: 1px solid var(--warn-line); }
.tool.question .qchip {
  display: inline-block; font-size: 10.5px; font-weight: 600; color: var(--brand);
  background: var(--brand-soft); border-radius: 999px; padding: 1px 8px; margin-bottom: 3px;
}
.tool.question .qtext { font-size: 13px; margin-bottom: 6px; }
.tool.question .qopt {
  display: flex; gap: 7px; align-items: flex-start; font-size: 12.5px;
  padding: 4px 2px; border-radius: 6px; cursor: pointer;
}
.tool.question .qopt:hover { background: var(--brand-wash); }
.tool.question .qopt input { margin-top: 2px; accent-color: var(--brand); }
.tool.question .qlabel { display: block; }
.tool.question .qdesc { display: block; font-size: 11.5px; color: var(--muted); }
.tool.question .qfree {
  width: calc(100% - 26px); margin: 2px 0 2px 24px; padding: 4px 8px;
  font: inherit; font-size: 12.5px;
}
.msg.system .retrybtn {
  display: inline-flex; align-items: center; gap: 4px; margin-left: 8px;
  font-size: 11.5px; padding: 2px 9px; border: 1px solid var(--line-strong);
  border-radius: 999px; background: var(--surface); cursor: pointer; color: var(--brand);
}
.msg.system .retrybtn:hover { border-color: var(--brand); }
/* write_file approval: compact red/green preview of the actual change */
.write-diff {
  flex-basis: 100%; font-family: var(--mono); font-size: 11px;
  background: var(--surface); border: 1px solid var(--line); border-radius: 6px;
  padding: 5px 7px; margin-bottom: 4px; overflow-x: auto; white-space: pre;
}
.write-diff .wd-head, .write-diff .wd-more { color: var(--muted); font-family: inherit; }
.write-diff .wd-del { color: var(--err-ink); background: var(--err-soft)66; }
.write-diff .wd-add { color: var(--ok-ink); background: var(--ok-soft)66; }
.write-diff .wd-new { color: var(--ok-ink); }
.approve-note {
  flex-basis: 100%; font-size: 12px; color: var(--warn-ink);
  background: var(--warn-soft); border-radius: 6px; padding: 4px 8px; margin-bottom: 4px;
}
.approve-q { font-size: 12px; color: var(--warn-ink); margin-right: 2px;
  display: inline-flex; align-items: center; gap: 4px; }
.approve-bar button {
  border: none; border-radius: 7px; padding: 4px 12px; font-size: 12px; cursor: pointer;
}
.approve-bar .ok { background: var(--ok); color: #fff; }
.approve-bar .always { background: var(--surface); border: 1px solid var(--line-strong); color: #444; }
.approve-bar .no { background: var(--err); color: #fff; }
.approve-bar .btn.primary { background: var(--brand); color: #fff; }

.thinkline { font-size: 12px; color: var(--faint); margin: 2px 0 6px 4px; }
.thinkline summary { cursor: pointer; list-style: none;
  display: inline-flex; align-items: center; gap: 4px; }
.thinkline pre {
  max-height: 180px; overflow: auto; background: var(--surface-2);
  padding: 6px; border-radius: 6px; font-size: 11px; white-space: pre-wrap;
}
.thinkingdots .bubble span {
  display: inline-block; width: 6px; height: 6px; margin-right: 4px;
  border-radius: 50%; background: var(--faint); animation: bounce 1.2s infinite;
}
.thinkingdots .bubble span:nth-child(2) { animation-delay: .15s; }
.thinkingdots .bubble span:nth-child(3) { animation-delay: .3s; }
@keyframes bounce { 30% { transform: translateY(-4px); } }
.refchip { background: var(--ok-soft); border-color: var(--ok-line); }
.clearall {
  border: none; background: none; color: var(--muted); font-size: 11px;
  cursor: pointer; text-decoration: underline;
}
#staged { padding: 0 10px; flex: 0 0 auto; }

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
#btn-stop { background: var(--err) !important; }
/* Touch devices: grow the panel's bespoke tap targets. */
@media (pointer: coarse) {
  #chat-btns button { width: 42px; height: 42px; }
  #chat-switch { padding: 8px 10px; }
  #btn-mode { padding: 7px 12px; }
  #ai-setup button { padding: 7px 12px; }
  .tool .topen { padding: 7px 12px; }
}
/* presented-file shortcut on a tool card */
.tool .topen {
  display: inline-flex; align-items: center; gap: 5px; margin-top: 4px;
  border: 1px solid var(--line-strong); background: var(--surface); border-radius: 7px;
  padding: 3px 10px; font: inherit; font-size: 12px; cursor: pointer;
}
.tool .topen:hover { background: var(--brand-wash); border-color: var(--brand-line); }
#dropzone {
  position: absolute; inset: 0; display: none; align-items: center; justify-content: center;
  background: rgba(47,109,246,.12); border: 3px dashed var(--brand); color: var(--brand);
  font-size: 16px; font-weight: 600; pointer-events: none; z-index: 5;
}
#sidebar.dropping #dropzone { display: flex; }
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
let session = "main";     // chat session shown in the sidebar
let sessions = [];        // sessions.list snapshot (chats + agent conversations)
let busySessions = [];    // sessions with a live run (app.info.aiSessions)
let approvals = [];       // pending tool approvals on this device
let questions = [];       // open ask_user prompts on this device
let aiBusy = false;
let draftText = "";
let staged = [];          // {name, b64} uploads
let refs = [];            // archive file names referenced
let sidebarInit = false;  // has the initial open/closed state been applied?

const STAGE_INLINE_MAX = 16 * 1024 * 1024;
const MAX_UPLOAD = 100 * 1024 * 1024;
const stateLabel = { running: S.chat.stateRunning, done: S.chat.stateDone, error: S.chat.stateError, denied: S.chat.stateDenied, approval: S.chat.stateApproval, interrupted: S.chat.stateInterrupted };

function setStatus(s) { $("ai-status").textContent = s; }
function setBusy(b) {
  aiBusy = b;
  $("btn-stop").classList.toggle("hidden", !b);
  $("btn-send").classList.toggle("hidden", b);
  // Sending stays enabled while the assistant works — the run answers late
  // messages after the current turn. Say so instead of looking stuck.
  $("chat-input").placeholder = b
    ? S.chat.inputBusy
    : S.chat.inputPlaceholder;
  renderChatSoon();
}
let renderTimer = null;
function renderChatSoon() { clearTimeout(renderTimer); renderTimer = setTimeout(refreshChat, 30); }

// Re-parsing markdown for the whole draft on every stream tick is what makes
// long replies janky: throttle to 150ms, leading edge + trailing flush.
let draftLast = 0, draftTimer = null;
function renderDraftThrottled() {
  const paint = () => {
    draftLast = Date.now();
    const el = document.getElementById("draftbubble");
    if (!el) return renderChatSoon();
    el.innerHTML = mdRender(draftText);
    const box = $("messages");
    if (box.scrollHeight - box.scrollTop - box.clientHeight < 160) box.scrollTop = box.scrollHeight;
  };
  const wait = draftLast + 150 - Date.now();
  clearTimeout(draftTimer);
  if (wait <= 0) paint();
  else draftTimer = setTimeout(paint, wait);
}

// ---------- chat rendering ----------
async function refreshChat() {
  let r;
  try { r = await rpc("chat.list", { session }); } catch { return; }
  renderAgentBanner();
  const box = $("messages");
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 90;
  box.innerHTML = "";

  // Map tool_use id -> result
  const resultById = new Map();
  for (const row of r.rows) {
    if (row[4] !== "tool") continue;
    let c = {}; try { c = JSON.parse(row[5]); } catch {}
    for (const res of c.results || []) resultById.set(res.id, res);
  }
  const approvalById = new Map(approvals.map((a) => [a.id, a]));
  const renderedToolIds = new Set();

  for (const row of r.rows) {
    const [mid, ts, device, user, role, contentStr] = row;
    let content = {}; try { content = JSON.parse(contentStr); } catch {}
    if (role === "user") {
      const d = div("msg user");
      d.innerHTML = `<div class="who">${esc(user || device)}</div><div class="bubble">${mdRender(content.text || "")}</div>`;
      d.querySelector(".bubble").dataset.src = content.text || "";
      for (const a of content.attachments || []) {
        const chip = document.createElement("span");
        chip.className = "attchip";
        chip.innerHTML = I(a.ref ? "link" : "paperclip", 10) + " " + esc(a.name);
        d.appendChild(chip);
      }
      box.appendChild(d);
    } else if (role === "assistant") {
      for (const b of content.blocks || []) {
        if (b.type === "text" && b.text && b.text.trim()) {
          const d = div("msg assistant");
          d.innerHTML = `<div class="who">${S.chat.whoAssistant}</div><div class="bubble">${mdRender(b.text)}</div>`;
          d.querySelector(".bubble").dataset.src = b.text;
          box.appendChild(d);
        } else if (b.type === "thinking" && b.thinking) {
          const d = div("thinkline");
          d.innerHTML = `<details><summary>${I("brain", 12)} ${S.chat.thinking}</summary><pre>${esc(clip(b.thinking, 4000))}</pre></details>`;
          box.appendChild(d);
        } else if (b.type === "tool_use") {
          renderedToolIds.add(b.id);
          box.appendChild(renderToolBlock(b, resultById.get(b.id), approvalById.get(b.id)));
        }
      }
    } else if (role === "system") {
      const d = div("msg system");
      d.innerHTML = `<div class="bubble">${esc(content.text || "")}</div>`;
      // The newest AI error gets a Retry: re-runs the loop on this chat
      // (e.g. after fixing the API key or a transient provider failure).
      if ((content.text || "").startsWith("AI error") && row === r.rows[r.rows.length - 1] && !aiBusy) {
        const b = document.createElement("button");
        b.className = "retrybtn";
        b.innerHTML = I("rotate-cw", 11) + " " + S.common.retry;
        b.onclick = () => rpc("ai.run", { session });
        d.querySelector(".bubble").appendChild(b);
      }
      box.appendChild(d);
    } else if (role === "summary") {
      // Everything before this row was replaced by one summary.
      const d = div("msg summary");
      d.innerHTML = `<details><summary>${I("package", 12)} ${S.chat.compactedSummary}` +
        `</summary><div class="bubble">${mdRender(content.text || "")}</div></details>`;
      box.appendChild(d);
    }
  }
  // Approvals with no visible tool block — a sub-agent's gated call lives in
  // its hidden agent session, but the prompt belongs to THIS (root) chat.
  // Render them standalone so they can actually be answered.
  for (const a of approvals) {
    if ((a.session || "main") !== session || renderedToolIds.has(a.id)) continue;
    box.appendChild(renderToolBlock({ type: "tool_use", id: a.id, name: a.name, input: a.input }, undefined, a));
  }
  // Open ask_user prompts for this chat: radio/checkbox groups per question,
  // always with a free-form "Other" answer.
  for (const q of questions) {
    if ((q.session || "main") !== session) continue;
    box.appendChild(renderQuestionCard(q));
  }
  if (aiBusy) {
    if (draftText) {
      const d = div("msg assistant");
      d.innerHTML = `<div class="who">${S.chat.whoAssistant}</div><div class="bubble" id="draftbubble">${mdRender(draftText)}</div>`;
      box.appendChild(d);
    } else {
      const d = div("msg assistant thinkingdots");
      d.innerHTML = `<div class="bubble"><span></span><span></span><span></span></div>`;
      box.appendChild(d);
    }
  }
  if (nearBottom) box.scrollTop = box.scrollHeight;
}

/// One ask_user prompt: 1-4 questions, each a radio group (or checkboxes
/// when multiSelect) plus an "Other…" free-text field. Submit sends every
/// question's answer in order; blanks are allowed (they read as unanswered).
function renderQuestionCard(q) {
  const d = div("tool st-approval question");
  const qs = q.questions || [];
  let inner = `<div class="qhead">${I("circle-help", 14)} <b>${S.chat.questionHeading}</b></div>`;
  qs.forEach((item, qi) => {
    inner += `<div class="qitem" data-qi="${qi}">`;
    if (item.header) inner += `<span class="qchip">${esc(item.header)}</span>`;
    inner += `<div class="qtext">${esc(item.question || "")}</div>`;
    const type = item.multiSelect ? "checkbox" : "radio";
    for (const [oi, o] of (item.options || []).entries()) {
      inner += `<label class="qopt"><input type="${type}" name="q${qi}" value="${oi}">` +
        `<span><span class="qlabel">${esc(o.label || "")}</span>` +
        (o.description ? `<span class="qdesc">${esc(o.description)}</span>` : "") +
        `</span></label>`;
    }
    inner += `<label class="qopt qother"><input type="${type === "radio" ? "radio" : "checkbox"}" name="q${qi}" value="other">` +
      `<span class="qlabel">${S.chat.questionOther}</span></label>` +
      `<input type="text" class="qfree" placeholder="${S.chat.questionFreeText}">`;
    inner += `</div>`;
  });
  d.innerHTML = inner;
  // Typing in "Other" selects its toggle; picking a real option clears it.
  d.querySelectorAll(".qitem").forEach((item) => {
    const free = item.querySelector(".qfree");
    const otherToggle = item.querySelector(".qother input");
    free.addEventListener("input", () => { if (free.value.trim()) otherToggle.checked = true; });
    item.querySelectorAll("input[type=radio]").forEach((rb) => {
      rb.addEventListener("change", () => { if (rb.value !== "other" && rb.checked) free.value = ""; });
    });
  });
  const bar = div("approve-bar");
  const send = document.createElement("button");
  send.className = "btn primary sm";
  send.textContent = S.chat.questionSubmit;
  send.onclick = async () => {
    const answers = qs.map((item, qi) => {
      const el = d.querySelector(`.qitem[data-qi="${qi}"]`);
      const picked = [...el.querySelectorAll(`input[name=q${qi}]:checked`)];
      const choices = picked
        .filter((i) => i.value !== "other")
        .map((i) => (item.options[Number(i.value)] || {}).label || "");
      const other = picked.some((i) => i.value === "other")
        ? el.querySelector(".qfree").value.trim()
        : "";
      return other ? { choices, other } : { choices };
    });
    send.disabled = true;
    try { await rpc("ai.answer", { id: q.id, answers }); }
    catch (e) { send.disabled = false; dlgAlert(S.chat.questionFailed(e.message)); }
  };
  bar.appendChild(send);
  d.appendChild(bar);
  return d;
}

function renderToolBlock(tu, result, approval) {
  const meta = toolMeta(tu.name, tu.input || {});
  let st = "running";
  if (approval) st = "approval";
  else if (result) st = result.denied ? "denied" : result.is_error ? "error" : "done";
  else if (!aiBusy) st = "interrupted";

  const d = div(`tool st-${st}`);
  const open = st === "error" ? " open" : "";
  let body = `<pre class="tinput">${esc(clip(pretty(tu.input || {}), 4000))}</pre>`;
  if (result) {
    body += `<pre class="toutput${result.is_error ? " terr" : ""}">${esc(clip(pretty(result.content), 4000))}</pre>`;
  }
  d.innerHTML =
    `<details${open}><summary><span class="ticon">${I(meta.icon, 13)}</span>` +
    `<b>${esc(meta.title)}</b> <span class="tdetail">${esc(clip(meta.detail, 90))}</span>` +
    `<span class="tstate">${stateLabel[st]}</span></summary>` +
    `<div class="tbody">${body}</div></details>`;
  // A presented file stays one click away after the fact.
  if (tu.name === "present_file" && tu.input && tu.input.name) {
    const b = document.createElement("button");
    b.className = "topen";
    b.innerHTML = I("eye", 12) + " " + S.chat.openFile(esc(tu.input.name));
    b.onclick = () => openViewer(tu.input.name, tu.input.mode || "auto", "assistant");
    d.querySelector(".tbody").appendChild(b);
  }
  // A sub-agent's own conversation is one click away (read-only there).
  if (tu.name === "agent_run" || tu.name === "agent_send") {
    const agentSession = tu.name === "agent_send"
      ? (tu.input || {}).session
      : (() => { try { return JSON.parse(result && result.content).session; } catch { return null; } })();
    let rep = null;
    try { rep = JSON.parse(result.content); } catch {}
    const bodyEl = d.querySelector(".tbody");
    if (rep && rep.report) {
      const pane = div("agentreport");
      pane.innerHTML = mdRender(rep.report);
      bodyEl.appendChild(pane);
      const steps = div("tdetail");
      steps.textContent = S.chat.stepCount(rep.steps);
      bodyEl.appendChild(steps);
    }
    if (agentSession) {
      const b = document.createElement("button");
      b.className = "topen";
      b.innerHTML = I("bot", 12) + " " + S.chat.openAgentChat;
      b.onclick = () => switchSession(agentSession);
      bodyEl.appendChild(b);
    }
  }
  if (approval) {
    const bar = div("approve-bar");
    // note = why this prompt appears even in auto mode (local DB file,
    // local network address); "always" then only covers that access kind.
    bar.innerHTML =
      (approval.note ? `<div class="approve-note">${esc(approval.note)}</div>` : "") +
      `<span class="approve-q">${I("shield-alert", 13)} ${S.chat.approvalQ}</span>` +
      `<button class="ok">${S.chat.approvalAllow}</button><button class="always">${S.chat.approvalAlwaysAllow(esc(tu.name))}</button><button class="no">${S.chat.approvalDeny}</button>`;
    bar.querySelector(".ok").onclick = () => rpc("ai.approve", { id: tu.id, allow: true });
    bar.querySelector(".always").onclick = () => rpc("ai.approve", { id: tu.id, allow: true, always: true });
    bar.querySelector(".no").onclick = () => rpc("ai.approve", { id: tu.id, allow: false });
    d.appendChild(bar);
    // write_file / edit_file: show what would actually change, not raw JSON.
    if (tu.name === "write_file" && tu.input && tu.input.name) {
      attachWriteDiff(d, bar, tu.input.name, String(tu.input.content ?? ""));
    } else if (tu.name === "edit_file" && tu.input && tu.input.name) {
      attachEditDiff(d, bar, tu.input);
    }
  }
  return d;
}

/// Fetch the file's current content and prepend a compact old/new diff to
/// the approval bar: the unchanged prefix/suffix is trimmed away and the
/// differing middle shown red/green (clipped). New files say so instead.
async function attachWriteDiff(card, bar, name, next) {
  let prev = null;
  try {
    const r = await fetch(appUrl(name), { cache: "no-store" });
    if (r.ok) prev = await r.text();
  } catch {}
  if (!bar.isConnected) return; // approval resolved (or chat re-rendered) meanwhile
  const box = div("write-diff");
  if (prev == null) {
    box.innerHTML = `<span class="wd-new">${S.chat.diffCreates(esc(name), fmtSize(new TextEncoder().encode(next).length))}</span>`;
  } else if (prev === next) {
    box.innerHTML = `<span class="wd-new">${S.chat.diffRewriteIdentical(esc(name))}</span>`;
  } else {
    renderLineDiff(box, name, prev, next);
  }
  bar.insertBefore(box, bar.firstChild);
}

/// edit_file approval: apply the replacement locally (mirroring the server's
/// semantics) and show the same red/green preview — or say why it will fail.
async function attachEditDiff(card, bar, input) {
  const name = String(input.name), old = String(input.old_string ?? "");
  let prev = null;
  try {
    const r = await fetch(appUrl(name), { cache: "no-store" });
    if (r.ok) prev = await r.text();
  } catch {}
  if (!bar.isConnected) return;
  const box = div("write-diff");
  const n = prev == null || old === "" ? 0 : prev.split(old).length - 1;
  if (prev == null) {
    box.innerHTML = `<span class="wd-new">${S.chat.diffEditNoPreview(esc(name))}</span>`;
  } else if (n === 0) {
    box.innerHTML = `<span class="wd-new">${S.chat.diffEditNotFound(esc(name))}</span>`;
  } else {
    const nw = String(input.new_string ?? "");
    const next = input.replace_all ? prev.split(old).join(nw) : prev.replace(old, nw);
    renderLineDiff(box, name, prev, next);
  }
  bar.insertBefore(box, bar.firstChild);
}

/// Shared red/green preview: trim the common prefix/suffix lines, show the
/// differing middle (clipped).
function renderLineDiff(box, name, prev, next) {
  {
    const a = prev.split("\n"), b = next.split("\n");
    let s = 0;
    while (s < a.length && s < b.length && a[s] === b[s]) s++;
    let e = 0;
    while (e < a.length - s && e < b.length - s && a[a.length - 1 - e] === b[b.length - 1 - e]) e++;
    const clipLines = (lines, sign) =>
      lines.slice(0, 12).map((l) => `<div class="wd-${sign}">${sign === "del" ? "-" : "+"} ${esc(clip(l, 160))}</div>`).join("") +
      (lines.length > 12 ? `<div class="wd-more">…${lines.length - 12} more</div>` : "");
    box.innerHTML =
      `<div class="wd-head">${S.chat.diffChanges(esc(name), s + 1)}</div>` +
      clipLines(a.slice(s, a.length - e), "del") + clipLines(b.slice(s, b.length - e), "add");
  }
}

// ---------- chat sessions (switcher) ----------
const sessionMeta = (id) => sessions.find((s) => s.id === id) || null;
const sessionTitle = (s) =>
  (s && s.title) || (s && s.id === "main" ? S.chat.mainChat : s ? (s.kind === "agent" ? S.chat.agentTitle : S.chat.chatTitle) : S.chat.chatTitle);

let sessTimer = null;
/// Chat ops arrive in bursts (assistant + tool rows) — coalesce the list refresh.
function refreshSessionsSoon() { clearTimeout(sessTimer); sessTimer = setTimeout(refreshSessions, 300); }
async function refreshSessions() {
  try { sessions = await rpc("sessions.list"); } catch { return; }
  // The active chat can vanish (deleted on another device) — fall back to main.
  if (session !== "main" && !sessionMeta(session)) { session = "main"; persistSession(); refreshChat(); }
  renderSwitcher();
}
function persistSession() {
  if (state.appId) localStorage.setItem(`uapp.session.${state.appId}`, session);
}
function renderSwitcher() {
  const cur = sessionMeta(session);
  const label = $("chat-title");
  if (label) label.textContent = sessionTitle(cur) || S.chat.title;
  const btn = $("chat-switch");
  if (!btn) return;
  // Chats other than this one still working: a subtle dot on the switcher.
  const others = busySessions.filter((s) => s !== session).length;
  btn.classList.toggle("working", others > 0);
  btn.title = others ? S.chat.otherChatWorking(others) : S.chat.switchChat;
  renderAgentBanner();
}
/// Agent conversations are read-only: they belong to an autonomous agent.
function renderAgentBanner() {
  const b = $("agent-banner");
  if (!b) return;
  const cur = sessionMeta(session);
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
  if (!id || id === session) { if (id) openPanel("sidebar"); return; }
  session = id;
  persistSession();
  draftText = "";
  openPanel("sidebar");
  await refreshSessions();
  // This chat's own busy state (another chat's run must not show here).
  try { const info = await rpc("app.info"); setBusy((info.aiSessions || []).includes(session)); } catch {}
  refreshChat();
}
function chatMenu(x, y) {
  const items = [];
  for (const s of sessions.filter((s) => s.kind !== "agent" && !s.archived)) {
    const working = busySessions.includes(s.id) ? " ●" : "";
    items.push({
      icon: s.id === session ? "check" : "message-square",
      label: `${sessionTitle(s)} (${s.messages || 0})${working}`,
      run: () => switchSession(s.id),
    });
  }
  // Agent conversations stay out of the list, but archived chats can return.
  const archived = sessions.filter((s) => s.kind !== "agent" && s.archived);
  if (archived.length) {
    items.push({ sep: true });
    for (const s of archived) {
      items.push({ icon: "archive", label: `${sessionTitle(s)} (archived)`, run: () => switchSession(s.id) });
    }
  }
  items.push({ sep: true });
  items.push({ icon: "plus", label: S.chat.menuNewChat, run: newChat });
  const cur = sessionMeta(session);
  if (cur && cur.kind !== "agent") {
    items.push({ icon: "pencil", label: S.chat.menuRenameChat, run: renameChat });
    items.push({
      icon: "archive", label: cur.archived ? S.chat.menuUnarchive : S.chat.menuArchive,
      run: async () => { await rpc("sessions.archive", { id: session, archived: !cur.archived }); refreshSessions(); },
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
  const cur = sessionMeta(session);
  const title = await dlgInput(S.chat.renameChatPrompt, sessionTitle(cur), S.common.rename);
  if (title === null || !title.trim()) return;
  await rpc("sessions.rename", { id: session, title: title.trim() });
  refreshSessions();
}
async function deleteChat() {
  const cur = sessionMeta(session);
  if (!(await dlgConfirm(S.chat.deleteConfirm(sessionTitle(cur))))) return;
  const id = session;
  session = "main";
  persistSession();
  await rpc("sessions.delete", { id });
  await refreshSessions();
  refreshChat();
}
async function compactChat() {
  if (!(await dlgConfirm(S.chat.compactConfirm))) return;
  setStatus(S.chat.statusCompacting);
  try {
    const r = await rpc("chat.compact", { session });
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
  const s = $("staged");
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
  if (await dlgConfirm(S.chat.clearConfirm)) await rpc("chat.clear", { session });
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
    await rpc("chat.send", { text, attachments: atts, refs: r, mode: state.mode, session });
  } catch (e) {
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
  if (p.session && p.session !== session) {
    if (p.state === "running" || p.state === "streaming" || p.state === "thinking" || p.state === "tool") {
      if (!busySessions.includes(p.session)) { busySessions.push(p.session); renderSwitcher(); }
    } else {
      busySessions = busySessions.filter((s) => s !== p.session);
      renderSwitcher();
      refreshSessionsSoon();
    }
    return;
  }
  if (p.state === "running") { setBusy(true); setStatus(p.note || S.chat.statusThinking); }
  else if (p.state === "thinking") {
    aiBusy = true; setBusy(true);
    setStatus(p.chars ? S.chat.statusThinkingK((p.chars / 1000).toFixed(1)) : S.chat.statusThinking);
  } else if (p.state === "streaming") {
    aiBusy = true;
    draftText = p.text || "";
    setStatus(S.chat.statusWriting);
    renderDraftThrottled();
  } else if (p.state === "tool") { draftText = ""; setBusy(true); setStatus(S.chat.statusRunning(p.tool)); }
  else { draftText = ""; setBusy(false); setStatus(""); refreshChat(); }
}

function onInfo(info) {
  const savedSession = state.appId && localStorage.getItem(`uapp.session.${state.appId}`);
  if (savedSession && savedSession !== session) { session = savedSession; refreshChat(); }
  busySessions = info.aiSessions || [];
  // Busy is per session: only this conversation's run drives the UI.
  setBusy(busySessions.includes(session));
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
  const btn = $("btn-mode");
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

  $("btn-chat-compact").innerHTML = I("package", 14);
  $("btn-clear").innerHTML = I("trash-2", 14);
  $("btn-close").innerHTML = I("x", 15);
  $("btn-attach").innerHTML = I("paperclip", 14);
  $("btn-fromapp").innerHTML = I("folder-open", 14);
  $("btn-send").innerHTML = I("send-horizontal", 15);
  $("btn-stop").innerHTML = I("square", 13);

  // "Configure a provider" banner: shown while app.info reports no AI
  // provider (absent field = older core → stay hidden). config.set triggers
  // a fresh app.info push, so it hides the moment a provider is saved.
  $("ai-setup-msg").innerHTML =
    I("circle-alert", 13) + " " + S.chat.setupMsg;
  $("btn-ai-setup").innerHTML = I("settings", 12) + " " + S.chat.setupBtn;
  $("btn-ai-setup").onclick = () => openPanel("settingspanel");
  on("info", (info) => $("ai-setup").classList.toggle("hidden", info.aiConfigured !== false));

  $("btn-close").onclick = () => closePanel("sidebar");
  $("btn-send").onclick = send;
  $("btn-stop").onclick = () => rpc("ai.stop", { session });
  $("btn-chat-compact").onclick = compactChat;
  $("btn-clear").onclick = clearChat;
  renderMode();
  $("btn-mode").onclick = () => {
    setMode(state.mode === "auto" ? "manual" : state.mode === "manual" ? "plan" : "auto");
    renderMode();
  };
  $("chat-switch").onclick = (e) => {
    const r = e.currentTarget.getBoundingClientRect();
    chatMenu(r.left, r.bottom + 4);
  };
  $("chat-input").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
  });
  $("chat-input").addEventListener("paste", (e) => {
    for (const item of e.clipboardData?.items || []) {
      if (item.kind === "file") { const f = item.getAsFile(); if (f) { stageFile(f); e.preventDefault(); } }
    }
  });
  $("btn-attach").onclick = () => $("file-input").click();
  $("file-input").addEventListener("change", (e) => {
    for (const f of e.target.files) stageFile(f);
    e.target.value = "";
  });
  $("btn-fromapp").onclick = () => fromAppDialog().catch((e) => dlgAlert(e.message));

  // Right-click a chat bubble to copy its markdown source. When text is
  // selected inside it, the browser's own copy menu is more useful — keep it.
  $("messages").addEventListener("contextmenu", (e) => {
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
    try { approvals = await rpc("ai.pending"); renderChatSoon(); } catch {}
    try { questions = await rpc("ai.questions"); renderChatSoon(); } catch {}
  });
  on("chat-changed", () => { draftText = ""; refreshChat(); refreshSessionsSoon(); });
  on("ai", onAiEvent);
  on("approval", (pending) => { approvals = pending; renderChatSoon(); });
  on("question", (pending) => { questions = pending; renderChatSoon(); });
  on("info", onInfo);
  on("info", renderMode); // the saved per-app mode loads with app.info
  on("mode", renderMode);
}

customElements.define("chat-panel", ChatPanel);
