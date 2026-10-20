//! Summarizing old chat rows into one summary row.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::providers::call_provider;
use super::{load_config, RunCtx};

pub const COMPACT_PROMPT: &str = "You are a summarizer. Produce a concise but complete summary of the conversation so far: key decisions, changes made to the app and its data, current state, and open TODOs. This summary will REPLACE the prior messages as context, so preserve everything needed to continue the work.";

/// Rows of one session, with the (ts, ctr) ordering key compaction needs.
pub(crate) fn session_rows(app: &Arc<App>, session: &str) -> Result<Vec<Value>> {
    let eng = app.eng();
    let r = store::query(
        &eng.db,
        "SELECT mid, ts, ctr, role, content, device FROM uapp_chat
         WHERE COALESCE(session,'main') = ?1 ORDER BY ts, ctr, device, id",
        &[json!(session)],
    )?;
    Ok(r["rows"].as_array().cloned().unwrap_or_default())
}

/// A readable transcript of chat rows for the summarizer (newest kept when the
/// cap bites — the tail is the part that matters most).
pub(crate) fn transcript(rows: &[Value], cap: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in rows {
        let role = row[3].as_str().unwrap_or("");
        let content: Value =
            serde_json::from_str(row[4].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let body = match role {
            "user" | "system" => content["text"].as_str().unwrap_or("").to_string(),
            "summary" => format!(
                "(earlier summary) {}",
                content["text"].as_str().unwrap_or("")
            ),
            "assistant" => content["blocks"]
                .as_array()
                .map(|bs| {
                    bs.iter()
                        .filter_map(|b| match b["type"].as_str().unwrap_or("") {
                            "text" => b["text"].as_str().map(|t| t.to_string()),
                            "tool_use" => Some(format!(
                                "(called {} {})",
                                b["name"].as_str().unwrap_or(""),
                                clip(&b["input"].to_string(), 400)
                            )),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            "tool" => content["results"]
                .as_array()
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            format!(
                                "(result {}) {}",
                                r["name"].as_str().unwrap_or(""),
                                clip(r["content"].as_str().unwrap_or(""), 1500)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        if !body.trim().is_empty() {
            parts.push(format!("{role}: {body}"));
        }
    }
    let mut out = parts.join("\n\n");
    if out.len() > cap {
        let mut start = out.len() - cap;
        while !out.is_char_boundary(start) {
            start += 1;
        }
        out = format!("…(earlier turns omitted)\n{}", &out[start..]);
    }
    out
}

pub(crate) fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Summarize everything in a session except the last two user↔assistant
/// exchanges, then replace it with one `summary` row. The summary text is
/// produced HERE, before the op is emitted, so replay is deterministic.
pub fn compact_session(app: &Arc<App>, session: &str) -> Result<Value> {
    let cfg = load_config(app)?;
    let rows = session_rows(app, session)?;
    let user_at: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r[3].as_str() == Some("user"))
        .map(|(i, _)| i)
        .collect();
    // Keep the last two exchanges; nothing older means nothing to do.
    let keep_from = if user_at.len() >= 2 {
        user_at[user_at.len() - 2]
    } else {
        0
    };
    if keep_from == 0 {
        return Ok(
            json!({"ok": true, "kept": rows.len(), "summarized_bytes": 0,
                         "note": "nothing old enough to compact"}),
        );
    }
    let older = &rows[..keep_from];
    let summarized_bytes: usize = older
        .iter()
        .map(|r| r[4].as_str().map(|s| s.len()).unwrap_or(0))
        .sum();
    let text = transcript(older, 300_000);
    if text.trim().is_empty() {
        return Ok(
            json!({"ok": true, "kept": rows.len(), "summarized_bytes": 0,
                         "note": "nothing to summarize"}),
        );
    }
    let msgs = vec![(
        "user".to_string(),
        vec![json!({"type": "text", "text": text})],
    )];
    // Sentinel session: the summarizer's streamed deltas must not render as a
    // live draft bubble in the chat being compacted (no client shows this id).
    let blocks = call_provider(
        app,
        &format!("compact:{session}"),
        &cfg,
        COMPACT_PROMPT,
        &msgs,
        &[],
    )?;
    let summary: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str().map(|t| t.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if summary.is_empty() {
        bail!("the summarizer returned no text — nothing was compacted");
    }
    let boundary = &rows[keep_from - 1];
    rpc::local_op(
        app,
        "chat_compact",
        json!({
            "session": session,
            "upto_ts": boundary[1],
            "upto_ctr": boundary[2],
            // Tie-breaker matching the sort order: a concurrent row from another
            // device with the same (ts,ctr) that sorts AFTER the boundary is part
            // of the kept tail and must not be deleted.
            "upto_dev": boundary[5],
            "mid": format!("sum-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
            "summary": summary,
        }),
    )?;
    Ok(json!({"ok": true, "kept": rows.len() - keep_from, "summarized_bytes": summarized_bytes}))
}

/// Compact automatically when a run starts on an oversized history. Guarded
/// against looping: if the oldest row is ALREADY a summary, compacting again
/// would just re-summarize a summary — the outgoing hard trim handles it.
pub(crate) fn auto_compact(app: &Arc<App>, ctx: &RunCtx, cap: usize) {
    let Ok(rows) = session_rows(app, &ctx.session) else {
        return;
    };
    let bytes: usize = rows
        .iter()
        .map(|r| r[4].as_str().map(|s| s.len()).unwrap_or(0))
        .sum();
    if bytes <= cap {
        return;
    }
    if rows
        .first()
        .map(|r| r[3].as_str() == Some("summary"))
        .unwrap_or(false)
    {
        return;
    }
    app.notify(
        "ai",
        json!({"state": "running", "session": ctx.session,
                            "note": "summarizing the conversation so far…"}),
    );
    if let Err(e) = compact_session(app, &ctx.session) {
        eprintln!("uapp: auto-compaction failed: {e}");
    }
}
