//! The tool loop.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::approval::approval_gate;
use super::compact::auto_compact;
use super::history::build_messages;
use super::prompt::system_prompt;
use super::providers::{call_provider_retrying, is_stopped, Request, Usage};
use super::toolset::{app_action_tools, builtin_tools, mcp_tools, run_tool};
use super::{
    compact_bytes, load_config, plan_mode, CtxGuard, RunCtx, MAX_ITERATIONS, TOOL_RESULT_LIMIT,
};

/// What a finished loop produced: how many provider turns it took and the
/// assistant's final text (a sub-agent's report).
pub(crate) struct Outcome {
    pub(crate) steps: usize,
    pub(crate) report: String,
}

pub fn run(app: &Arc<App>, ctx: &RunCtx) -> Result<()> {
    run_loop(app, ctx, MAX_ITERATIONS).map(|_| ())
}

pub(crate) fn run_loop(app: &Arc<App>, ctx: &RunCtx, max_iters: usize) -> Result<Outcome> {
    let _guard = CtxGuard::enter(ctx);
    let cfg = load_config(app)?;
    // Budget for compaction and for the outgoing hard trim. A local server's
    // context window is far smaller than a cloud model's, so it gets a tighter
    // one.
    let cap = compact_bytes(&cfg);
    // Builtins are stable for the run. MCP tools are re-listed whenever the
    // mcp config changes (the model can add servers mid-run with
    // add_mcp_server). App actions are re-read each iteration because the
    // page re-registers them after every reload.
    let builtins = builtin_tools(app, ctx);
    let mut mcp_snapshot = String::new();
    let mut mcp_cached: Vec<Value> = Vec::new();
    let mut steps = 0usize;
    let mut report = String::new();

    for _ in 0..max_iters {
        if ctx.stopped() {
            break;
        }
        // Every iteration, not just at run start: a long tool-using run grows
        // the history by tens of KB per step and would otherwise sail past the
        // model's context window mid-run.
        auto_compact(app, ctx, cap);
        let system = system_prompt(app, ctx); // refreshed: schema/files change as we work
        let mcp_now = serde_json::to_string(&rpc::mcp_servers_enabled(app)).unwrap_or_default();
        if mcp_now != mcp_snapshot {
            mcp_cached = mcp_tools(app);
            mcp_snapshot = mcp_now;
        }
        let mut tools = builtins.clone();
        tools.extend(mcp_cached.clone());
        tools.extend(app_action_tools(app));
        // Captured BEFORE building: any user row landing after this id was not
        // part of this turn's context and still needs an answer.
        let seen_id = last_chat_id(app, &ctx.session);
        let msgs = build_messages(app, &ctx.session, cap)?;
        if msgs.is_empty() {
            break;
        }
        let stop = || ctx.stopped();
        let req = Request {
            session: &ctx.session,
            system: &system,
            msgs: &msgs,
            tools: &tools,
            stop: &stop,
        };
        let reply = match call_provider_retrying(app, ctx, &cfg, &req) {
            Ok(r) => r,
            // Stop is not an error: the user asked for it.
            Err(e) if is_stopped(&e) => break,
            Err(e) => return Err(e),
        };
        // Stopped while the reply came in: drop it. Writing it now would
        // also re-create rows in a chat that sessions.delete just removed.
        if ctx.stopped() {
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
            break;
        }
        let mut results = Vec::new();
        for tu in &tool_uses {
            if ctx.stopped() {
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": "(stopped by user)", "is_error": true}));
                continue;
            }
            let name = tu["name"].as_str().unwrap_or("");
            let id = tu["id"].as_str().unwrap_or("");
            // Plan mode backstop: write tools are absent from the offered
            // toolset, but a hallucinated call must not execute either.
            if plan_mode(app)
                && (crate::tools::is_gated(app, name) || crate::tools::is_agent_tool(name))
            {
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": "Plan mode is read-only — this tool is unavailable. \
                                Propose the change; the user will switch modes to apply it.",
                    "is_error": true}));
                continue;
            }
            let forced = crate::tools::forced_gate(name, &tu["input"]);
            if !approval_gate(app, ctx, id, name, &tu["input"], &forced) {
                // Denied: a normal (non-crashing) result the model can adapt to.
                results.push(json!({"id": tu["id"], "name": tu["name"],
                    "content": format!("The user denied permission to run {name}."),
                    "is_error": true, "denied": true}));
                continue;
            }
            app.notify(
                "ai",
                json!({"state": "tool", "tool": name, "session": ctx.session}),
            );
            let mut input = tu["input"].clone();
            if forced.is_some() {
                // Marks the user's approval for the RPC layer; stripped from
                // anything arriving over the wire, so only this path sets it.
                input["_user_approved"] = json!(true);
            }
            let r = if let Some(bad) = input["_invalid"].as_str() {
                Err(anyhow!(
                    "the arguments of this tool call did not arrive as valid JSON (the response was \
                     probably cut off) — the tool did NOT run. Send the call again. Received: {}",
                    bad.chars().take(200).collect::<String>()
                ))
            } else {
                run_tool(app, name, &input)
            };
            let (mut content, is_error) = match r {
                Ok(v) => (v.to_string(), false),
                // A schema-shaped failure (bad/missing params) is fed back WITH
                // the tool's schema so the model can repair the call itself
                // instead of guessing the same shape again.
                Err(e) => (format!("Error: {e}{}", schema_hint(name, &e)), true),
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
            results.push(json!({"id": tu["id"], "name": tu["name"],
                "content": content, "is_error": is_error}));
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
        let mid = format!("t-{}-{}", crate::store::now_ms(), fastrand::u32(..));
        rpc::local_op(
            app,
            "chat",
            json!({"mid": mid, "role": "tool", "session": ctx.session,
                   "content": {"results": results}}),
        )?;
    }
    Ok(Outcome { steps, report })
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
        "SELECT role FROM uapp_chat WHERE COALESCE(session,'main') = ?1 ORDER BY id DESC LIMIT 1",
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
