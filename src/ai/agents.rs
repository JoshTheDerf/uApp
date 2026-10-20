//! Sub-agents: delegated loops in their own chat session.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::compact::clip;
use super::history::{load_messages, Msg};
use super::run::run_loop;
use super::{current_ctx, RunCtx, FORK_DIGEST_BYTES};

/// A `fork` sub-agent's copy of the parent conversation: the newest `cap`
/// bytes of it as JSON. The end matters most (it holds the task at hand),
/// and images and reasoning are left out, since base64 and signatures would
/// crowd out everything else.
pub(crate) fn fork_digest(mut msgs: Vec<Msg>, cap: usize) -> String {
    for (_, blocks) in msgs.iter_mut() {
        blocks.retain(|b| b["type"] != "thinking" && b["type"] != "redacted_thinking");
        for b in blocks.iter_mut() {
            if b["type"] == "image" {
                *b = json!({"type": "text", "text": format!("[image {}]", b["name"].as_str().unwrap_or(""))});
            }
            if let Some(o) = b.as_object_mut() {
                o.remove("origin");
            }
        }
    }
    let full = json!(msgs).to_string();
    if full.len() <= cap {
        return full;
    }
    let mut start = full.len() - cap;
    while !full.is_char_boundary(start) {
        start += 1;
    }
    format!("…(earlier part omitted){}", &full[start..])
}

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
        let digest = load_messages(app, &ctx.session)
            .map(|msgs| fork_digest(msgs, FORK_DIGEST_BYTES))
            .unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_keeps_the_end_without_images_or_thinking() {
        let msgs = vec![
            (
                "user".to_string(),
                vec![
                    json!({"type": "image", "name": "data/p.jpg", "b64": "A".repeat(50_000)}),
                    json!({"type": "text", "text": "start", "origin": "user"}),
                ],
            ),
            (
                "assistant".to_string(),
                vec![
                    json!({"type": "thinking", "thinking": "hmm", "signature": "S".repeat(9000)}),
                    json!({"type": "text", "text": "x".repeat(40_000)}),
                ],
            ),
            (
                "user".to_string(),
                vec![json!({"type": "text", "text": "the newest request"})],
            ),
        ];
        let d = fork_digest(msgs, 4000);
        assert!(d.len() < 4100);
        assert!(d.contains("the newest request"));
        assert!(!d.contains("AAAA") && !d.contains("SSSS"));
    }
}
