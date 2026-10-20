//! Token usage, as recorded on assistant rows (see run.rs), summed for the
//! `ai.usage` RPC.

use anyhow::Result;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::app::App;
use crate::store;

const FIELDS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
];

fn add(total: &mut Map<String, Value>, usage: &Value) {
    for f in FIELDS {
        let n = total.get(f).and_then(Value::as_u64).unwrap_or(0) + usage[f].as_u64().unwrap_or(0);
        total.insert(f.to_string(), json!(n));
    }
    let steps = total.get("steps").and_then(Value::as_u64).unwrap_or(0) + 1;
    total.insert("steps".to_string(), json!(steps));
}

/// `{session}`: that chat's steps with their usage, its total, and the
/// totals of the sub-agents it started. No session: a total per session and
/// overall.
pub fn usage_report(app: &Arc<App>, session: Option<&str>) -> Result<Value> {
    let rows = {
        let eng = app.eng();
        let r = store::query(
            &eng.db,
            "SELECT c.mid, c.ts, COALESCE(c.session,'main'), c.content, s.parent
             FROM uapp_chat c LEFT JOIN uapp_sessions s ON s.id = c.session
             WHERE c.role = 'assistant' ORDER BY c.ts, c.id",
            &[],
        )?;
        r["rows"].as_array().cloned().unwrap_or_default()
    };
    let mut total = Map::new();
    let mut by_session: Map<String, Value> = Map::new();
    let mut steps = Vec::new();
    for row in rows {
        let content: Value =
            serde_json::from_str(row[3].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let usage = &content["usage"];
        if !usage.is_object() {
            continue;
        }
        let sid = row[2].as_str().unwrap_or("main");
        let parent = row[4].as_str();
        match session {
            Some(s) if sid == s => {
                steps.push(
                    json!({"mid": row[0], "ts": row[1], "model": content["model"],
                                  "usage": usage}),
                );
                add(&mut total, usage);
            }
            // A sub-agent of this chat.
            Some(s) if parent == Some(s) => {
                let entry = by_session
                    .entry(sid.to_string())
                    .or_insert_with(|| json!({}));
                add(entry.as_object_mut().unwrap(), usage);
            }
            Some(_) => {}
            None => {
                let entry = by_session
                    .entry(sid.to_string())
                    .or_insert_with(|| json!({}));
                add(entry.as_object_mut().unwrap(), usage);
                add(&mut total, usage);
            }
        }
    }
    Ok(match session {
        Some(s) => json!({"session": s, "steps": steps, "total": total, "agents": by_session}),
        None => json!({"sessions": by_session, "total": total}),
    })
}
