//! Chat rows to provider-neutral messages, and trimming what goes out.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;

use super::OUTGOING_TOOL_BUDGET;

// ---- chat history -> provider-neutral messages ------------------------------

/// A neutral message: role + blocks.
/// blocks: {type:"text",text} | {type:"image",media,b64}
///       | {type:"tool_use",id,name,input} | {type:"tool_result",id,content,is_error}
pub(crate) fn build_messages(
    app: &Arc<App>,
    session: &str,
    cap: usize,
) -> Result<Vec<(String, Vec<Value>)>> {
    let chat = rpc::dispatch(app, "chat.list", json!({"session": session}))?;
    let rows = chat["rows"].as_array().cloned().unwrap_or_default();
    let mut msgs: Vec<(String, Vec<Value>)> = Vec::new();
    let mut open_tool_ids: Vec<(String, String)> = Vec::new(); // (id, name)

    for row in &rows {
        let role = row[4].as_str().unwrap_or("");
        let content: Value =
            serde_json::from_str(row[5].as_str().unwrap_or("{}")).unwrap_or(json!({}));
        let user = row[3].as_str().unwrap_or("");
        match role {
            "user" => {
                // Close any dangling tool calls (crashed run) before a new user turn.
                close_dangling(&mut msgs, &mut open_tool_ids);
                let mut blocks = Vec::new();
                let text = content["text"].as_str().unwrap_or("");
                let mut txt = if msgs.is_empty() || user.is_empty() {
                    text.to_string()
                } else {
                    format!("[{user}]: {text}")
                };
                if let Some(atts) = content["attachments"].as_array() {
                    for a in atts {
                        let name = a["name"].as_str().unwrap_or("");
                        if is_image(name) {
                            if let Some(b) = read_b64(app, name) {
                                blocks.push(
                                    json!({"type": "image", "media": media_type(name), "b64": b}),
                                );
                            }
                        }
                        txt.push_str(&format!("\n[attached file stored in archive at: {name}]"));
                    }
                }
                blocks.push(json!({"type": "text", "text": txt}));
                push_msg(&mut msgs, "user", blocks);
            }
            "assistant" => {
                close_dangling(&mut msgs, &mut open_tool_ids);
                let blocks: Vec<Value> = content["blocks"].as_array().cloned().unwrap_or_default();
                for b in &blocks {
                    if b["type"] == "tool_use" {
                        open_tool_ids.push((
                            b["id"].as_str().unwrap_or("").to_string(),
                            b["name"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                }
                if !blocks.is_empty() {
                    msgs.push(("assistant".into(), blocks));
                }
            }
            "tool" => {
                let results: Vec<Value> =
                    content["results"].as_array().cloned().unwrap_or_default();
                for r in &results {
                    let id = r["id"].as_str().unwrap_or("");
                    open_tool_ids.retain(|(i, _)| i != id);
                }
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({"type": "tool_result", "id": r["id"], "name": r["name"],
                               "content": r["content"], "is_error": r["is_error"].as_bool().unwrap_or(false)})
                    })
                    .collect();
                push_msg(&mut msgs, "user", blocks);
            }
            // A compaction summary stands in for everything before it.
            "summary" => {
                close_dangling(&mut msgs, &mut open_tool_ids);
                let text = format!(
                    "[Conversation summary — earlier messages were compacted]\n{}",
                    content["text"].as_str().unwrap_or("")
                );
                push_msg(
                    &mut msgs,
                    "user",
                    vec![json!({"type": "text", "text": text})],
                );
            }
            _ => {} // system/error rows are not sent to the model
        }
    }
    close_dangling(&mut msgs, &mut open_tool_ids);
    trim_tool_results(&mut msgs);
    hard_trim(&mut msgs, cap);
    Ok(msgs)
}

/// Non-destructive context trimming: walk tool_result blocks newest-first with
/// a byte budget and replace the ones that fall outside it (unless they're
/// small) with a note the model can act on. Only the OUTGOING copy changes —
/// the stored conversation keeps every byte.
pub(crate) fn trim_tool_results(msgs: &mut [(String, Vec<Value>)]) {
    let mut used = 0usize;
    for (_, blocks) in msgs.iter_mut().rev() {
        for b in blocks.iter_mut().rev() {
            if b["type"] != "tool_result" {
                continue;
            }
            let content = match b["content"].as_str() {
                Some(s) => s.to_string(),
                None => b["content"].to_string(),
            };
            if content.len() <= 1024 || used + content.len() <= OUTGOING_TOOL_BUDGET {
                used += content.len();
                continue;
            }
            let mut preview = content.clone();
            let mut end = 512.min(preview.len());
            while !preview.is_char_boundary(end) {
                end -= 1;
            }
            preview.truncate(end);
            b["content"] = json!(format!(
                "[older tool output trimmed — was {}KB. Re-run the tool with narrower \
                 parameters if needed. Preview: {}]",
                content.len().div_ceil(1024),
                preview
            ));
        }
    }
}

/// Last-resort ceiling: drop the OLDEST messages until the outgoing payload is
/// under the cap. Used when compaction can't help (the history is already one
/// summary plus a huge tail), so a run degrades instead of failing.
pub(crate) fn hard_trim(msgs: &mut Vec<(String, Vec<Value>)>, cap: usize) {
    // Sizes once, then a running total — re-serializing the whole history per
    // removed message made this quadratic on long conversations.
    let mut total: usize = msgs.iter().map(|(_, b)| json!(b).to_string().len()).sum();
    while msgs.len() > 2 && total > cap {
        total -= json!(msgs[0].1).to_string().len();
        msgs.remove(0);
        // The head must be a user turn with no orphaned tool_results: an
        // assistant-first payload and a tool_result without its tool_use are
        // both provider errors.
        while msgs.len() > 2 {
            if msgs[0].0 != "user" {
                total -= json!(msgs[0].1).to_string().len();
                msgs.remove(0);
                continue;
            }
            let before = json!(msgs[0].1).to_string().len();
            msgs[0].1.retain(|b| b["type"] != "tool_result");
            total = total.saturating_sub(before) + json!(msgs[0].1).to_string().len();
            if msgs[0].1.is_empty() {
                msgs.remove(0);
                continue;
            }
            break;
        }
    }
}

pub(crate) fn close_dangling(
    msgs: &mut Vec<(String, Vec<Value>)>,
    open: &mut Vec<(String, String)>,
) {
    if open.is_empty() {
        return;
    }
    let blocks: Vec<Value> = open
        .drain(..)
        .map(|(id, name)| {
            json!({"type": "tool_result", "id": id, "name": name,
                   "content": "(interrupted — no result recorded)", "is_error": true})
        })
        .collect();
    push_msg(msgs, "user", blocks);
}

pub(crate) fn push_msg(msgs: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some((last_role, last_blocks)) = msgs.last_mut() {
        if last_role == role {
            last_blocks.extend(blocks);
            return;
        }
    }
    msgs.push((role.into(), blocks));
}

pub(crate) fn is_image(name: &str) -> bool {
    let n = name.to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|e| n.ends_with(e))
}

pub(crate) fn media_type(name: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.ends_with(".png") {
        "image/png"
    } else if n.ends_with(".gif") {
        "image/gif"
    } else if n.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

pub(crate) fn read_b64(app: &Arc<App>, name: &str) -> Option<String> {
    rpc::dispatch(app, "files.read", json!({"name": name}))
        .ok()
        .and_then(|r| r["b64"].as_str().map(|s| s.to_string()))
}
