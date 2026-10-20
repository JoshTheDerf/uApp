//! Sub-agents: delegated loops in their own chat session.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::compact::clip;
use super::history::build_messages;
use super::run::run_loop;
use super::{current_ctx, RunCtx, AUTO_COMPACT_BYTES, FORK_DIGEST_BYTES};

/// Delegate a self-contained task to a sub-agent: its own chat session, the
/// same tools (minus the agent tools), the same approval gate, run
/// synchronously on this thread so the calling loop simply waits for a report.
pub fn agent_run(app: &Arc<App>, input: &Value) -> Result<Value> {
    let ctx = current_ctx();
    if ctx.depth > 0 {
        bail!("sub-agents cannot start sub-agents");
    }
    let description = input["description"].as_str().unwrap_or("").trim();
    let description = if description.is_empty() {
        "agent task"
    } else {
        description
    };
    let prompt = input["prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("agent_run needs a non-empty prompt"))?;
    let mode = match input["mode"].as_str().unwrap_or("fresh") {
        "fork" => "fork",
        "fresh" => "fresh",
        other => bail!("mode must be \"fresh\" or \"fork\" (got {other})"),
    };
    let max_steps = input["max_steps"].as_u64().unwrap_or(12).clamp(1, 24) as usize;
    let session = format!("ag-{}-{}", crate::store::now_ms(), fastrand::u32(..));
    rpc::local_op(
        app,
        "session_meta",
        json!({
            "id": session, "title": clip(description, 200), "kind": "agent",
            "parent": ctx.session, "created": crate::store::now_ms(),
        }),
    )?;
    let seed = if mode == "fork" {
        let digest = match build_messages(app, &ctx.session, AUTO_COMPACT_BYTES) {
            Ok(msgs) => clip(&json!(msgs).to_string(), FORK_DIGEST_BYTES),
            Err(_) => String::new(),
        };
        format!("Context from the conversation you are helping with (JSON digest):\n{digest}\n\nYour task:\n{prompt}")
    } else {
        prompt.to_string()
    };
    rpc::local_op(
        app,
        "chat",
        json!({
            "mid": format!("{}-{}", crate::store::now_ms(), fastrand::u32(..)),
            "role": "user", "session": session,
            "content": {"text": seed, "attachments": []},
        }),
    )?;
    agent_loop(app, &ctx, session, max_steps)
}

/// Continue an existing sub-agent conversation with another instruction.
pub fn agent_send(app: &Arc<App>, input: &Value) -> Result<Value> {
    let ctx = current_ctx();
    if ctx.depth > 0 {
        bail!("sub-agents cannot drive other sub-agents");
    }
    let session = rpc::session_id(
        input["session"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_send needs a session"))?,
    )?;
    let message = input["message"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("agent_send needs a non-empty message"))?;
    let kind: Option<String> = {
        let eng = app.eng();
        let r = store::query(
            &eng.db,
            "SELECT kind FROM uapp_sessions WHERE id = ?1",
            &[json!(session)],
        )?;
        r["rows"][0][0].as_str().map(|s| s.to_string())
    };
    match kind.as_deref() {
        Some("agent") => {}
        Some(other) => bail!("session {session} is a '{other}' chat, not an agent conversation"),
        None => bail!("no agent session named {session}"),
    }
    rpc::local_op(
        app,
        "chat",
        json!({
            "mid": format!("{}-{}", crate::store::now_ms(), fastrand::u32(..)),
            "role": "user", "session": session,
            "content": {"text": message, "attachments": []},
        }),
    )?;
    agent_loop(app, &ctx, session, 12)
}

/// Run one sub-agent loop to completion and shape its tool result.
pub(crate) fn agent_loop(
    app: &Arc<App>,
    parent: &RunCtx,
    session: String,
    max_steps: usize,
) -> Result<Value> {
    let child = parent.child(session.clone());
    let out = run_loop(app, &child, max_steps)?;
    let report = if out.report.trim().is_empty() {
        "(the sub-agent produced no final text — open its conversation to see what it did)"
            .to_string()
    } else {
        out.report
    };
    Ok(json!({"session": session, "steps": out.steps, "report": report}))
}
