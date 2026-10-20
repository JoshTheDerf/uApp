/* The chat conversation: message rows, tool cards, approval bars, ask_user
 * question cards and the streaming draft.
 *
 * Rendering is keyed and incremental. Every row has a stable key (its message
 * id, or the tool call id) and a signature of what it shows; a repaint keeps
 * each element whose signature is unchanged, so an open tool card, a text
 * selection or a half-answered question survives the refreshes that arrive
 * every few seconds. A row whose signature changed is rebuilt in place, and
 * keeps its <details> open state. */

import { rpc } from "./core.js";
import { $, I, esc, clip, pretty, div, fmtSize, appUrl, installStyle, dlgAlert, toolMeta } from "./ui.js";
import { mdRender } from "./markdown.js";
import { openViewer } from "./viewer.js";
import { cs } from "./chat-state.js";
import { S } from "./strings.js";

const CSS = /* css */ `
#chat-messages { flex: 1; overflow-y: auto; padding: 10px; }
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
.tool.st-approval { border-color: var(--warn); box-shadow: 0 0 0 2px color-mix(in srgb, var(--warn) 40%, transparent); }
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
.write-diff .wd-del { color: var(--err-ink); background: color-mix(in srgb, var(--err-soft) 40%, transparent); }
.write-diff .wd-add { color: var(--ok-ink); background: color-mix(in srgb, var(--ok-soft) 40%, transparent); }
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
/* presented-file shortcut on a tool card */
.tool .topen {
  display: inline-flex; align-items: center; gap: 5px; margin-top: 4px;
  border: 1px solid var(--line-strong); background: var(--surface); border-radius: 7px;
  padding: 3px 10px; font: inherit; font-size: 12px; cursor: pointer;
}
.tool .topen:hover { background: var(--brand-wash); border-color: var(--brand-line); }
@media (pointer: coarse) { .tool .topen { padding: 7px 12px; } }
`;

const stateLabel = () => ({
  running: S.chat.stateRunning, done: S.chat.stateDone, error: S.chat.stateError,
  denied: S.chat.stateDenied, approval: S.chat.stateApproval, interrupted: S.chat.stateInterrupted,
});

/// Set by chat-panel.js: what a tool card's "Open agent conversation" does.
export const hooks = { switchSession: () => {} };

/// Short, stable fingerprint of a string (signatures stay small in the DOM).
function hash(s) {
  let h = 5381;
  s = String(s);
  for (let i = 0; i < s.length; i++) h = ((h << 5) + h + s.charCodeAt(i)) | 0;
  return (h >>> 0).toString(36) + ":" + s.length;
}

/// The newest AI failure gets a Retry. The server writes those rows with an
/// "err-" message id.
const isAiError = (mid) => String(mid || "").startsWith("err-");

// ---------- rows -> items ----------
/// Turn chat.list rows (plus this device's pending approvals and questions)
/// into an ordered list of {key, sig, make}. `make` builds the element.
function buildItems(rows) {
  const items = [];
  const resultById = new Map();
  for (const row of rows) {
    if (row[4] !== "tool") continue;
    let c = {}; try { c = JSON.parse(row[5]); } catch {}
    for (const res of c.results || []) resultById.set(res.id, res);
  }
  const approvalById = new Map(cs.approvals.map((a) => [a.id, a]));
  const renderedToolIds = new Set();
  const last = rows[rows.length - 1];

  for (const row of rows) {
    const [mid, , device, user, role, contentStr] = row;
    let content = {}; try { content = JSON.parse(contentStr); } catch {}
    if (role === "user") {
      items.push({ key: "m:" + mid, sig: hash(contentStr), make: () => userRow(content, user || device) });
    } else if (role === "assistant") {
      (content.blocks || []).forEach((b, i) => {
        if (b.type === "text" && b.text && b.text.trim()) {
          items.push({ key: `m:${mid}:${i}`, sig: hash(b.text), make: () => assistantRow(b.text) });
        } else if (b.type === "thinking" && b.thinking) {
          items.push({ key: `m:${mid}:${i}`, sig: hash(b.thinking), make: () => thinkingRow(b.thinking) });
        } else if (b.type === "tool_use") {
          renderedToolIds.add(b.id);
          const result = resultById.get(b.id), approval = approvalById.get(b.id);
          items.push({
            key: "t:" + b.id,
            sig: [toolState(result, approval), result ? hash(result.content) : "", approval ? hash(approval.note || "") : ""].join("|"),
            make: () => renderToolBlock(b, result, approval),
          });
        }
      });
    } else if (role === "system") {
      const retry = isAiError(mid) && row === last && !cs.aiBusy;
      items.push({ key: "m:" + mid, sig: hash(contentStr) + (retry ? "|retry" : ""), make: () => systemRow(mid, content.text || "", retry) });
    } else if (role === "summary") {
      items.push({ key: "m:" + mid, sig: hash(contentStr), make: () => summaryRow(content.text || "") });
    }
  }
  // Approvals with no visible tool block: a sub-agent's gated call lives in
  // its hidden agent session, but the prompt belongs to THIS (root) chat.
  // Render them standalone so they can actually be answered.
  for (const a of cs.approvals) {
    if ((a.session || "main") !== cs.session || renderedToolIds.has(a.id)) continue;
    items.push({
      key: "a:" + a.id, sig: "approval",
      make: () => renderToolBlock({ type: "tool_use", id: a.id, name: a.name, input: a.input }, undefined, a),
    });
  }
  // Open ask_user prompts for this chat.
  for (const q of cs.questions) {
    if ((q.session || "main") !== cs.session) continue;
    items.push({ key: "q:" + q.id, sig: hash(JSON.stringify(q.questions || [])), make: () => renderQuestionCard(q) });
  }
  if (cs.aiBusy) {
    if (cs.draftText) items.push({ key: "draft", sig: "draft", make: draftRow });
    else items.push({ key: "dots", sig: "dots", make: dotsRow });
  }
  return items;
}

function userRow(content, who) {
  const d = div("msg user");
  d.innerHTML = `<div class="who">${esc(who)}</div><div class="bubble">${mdRender(content.text || "")}</div>`;
  d.querySelector(".bubble").dataset.src = content.text || "";
  for (const a of content.attachments || []) {
    const chip = document.createElement("span");
    chip.className = "attchip";
    chip.innerHTML = I(a.ref ? "link" : "paperclip", 10) + " " + esc(a.name);
    d.appendChild(chip);
  }
  return d;
}
function assistantRow(text) {
  const d = div("msg assistant");
  d.innerHTML = `<div class="who">${S.chat.whoAssistant}</div><div class="bubble">${mdRender(text)}</div>`;
  d.querySelector(".bubble").dataset.src = text;
  return d;
}
function thinkingRow(text) {
  const d = div("thinkline");
  d.innerHTML = `<details><summary>${I("brain", 12)} ${S.chat.thinking}</summary><pre>${esc(clip(text, 4000))}</pre></details>`;
  return d;
}
function systemRow(mid, text, retry) {
  const d = div("msg system");
  d.innerHTML = `<div class="bubble">${esc(text)}</div>`;
  if (retry) {
    // Re-runs the loop on this chat (after fixing the API key, or a
    // transient provider failure).
    const b = document.createElement("button");
    b.className = "retrybtn";
    b.innerHTML = I("rotate-cw", 11) + " " + S.common.retry;
    b.onclick = () => rpc("ai.run", { session: cs.session });
    d.querySelector(".bubble").appendChild(b);
  }
  return d;
}
function summaryRow(text) {
  // Everything before this row was replaced by one summary.
  const d = div("msg summary");
  d.innerHTML = `<details><summary>${I("package", 12)} ${S.chat.compactedSummary}` +
    `</summary><div class="bubble">${mdRender(text)}</div></details>`;
  return d;
}
function draftRow() {
  const d = div("msg assistant");
  d.innerHTML = `<div class="who">${S.chat.whoAssistant}</div><div class="bubble" id="chat-draft" aria-busy="true">${mdRender(cs.draftText)}</div>`;
  return d;
}
function dotsRow() {
  const d = div("msg assistant thinkingdots");
  d.innerHTML = `<div class="bubble"><span></span><span></span><span></span></div>`;
  return d;
}

// ---------- reconcile ----------
/// A rebuilt row keeps the open/closed state of its <details>, matched by
/// position (a tool card's layout doesn't change between states).
function carryState(from, to) {
  const a = from.querySelectorAll("details"), b = to.querySelectorAll("details");
  if (a.length !== b.length) return;
  a.forEach((d, i) => { b[i].open = d.open; });
}

/// Repaint the conversation from chat.list rows, touching only what changed.
export function paintChat(rows) {
  installStyle("chat-messages", CSS);
  const box = $("chat-messages");
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 90;
  const old = new Map();
  for (const el of box.children) if (el.dataset.key) old.set(el.dataset.key, el);
  const targets = buildItems(rows).map((it) => {
    const prev = old.get(it.key);
    if (prev && prev.dataset.sig === it.sig) return prev;
    const el = it.make();
    el.dataset.key = it.key;
    el.dataset.sig = it.sig;
    if (prev) carryState(prev, el);
    return el;
  });
  // Put targets[i] at position i; anything left over at the end is stale.
  // Rows that didn't move are never re-inserted, so their state is untouched.
  targets.forEach((el, i) => {
    if (box.children[i] !== el) box.insertBefore(el, box.children[i] || null);
  });
  while (box.children.length > targets.length) box.lastElementChild.remove();
  if (nearBottom) box.scrollTop = box.scrollHeight;
  return targets.length;
}

/// Drop every row (switching chats: nothing carries over).
export function clearChat() {
  $("chat-messages")?.replaceChildren();
}

/// Update the streaming draft in place. Returns false when there is no
/// draft row yet (the caller then repaints the conversation).
export function paintDraft() {
  const el = $("chat-draft");
  if (!el) return false;
  el.innerHTML = mdRender(cs.draftText);
  const box = $("chat-messages");
  if (box.scrollHeight - box.scrollTop - box.clientHeight < 160) box.scrollTop = box.scrollHeight;
  return true;
}

// ---------- question card ----------
/// One ask_user prompt: 1-4 questions, each a radio group (or checkboxes
/// when multiSelect) plus an "Other" free-text field. Submit sends every
/// question's answer in order; blanks are allowed (they read as unanswered).
function renderQuestionCard(q) {
  const d = div("tool st-approval question");
  const qs = q.questions || [];
  // Radio names must be unique per card: two open prompts would otherwise
  // share one group.
  const grp = (qi) => `q-${q.id}-${qi}`;
  let inner = `<div class="qhead">${I("circle-help", 14)} <b>${S.chat.questionHeading}</b></div>`;
  qs.forEach((item, qi) => {
    inner += `<div class="qitem" data-qi="${qi}">`;
    if (item.header) inner += `<span class="qchip">${esc(item.header)}</span>`;
    inner += `<div class="qtext">${esc(item.question || "")}</div>`;
    const type = item.multiSelect ? "checkbox" : "radio";
    for (const [oi, o] of (item.options || []).entries()) {
      inner += `<label class="qopt"><input type="${type}" name="${esc(grp(qi))}" value="${oi}">` +
        `<span><span class="qlabel">${esc(o.label || "")}</span>` +
        (o.description ? `<span class="qdesc">${esc(o.description)}</span>` : "") +
        `</span></label>`;
    }
    inner += `<label class="qopt qother"><input type="${type}" name="${esc(grp(qi))}" value="other">` +
      `<span class="qlabel">${S.chat.questionOther}</span></label>` +
      `<input type="text" class="qfree" placeholder="${S.chat.questionFreeText}" aria-label="${S.chat.questionOther}">`;
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
      const picked = [...el.querySelectorAll("input:checked")];
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

// ---------- tool card ----------
function toolState(result, approval) {
  if (approval) return "approval";
  if (result) return result.denied ? "denied" : result.is_error ? "error" : "done";
  return cs.aiBusy ? "running" : "interrupted";
}

function renderToolBlock(tu, result, approval) {
  const meta = toolMeta(tu.name, tu.input || {});
  const st = toolState(result, approval);
  const d = div(`tool st-${st}`);
  const open = st === "error" ? " open" : "";
  let body = `<pre class="tinput">${esc(clip(pretty(tu.input || {}), 4000))}</pre>`;
  if (result) {
    body += `<pre class="toutput${result.is_error ? " terr" : ""}">${esc(clip(pretty(result.content), 4000))}</pre>`;
  }
  d.innerHTML =
    `<details${open}><summary><span class="ticon">${I(meta.icon, 13)}</span>` +
    `<b>${esc(meta.title)}</b> <span class="tdetail">${esc(clip(meta.detail, 90))}</span>` +
    `<span class="tstate">${stateLabel()[st]}</span></summary>` +
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
      b.onclick = () => hooks.switchSession(agentSession);
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
      attachWriteDiff(bar, tu.input.name, String(tu.input.content ?? ""));
    } else if (tu.name === "edit_file" && tu.input && tu.input.name) {
      attachEditDiff(bar, tu.input);
    }
  }
  return d;
}

/// Fetch the file's current content and prepend a compact old/new diff to
/// the approval bar: the unchanged prefix/suffix is trimmed away and the
/// differing middle shown red/green (clipped). New files say so instead.
async function attachWriteDiff(bar, name, next) {
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
/// semantics) and show the same red/green preview, or say why it will fail.
async function attachEditDiff(bar, input) {
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
