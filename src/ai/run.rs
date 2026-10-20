//! The tool loop.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::approval::approval_gate;
use super::compact::Compactor;
use super::history::{build_messages, Budget};
use super::prompt::{app_snapshot, system_prompt};
use super::providers::{call_provider_retrying, is_stopped, Request, Usage};
use super::toolset::{action_list, run_target, toolset_key, unwrap_call_action, Target, Toolset};
use super::{
    compact_bytes, load_config, plan_mode, CtxGuard, RunCtx, MAX_ITERATIONS, TOOL_RESULT_LIMIT,
};

/// What a finished loop produced: how many provider turns it took and the
/// assistant's final text (a sub-agent's report).
pub(crate) struct Outcome {
    pub(crate) steps: usize,
    pub(crate) report: String,
    /// False when the step limit cut the loop off mid-task.
    pub(crate) completed: bool,
}

pub fn run(app: &Arc<App>, ctx: &RunCtx) -> Result<()> {
    run_loop(app, ctx, MAX_ITERATIONS).map(|_| ())
}

/// What a run fixes at its start and keeps for every step: the system
/// prompt and the tool list (see toolset.rs for why). Rebuilt only when
/// plan mode, the MCP servers or the disabled tools change mid-run, which
/// are explicit choices.
struct Frozen {
    system: String,
    toolset: Toolset,
    plan: bool,
}

impl Frozen {
    fn build(app: &Arc<App>, ctx: &RunCtx, plan: bool) -> Frozen {
        Frozen {
            system: system_prompt(app, ctx, plan),
            toolset: Toolset::freeze(app, ctx, plan),
            plan,
        }
    }
    fn refresh(&mut self, app: &Arc<App>, ctx: &RunCtx, plan: bool) {
        if plan != self.plan || toolset_key(app, plan) != self.toolset.key {
            *self = Frozen::build(app, ctx, plan);
        }
    }
}

/// Append the app snapshot to the session when it differs from the newest
/// one there. Each one stays where it was written, so earlier requests
/// remain a prefix of later ones.
fn deliver_snapshot(app: &Arc<App>, session: &str) -> Result<()> {
    let snapshot = app_snapshot(app);
    let last = {
        let eng = app.eng();
        store::query(
            &eng.db,
            "SELECT content FROM uapp_chat WHERE COALESCE(session,'main') = ?1 \
             AND role = 'context' ORDER BY id DESC LIMIT 1",
            &[json!(session)],
        )?["rows"][0][0]
            .as_str()
            .and_then(|c| serde_json::from_str::<Value>(c).ok())
            .and_then(|c| c["text"].as_str().map(String::from))
    };
    if last.as_deref() == Some(snapshot.as_str()) {
        return Ok(());
    }
    rpc::local_op(
        app,
        "chat",
        json!({"mid": format!("c-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
               "role": "context", "session": session, "content": {"text": snapshot}}),
    )?;
    Ok(())
}

pub(crate) fn run_loop(app: &Arc<App>, ctx: &RunCtx, max_iters: usize) -> Result<Outcome> {
    let _guard = CtxGuard::enter(ctx);
    let cfg = load_config(app)?;
    // Budget for compaction and for the outgoing hard trim. A local server's
    // context window is far smaller than a cloud model's, so it gets a tighter
    // one.
    let cap = compact_bytes(&cfg);
    let mut frozen = Frozen::build(app, ctx, plan_mode(app, ctx));
    let mut compactor = Compactor::default();
    let mut steps = 0usize;
    let mut report = String::new();

    // False only when the loop ran out of steps with work still going.
    let mut ended = false;
    for _ in 0..max_iters {
        if ctx.stopped() {
            ended = true;
            break;
        }
        // Every iteration, not just at run start: a long tool-using run grows
        // the history by tens of KB per step and would otherwise sail past the
        // model's context window mid-run.
        compactor.maybe_compact(app, ctx, &cfg, cap);
        frozen.refresh(app, ctx, plan_mode(app, ctx));
        deliver_snapshot(app, &ctx.session)?;
        // Captured BEFORE building: any user row landing after this id was not
        // part of this turn's context and still needs an answer.
        let seen_id = last_chat_id(app, &ctx.session);
        let msgs = build_messages(app, &ctx.session, Budget::for_cap(cap))?;
        if msgs.is_empty() {
            ended = true;
            break;
        }
        let stop = || ctx.stopped();
        let req = Request {
            session: &ctx.session,
            system: &frozen.system,
            msgs: &msgs,
            tools: &frozen.toolset.tools,
            stop: &stop,
        };
        let reply = match call_provider_retrying(app, ctx, &cfg, &req) {
            Ok(r) => r,
            // Stop is not an error: the user asked for it.
            Err(e) if is_stopped(&e) => {
                ended = true;
                break;
            }
            Err(e) => return Err(e),
        };
        // Stopped while the reply came in: drop it. Writing it now would
        // also re-create rows in a chat that sessions.delete just removed.
        if ctx.stopped() {
            ended = true;
            break;
        }
        steps += 1;
        let blocks = reply.blocks;
        let tool_uses: Vec<Value> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .cloned()
            .collect();
        let text: String = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(|t| t.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            report = text;
        }
        let mid = format!("a-{}-{}", crate::store::now_ms(), fastrand::u32(..));
        rpc::local_op(
            app,
            "chat",
            json!({"mid": mid, "role": "assistant", "session": ctx.session,
                   "content": assistant_content(&blocks, &reply.model, reply.usage)}),
        )?;
        if tool_uses.is_empty() {
            // A message sent while this turn was in flight (this device or
            // another) would otherwise sit unanswered: chat.send sees the run
            // as live and doesn't start a second one.
            if has_user_after(app, &ctx.session, seen_id) {
                continue;
            }
            ended = true;
            break;
        }
        let mut results = Vec::new();
        for tu in &tool_uses {
            results.push(run_call(app, ctx, &frozen.toolset, tu));
        }
        // Auto-surface (but don't dump): if the live app has logged new
        // errors/warnings since the AI last looked — e.g. the edit just made
        // reloaded the iframe and it threw — staple a one-line nudge to the
        // final result so the model knows to call read_console.
        if let Some(note) = app.console_alert() {
            if let Some(last) = results.last_mut() {
                if let Some(c) = last["content"].as_str() {
                    last["content"] = json!(format!("{c}{note}"));
                }
            }
        }
        // The results say what ran, so they are kept after a Stop too, unless
        // the chat itself is gone (sessions.delete stops the run first).
        if ctx.stopped() && !row_exists(app, &mid) {
            ended = true;
            break;
        }
        let tmid = format!("t-{}-{}", crate::store::now_ms(), fastrand::u32(..));
        rpc::local_op(
            app,
            "chat",
            json!({"mid": tmid, "role": "tool", "session": ctx.session,
                   "content": {"results": results}}),
        )?;
    }
    if !ended && !ctx.stopped() {
        // Out of steps mid-task. Say so in the chat; ending as a plain
        // "idle" looked exactly like a finished answer.
        rpc::local_op(
            app,
            "chat",
            json!({"mid": format!("cap-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
                   "role": "system", "session": ctx.session,
                   "content": {"text": format!(
                       "Stopped after {max_iters} steps without finishing. Send a message \
                        (\"continue\" is enough) to let the assistant keep going.")}}),
        )?;
    }
    Ok(Outcome {
        steps,
        report,
        completed: ended,
    })
}

/// Does this chat row still exist?
fn row_exists(app: &Arc<App>, mid: &str) -> bool {
    let eng = app.eng();
    store::query(
        &eng.db,
        "SELECT count(*) FROM uapp_chat WHERE mid = ?1",
        &[json!(mid)],
    )
    .map(|r| r["rows"][0][0].as_i64().unwrap_or(0) > 0)
    .unwrap_or(false)
}

/// Run one tool call the model made, through plan mode, the disabled list
/// and the approval gate, and shape its result.
fn run_call(app: &Arc<App>, ctx: &RunCtx, toolset: &Toolset, tu: &Value) -> Value {
    let fail = |content: String| json!({"id": tu["id"], "name": tu["name"], "content": content, "is_error": true});
    if ctx.stopped() {
        return fail("(stopped by user)".into());
    }
    let called = tu["name"].as_str().unwrap_or("");
    let id = tu["id"].as_str().unwrap_or("");
    let (target, mut input) = match toolset.resolve(app, called) {
        Ok(t) => unwrap_call_action(t, &tu["input"]),
        Err(e) => return fail(format!("Error: {e}")),
    };
    let name = target.raw_name();
    if crate::tools::disabled_set(app).contains(&name) {
        return fail(format!(
            "Error: {name} is switched off in this app's tool settings"
        ));
    }
    // A registered action decides whether it's gated (its readonly flag), so
    // let one the page is still registering after a reload arrive first.
    if let Target::App(action) = &target {
        if !action.is_empty() && app.wait_for_action(action).is_err() {
            return fail(format!(
                "Error: no app action named '{action}' is registered. Registered actions: {}",
                action_list(app)
            ));
        }
    }
    // Plan mode backstop: write tools are absent from the offered toolset,
    // but a hallucinated call must not execute either.
    if plan_mode(app, ctx)
        && (matches!(target, Target::Mcp { .. })
            || crate::tools::is_gated(app, &name)
            || crate::tools::is_agent_tool(&name))
    {
        return fail(
            "Plan mode is read-only — this tool is unavailable. \
             Propose the change; the user will switch modes to apply it."
                .into(),
        );
    }
    let forced = crate::tools::forced_gate(&name, &input);
    if !approval_gate(app, ctx, id, &name, &input, &forced) {
        // Denied: a normal (non-crashing) result the model can adapt to.
        let mut r = fail(format!("The user denied permission to run {name}."));
        r["denied"] = json!(true);
        return r;
    }
    app.notify(
        "ai",
        json!({"state": "tool", "tool": name, "session": ctx.session}),
    );
    if forced.is_some() {
        // Marks the user's approval for the RPC layer; stripped from
        // anything arriving over the wire, so only this path sets it.
        input["_user_approved"] = json!(true);
    }
    let r = if let Some(bad) = tu["input"]["_invalid"].as_str() {
        Err(anyhow!(
            "the arguments of this tool call did not arrive as valid JSON (the response was \
             probably cut off) — the tool did NOT run. Send the call again. Received: {}",
            bad.chars().take(200).collect::<String>()
        ))
    } else {
        run_target(app, &target, &input)
    };
    let (mut content, is_error) = match r {
        Ok(v) => (v.to_string(), false),
        // A schema-shaped failure (bad/missing params) is fed back WITH
        // the tool's schema so the model can repair the call itself
        // instead of guessing the same shape again.
        Err(e) => (format!("Error: {e}{}", schema_hint(&name, &e)), true),
    };
    // Cap results AND errors: an MCP server's error text can be
    // arbitrarily large and would otherwise ride along in every later
    // request of the conversation.
    if content.len() > TOOL_RESULT_LIMIT {
        let was = content.len();
        // char-boundary-safe: a plain truncate panics mid-UTF-8
        crate::rpc::truncate_utf8(&mut content, TOOL_RESULT_LIMIT);
        content.push_str(&format!(
            "…(truncated — output was {}KB. Re-run with narrower parameters \
             (LIMIT, column lists, byte/row ranges) rather than requesting it all.)",
            was.div_ceil(1024)
        ));
    }
    json!({"id": tu["id"], "name": tu["name"], "content": content, "is_error": is_error})
}

/// An assistant row's content: the blocks, plus which model answered and
/// what the call cost in tokens (read back by `ai.usage`).
pub(crate) fn assistant_content(blocks: &[Value], model: &str, usage: Usage) -> Value {
    let mut c = json!({"blocks": blocks});
    if !model.is_empty() {
        c["model"] = json!(model);
    }
    if !usage.is_empty() {
        c["usage"] = usage.to_json();
    }
    c
}

/// Newest chat row id in a session — the watermark for "seen by this turn".
pub(crate) fn last_chat_id(app: &Arc<App>, session: &str) -> i64 {
    let eng = app.eng();
    store::query(
        &eng.db,
        "SELECT COALESCE(max(id), 0) FROM uapp_chat WHERE COALESCE(session,'main') = ?1",
        &[json!(session)],
    )
    .map(|r| r["rows"][0][0].as_i64().unwrap_or(0))
    .unwrap_or(0)
}

/// Did a user message land after the watermark? (Late sends into a session
/// whose loop was about to finish — including ones that arrived mid-provider
/// call, which sort BEFORE the assistant reply.)
pub(crate) fn has_user_after(app: &Arc<App>, session: &str, seen_id: i64) -> bool {
    let eng = app.eng();
    store::query(
        &eng.db,
        "SELECT count(*) FROM uapp_chat WHERE COALESCE(session,'main') = ?1 AND role = 'user'
         AND id > ?2",
        &[json!(session), json!(seen_id)],
    )
    .map(|r| r["rows"][0][0].as_i64().unwrap_or(0) > 0)
    .unwrap_or(false)
}

/// Is the newest row of this session a user message nobody answered yet?
pub fn unanswered(app: &Arc<App>, session: &str) -> bool {
    let eng = app.eng();
    store::query(
        &eng.db,
        "SELECT role FROM uapp_chat WHERE COALESCE(session,'main') = ?1 AND role != 'context'
         ORDER BY id DESC LIMIT 1",
        &[json!(session)],
    )
    .map(|r| r["rows"][0][0].as_str() == Some("user"))
    .unwrap_or(false)
}

/// For a failure that looks like a bad tool input, append the tool's JSON
/// schema so the next attempt can be well-formed.
pub(crate) fn schema_hint(name: &str, err: &anyhow::Error) -> String {
    let msg = err.to_string().to_lowercase();
    let schema_shaped = [
        "missing required",
        "must be",
        "expected",
        "invalid",
        "not an object",
        "unknown field",
        "needs ",
    ]
    .iter()
    .any(|k| msg.contains(k));
    if !schema_shaped {
        return String::new();
    }
    match crate::tools::registry().iter().find(|t| t.name == name) {
        Some(def) => format!(
            "\nInvalid input for {name}. Expected schema: {}",
            (def.schema)()
        ),
        None => String::new(),
    }
}
