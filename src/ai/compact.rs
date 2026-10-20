//! Summarizing old chat rows into one summary row.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;
use crate::store;

use super::providers::{Request, call_provider};
use super::{AiConfig, RunCtx, compact_bytes, load_config};

pub const COMPACT_PROMPT: &str = "You are a summarizer. Produce a concise but complete summary of the conversation so far: what the user asked for, key decisions, changes made to the app and its data, the current state, what was being done when the transcript ends, and open TODOs. This summary will REPLACE the prior messages as context, so preserve everything needed to continue the work, including exact file names, table names and values that matter.";

/// Introduces the verbatim request a summary carries (see compact_with).
const REQUEST_MARKER: &str = "The user's current request, word for word:\n";

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

/// Where a compaction's kept tail starts, given each row's role and size,
/// or None when there is nothing worth summarizing.
///
/// The tail keeps at most the last two user exchanges and at most
/// `keep_budget` bytes, and it can be empty: one long run with a single
/// request is compacted too. It starts at a user or assistant row, never at
/// tool results, so the kept rows still pair calls with their results.
pub(crate) fn plan_compaction(rows: &[(&str, usize)], keep_budget: usize) -> Option<usize> {
    let valid = |i: usize| i == rows.len() || (i > 0 && matches!(rows[i].0, "user" | "assistant"));
    let users: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].0 == "user").collect();
    let exchanges = if users.len() >= 2 {
        users[users.len() - 2]
    } else {
        0
    };
    let mut tail = 0usize;
    let mut by_budget = rows.len();
    for i in (1..=rows.len()).rev() {
        if i < rows.len() {
            tail += rows[i].1;
        }
        if tail > keep_budget {
            break;
        }
        if valid(i) {
            by_budget = i;
        }
    }
    let keep_from = (exchanges..=rows.len())
        .find(|&i| i >= by_budget && valid(i))
        .unwrap_or(rows.len());
    // Something besides an earlier summary (and snapshots) must go.
    rows[..keep_from]
        .iter()
        .any(|(role, _)| !matches!(*role, "summary" | "context"))
        .then_some(keep_from)
}

/// Bytes of history the summary would replace, counted from the newest
/// summary on: what auto-compaction compares with the budget.
pub(crate) fn bytes_since_summary(rows: &[(&str, usize)]) -> usize {
    let from = rows.iter().rposition(|(r, _)| *r == "summary").unwrap_or(0);
    rows[from..].iter().map(|(_, b)| b).sum()
}

/// Summarize the older part of a session (see [`plan_compaction`]) and
/// replace it with one `summary` row. The `chat.compact` RPC.
pub fn compact_session(app: &Arc<App>, session: &str) -> Result<Value> {
    let cfg = load_config(app)?;
    compact_with(app, session, &cfg, compact_bytes(&cfg), &|| false)
}

fn compact_with(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    cap: usize,
    stop: &dyn Fn() -> bool,
) -> Result<Value> {
    let rows = session_rows(app, session)?;
    let meta: Vec<(&str, usize)> = rows
        .iter()
        .map(|r| {
            (
                r[3].as_str().unwrap_or(""),
                r[4].as_str().map(str::len).unwrap_or(0),
            )
        })
        .collect();
    let counted = |rs: &[Value]| rs.iter().filter(|r| r[3] != "context").count();
    let Some(keep_from) = plan_compaction(&meta, cap / 4) else {
        return Ok(
            json!({"ok": true, "kept": counted(&rows), "summarized_bytes": 0,
                         "note": "nothing old enough to compact"}),
        );
    };
    let older = &rows[..keep_from];
    let summarized_bytes: usize = meta[..keep_from].iter().map(|(_, b)| b).sum();
    // The summarizer gets most of the provider's budget, newest part kept.
    let text = transcript(older, cap * 3 / 4);
    if text.trim().is_empty() {
        return Ok(
            json!({"ok": true, "kept": counted(&rows), "summarized_bytes": 0,
                         "note": "nothing to summarize"}),
        );
    }
    let msgs = vec![(
        "user".to_string(),
        vec![json!({"type": "text", "text": text})],
    )];
    // Sentinel session: the summarizer's streamed deltas must not render as a
    // live draft bubble in the chat being compacted (no client shows this id).
    let live_session = format!("compact:{session}");
    let reply = call_provider(
        app,
        cfg,
        &Request {
            session: &live_session,
            system: COMPACT_PROMPT,
            msgs: &msgs,
            tools: &[],
            stop,
        },
    )?;
    let mut summary: String = reply
        .blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str().map(|t| t.to_string()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if summary.is_empty() {
        bail!("the summarizer returned no text — nothing was compacted");
    }
    // When the request being worked on is itself summarized away (one long
    // run), it goes along word for word: the model must not lose the task.
    // An earlier summary already carrying it hands it on.
    if !rows[keep_from..].iter().any(|r| r[3] == "user") {
        let request = older
            .iter()
            .rev()
            .find(|r| r[3] == "user" || r[3] == "summary")
            .and_then(|r| {
                let content: Value =
                    serde_json::from_str(r[4].as_str().unwrap_or("{}")).unwrap_or(json!({}));
                let text = content["text"].as_str()?;
                if r[3] == "user" {
                    Some(text.to_string())
                } else {
                    text.split_once(REQUEST_MARKER)
                        .map(|(_, req)| req.to_string())
                }
            });
        if let Some(req) = request {
            summary.push_str(&format!("\n\n{REQUEST_MARKER}{req}"));
        }
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
    Ok(
        json!({"ok": true, "kept": counted(&rows[keep_from..]), "summarized_bytes": summarized_bytes}),
    )
}

/// Auto-compaction state for one run.
#[derive(Default)]
pub(crate) struct Compactor {
    /// History size when the last attempt failed. The next try waits until
    /// the history has grown by a quarter budget past it and a number of
    /// steps have passed that doubles with each failure, so a failing
    /// summarizer isn't called again on every step.
    failed_at: Option<usize>,
    failures: u32,
    wait_steps: u32,
}

impl Compactor {
    /// Should a history of this size (since the newest summary) be compacted
    /// on this step? Counts down the wait after a failure.
    pub(crate) fn due(&mut self, bytes: usize, cap: usize) -> bool {
        if bytes <= cap {
            return false;
        }
        if self.wait_steps > 0 {
            self.wait_steps -= 1;
            return false;
        }
        self.failed_at.is_none_or(|f| bytes >= f + cap / 4)
    }

    fn failed(&mut self, bytes: usize) {
        self.failures += 1;
        self.failed_at = Some(bytes);
        self.wait_steps = (3u32 << (self.failures - 1).min(3)).min(24);
    }

    /// Compact the run's session when the history since the newest summary
    /// is over budget. Runs again whenever it grows past the budget again.
    pub(crate) fn maybe_compact(
        &mut self,
        app: &Arc<App>,
        ctx: &RunCtx,
        cfg: &AiConfig,
        cap: usize,
    ) {
        let Ok(rows) = session_rows(app, &ctx.session) else {
            return;
        };
        let meta: Vec<(&str, usize)> = rows
            .iter()
            .map(|r| {
                (
                    r[3].as_str().unwrap_or(""),
                    r[4].as_str().map(str::len).unwrap_or(0),
                )
            })
            .collect();
        let bytes = bytes_since_summary(&meta);
        if !self.due(bytes, cap) {
            return;
        }
        app.notify(
            "ai",
            json!({"state": "running", "session": ctx.session,
                   "note": "summarizing the conversation so far…"}),
        );
        let stop = || ctx.stopped();
        match compact_with(app, &ctx.session, cfg, cap, &stop) {
            Ok(_) => *self = Compactor::default(),
            Err(e) => {
                eprintln!("uapp: auto-compaction failed: {e}");
                self.failed(bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_last_two_exchanges_when_small() {
        let rows = [
            ("user", 10),
            ("assistant", 10),
            ("user", 10),
            ("assistant", 10),
            ("user", 10),
            ("assistant", 10),
            ("user", 10),
            ("assistant", 10),
        ];
        assert_eq!(plan_compaction(&rows, 10_000), Some(4));
    }

    #[test]
    fn one_long_run_compacts_and_never_splits_a_round() {
        // One request, then tool rounds: assistant call, tool results, snapshot.
        let mut rows = vec![("user", 100), ("context", 500)];
        for _ in 0..20 {
            rows.extend([("assistant", 2000), ("tool", 8000), ("context", 300)]);
        }
        let keep = plan_compaction(&rows, 25_000).unwrap();
        assert!(keep > 1);
        assert_eq!(rows[keep].0, "assistant");
        let tail: usize = rows[keep..].iter().map(|r| r.1).sum();
        assert!(tail <= 25_000);
        // A tiny budget may keep nothing at all.
        assert_eq!(plan_compaction(&rows, 10), Some(rows.len()));
    }

    #[test]
    fn a_lone_summary_is_not_resummarized() {
        let rows = [("summary", 900_000), ("context", 100)];
        assert_eq!(plan_compaction(&rows, 10), None);
        assert_eq!(plan_compaction(&[], 10), None);
    }

    #[test]
    fn recompacts_after_a_summary_and_backs_off_after_failure() {
        let rows = [
            ("summary", 50_000),
            ("user", 10),
            ("assistant", 300_000),
            ("tool", 200_000),
        ];
        let bytes = bytes_since_summary(&rows);
        let cap = 400_000;
        let mut c = Compactor::default();
        assert!(
            c.due(bytes, cap),
            "a summary at the head no longer blocks compaction"
        );
        c.failed(bytes);
        let grown = bytes + cap / 4;
        let tries = (0..20).filter(|_| c.due(grown, cap)).count();
        assert_eq!(tries, 20 - 3, "waits three steps after the first failure");
        let mut c = Compactor::default();
        c.failed(bytes);
        assert!(
            !c.due(bytes + 1000, cap) && !c.due(bytes + 1000, cap) && !c.due(bytes + 1000, cap)
        );
        assert!(!c.due(bytes + 1000, cap), "and for real growth");
        c.failed(grown);
        assert_eq!(c.wait_steps, 6, "the wait doubles");
        assert!(!Compactor::default().due(cap, cap));
    }
}
