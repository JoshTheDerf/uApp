//! Chat rows to provider-neutral messages, and trimming what goes out.
//!
//! A neutral message is `(role, blocks)`, blocks being
//! `{type:"text",text,origin?}` | `{type:"image",name,media,b64?}` |
//! `{type:"tool_use",id,name,input}` | `{type:"tool_result",id,name,content,is_error}` |
//! `{type:"thinking",thinking,signature}` | `{type:"redacted_thinking",data}`.
//! `origin` marks where a user-side text came from: "user" (something a
//! person typed), "summary" (a compaction summary) or "context" (the app
//! snapshot the loop appends). Providers ignore it.
//!
//! Only the OUTGOING copy is ever trimmed; stored history keeps every byte.
//! The trimming is built so that what was sent on one step is, as far as
//! possible, sent byte-for-byte again on the next. Prompt caching is a prefix
//! match, and Claude's thinking blocks are bound to the exact conversation
//! before them, so rewriting old turns every step costs money and reasoning.

use anyhow::Result;
use base64::Engine as _;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::app::App;
use crate::rpc;

pub(crate) type Msg = (String, Vec<Value>);

/// How much history goes out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    /// Ceiling for the whole outgoing history, in bytes.
    pub cap: usize,
    /// Share of it that big tool outputs and file-write inputs may take.
    pub bulky: usize,
}

impl Budget {
    pub(crate) fn for_cap(cap: usize) -> Budget {
        Budget {
            cap,
            bulky: (cap / 4).min(super::OUTGOING_TOOL_BUDGET),
        }
    }
}

/// Images are only resent from this many of the latest user turns. Older
/// ones become a one-line note: one phone photo per message would otherwise
/// fill the context within a few turns.
pub(crate) const IMAGE_TURNS: usize = 3;
/// At most this many images per request, newest kept.
pub(crate) const MAX_IMAGES: usize = 8;
/// What one image counts as against the budget, whatever its file size: a
/// provider bills an image by its pixels (Claude scales it to ~1.15MP, about
/// 1600 tokens), never by its base64.
pub(crate) const IMAGE_COST: usize = 8 * 1024;
/// Largest image file sent to a model. Claude's limit is 5MB of base64,
/// which is 3.75MB of file.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;
/// Longest side, in pixels, a provider accepts.
pub(crate) const MAX_IMAGE_SIDE: u32 = 8000;

/// Build the outgoing messages for one session: rows to messages, older
/// images to notes, bulky payloads trimmed, the whole thing under the cap,
/// then image bytes read for the images that survived.
pub(crate) fn build_messages(app: &Arc<App>, session: &str, budget: Budget) -> Result<Vec<Msg>> {
    let mut msgs = load_messages(app, session)?;
    limit_images(&mut msgs, IMAGE_TURNS, MAX_IMAGES);
    trim_bulky(&mut msgs, budget.bulky);
    hard_trim(&mut msgs, budget.cap);
    resolve_images(app, &mut msgs);
    Ok(msgs)
}

/// The session's rows as messages, images still unread references.
pub(crate) fn load_messages(app: &Arc<App>, session: &str) -> Result<Vec<Msg>> {
    let chat = rpc::dispatch(
        app,
        "chat.list",
        json!({"session": session, "context": true}),
    )?;
    Ok(rows_to_messages(
        chat["rows"].as_array().map(Vec::as_slice).unwrap_or(&[]),
    ))
}

/// Chat rows (`chat.list` shape: mid, ts, device, user, role, content) to
/// messages.
pub(crate) fn rows_to_messages(rows: &[Value]) -> Vec<Msg> {
    let mut msgs: Vec<Msg> = Vec::new();
    let mut open_tool_ids: Vec<(String, String)> = Vec::new(); // (id, name)

    for row in rows {
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
                            blocks.push(json!({"type": "image", "name": name,
                                               "media": media_type(name)}));
                        }
                        txt.push_str(&format!("\n[attached file stored in archive at: {name}]"));
                    }
                }
                blocks.push(json!({"type": "text", "text": txt, "origin": "user"}));
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
                // Only results whose call is still in the history: compaction
                // can cut between a call and its results, and a result with no
                // call before it is a provider error.
                let blocks: Vec<Value> = content["results"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .filter(|r| {
                        let id = r["id"].as_str().unwrap_or("");
                        let open = open_tool_ids.iter().any(|(i, _)| i == id);
                        open_tool_ids.retain(|(i, _)| i != id);
                        open
                    })
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
                    vec![json!({"type": "text", "text": text, "origin": "summary"})],
                );
            }
            // The app's files and schema as of this point (see run.rs).
            "context" => {
                close_dangling(&mut msgs, &mut open_tool_ids);
                let text = content["text"].as_str().unwrap_or("");
                if !text.is_empty() {
                    push_msg(
                        &mut msgs,
                        "user",
                        vec![json!({"type": "text", "text": text, "origin": "context"})],
                    );
                }
            }
            _ => {} // system/error rows are not sent to the model
        }
    }
    close_dangling(&mut msgs, &mut open_tool_ids);
    normalize(&mut msgs);
    msgs
}

/// Bytes a block takes in the outgoing request, images at their fixed cost.
pub(crate) fn block_bytes(b: &Value) -> usize {
    if b["type"] == "image" {
        return IMAGE_COST;
    }
    b.to_string().len()
}

fn msg_bytes(m: &Msg) -> usize {
    16 + m.1.iter().map(block_bytes).sum::<usize>()
}

pub(crate) fn total_bytes(msgs: &[Msg]) -> usize {
    msgs.iter().map(msg_bytes).sum()
}

fn has_origin(m: &Msg, origin: &str) -> bool {
    m.1.iter().any(|b| b["origin"] == origin)
}

/// Keep images only in the newest `turns` user turns (at most `max` of them,
/// newest first); older ones become a note naming the file.
pub(crate) fn limit_images(msgs: &mut [Msg], turns: usize, max: usize) {
    let mut user_turns = 0usize;
    let mut kept = 0usize;
    for (role, blocks) in msgs.iter_mut().rev() {
        if role != "user" {
            continue;
        }
        let typed = blocks.iter().any(|b| b["origin"] == "user");
        if typed {
            user_turns += 1;
        }
        for b in blocks.iter_mut().rev() {
            if b["type"] != "image" {
                continue;
            }
            if user_turns <= turns && kept < max {
                kept += 1;
                continue;
            }
            *b = json!({"type": "text", "text": format!(
                "[image {} was attached here; it is no longer sent to save room. \
                 Ask the user to attach it again if you need to see it]",
                b["name"].as_str().unwrap_or(""))});
        }
    }
}

/// Read the bytes of every image still in the messages. One that can't be
/// read, is too big or too large in pixels for a provider becomes a note: a
/// request carrying it would fail every time.
pub(crate) fn resolve_images(app: &Arc<App>, msgs: &mut [Msg]) {
    for (_, blocks) in msgs.iter_mut() {
        for b in blocks.iter_mut() {
            if b["type"] != "image" || b["b64"].is_string() {
                continue;
            }
            let name = b["name"].as_str().unwrap_or("").to_string();
            let bytes = {
                let eng = app.eng();
                crate::store::sqlar_read(&eng.db, &name).ok().flatten()
            };
            *b = match bytes {
                None => {
                    json!({"type": "text", "text": format!("[image {name} is no longer in the archive]")})
                }
                Some(bytes) => match image_problem(&bytes) {
                    Some(why) => json!({"type": "text", "text": format!(
                        "[image {name} is not shown to you: {why}]")}),
                    None => json!({"type": "image", "name": name, "media": b["media"],
                        "b64": base64::engine::general_purpose::STANDARD.encode(&bytes)}),
                },
            };
        }
    }
}

/// Why a provider would refuse this image, if it would.
pub fn image_problem(bytes: &[u8]) -> Option<String> {
    if let Some(why) = image_too_big(bytes.len()) {
        return Some(why);
    }
    match image_dims(bytes) {
        Some((w, h)) if w > MAX_IMAGE_SIDE || h > MAX_IMAGE_SIDE => Some(format!(
            "it is {w}x{h} pixels and the limit is {MAX_IMAGE_SIDE} on each side"
        )),
        _ => None,
    }
}

/// The size half of [`image_problem`], for when only the length is known.
pub fn image_too_big(len: usize) -> Option<String> {
    (len > MAX_IMAGE_BYTES).then(|| {
        format!(
            "it is {:.1}MB and the limit is {:.2}MB",
            len as f64 / 1e6,
            MAX_IMAGE_BYTES as f64 / 1e6
        )
    })
}

/// Width and height from a PNG, GIF, JPEG or WebP header.
pub(crate) fn image_dims(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let le24 = |i: usize| {
        Some(u32::from_le_bytes([
            *b.get(i)?,
            *b.get(i + 1)?,
            *b.get(i + 2)?,
            0,
        ]))
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
        return Some((w, h));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let (b0, b1, b2, b3) = (
                    *b.get(21)? as u32,
                    *b.get(22)? as u32,
                    *b.get(23)? as u32,
                    *b.get(24)? as u32,
                );
                Some((
                    1 + (b0 | (b1 & 0x3f) << 8),
                    1 + (b1 >> 6 | b2 << 2 | (b3 & 0xf) << 10),
                ))
            }
            b"VP8X" => Some((1 + le24(24)?, 1 + le24(27)?)),
            _ => None,
        };
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xff {
                return None;
            }
            let marker = b[i + 1];
            if marker == 0xff {
                i += 1; // fill byte
                continue;
            }
            let sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
            if sof {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + be16(i + 2)? as usize;
        }
    }
    None
}

/// A payload that can be trimmed out of an old turn: (message, block, bytes).
fn bulky_items(msgs: &[Msg]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (mi, (_, blocks)) in msgs.iter().enumerate() {
        for (bi, b) in blocks.iter().enumerate() {
            let size = match b["type"].as_str().unwrap_or("") {
                "tool_result" => match b["content"].as_str() {
                    Some(s) => s.len(),
                    None => b["content"].to_string().len(),
                },
                "tool_use" => match b["name"].as_str().unwrap_or("") {
                    "write_file" => b["input"]["content"].as_str().map_or(0, str::len),
                    "edit_file" => {
                        b["input"]["old_string"].as_str().map_or(0, str::len)
                            + b["input"]["new_string"].as_str().map_or(0, str::len)
                    }
                    _ => 0,
                },
                _ => 0,
            };
            if size > 1024 {
                out.push((mi, bi, size));
            }
        }
    }
    out
}

/// Trim old tool outputs and old file-write inputs once together they pass
/// `budget`, oldest first, replacing each with a note the model can act on.
///
/// How much to trim moves in steps of half the budget rather than with
/// every new byte: between steps the same items stay trimmed and the rest
/// stay whole, so the outgoing history only grows at the end and the cached
/// prefix (and Claude's thinking) stay valid. A step trims a batch at once.
pub(crate) fn trim_bulky(msgs: &mut [Msg], budget: usize) {
    let items = bulky_items(msgs);
    let total: usize = items.iter().map(|i| i.2).sum();
    if total <= budget {
        return;
    }
    let quantum = (budget / 2).max(1);
    let target = (total - budget).div_ceil(quantum) * quantum;
    let mut trimmed = 0usize;
    for (mi, bi, size) in items {
        if trimmed >= target {
            break;
        }
        trimmed += size;
        let b = &mut msgs[mi].1[bi];
        if b["type"] == "tool_result" {
            let content = match b["content"].as_str() {
                Some(s) => s.to_string(),
                None => b["content"].to_string(),
            };
            b["content"] = json!(format!(
                "[older tool output trimmed — was {}KB. Re-run the tool with narrower \
                 parameters if needed. Preview: {}]",
                size.div_ceil(1024),
                clip_at(&content, 512)
            ));
        } else if b["name"] == "write_file" {
            b["input"]["content"] = json!(format!(
                "[trimmed from this conversation: {}KB was written. read_file shows the \
                 current text]",
                size.div_ceil(1024)
            ));
        } else {
            for key in ["old_string", "new_string"] {
                if let Some(s) = b["input"][key].as_str()
                    && s.len() > 256
                {
                    let short = format!(
                        "{}…[trimmed, was {}KB]",
                        clip_at(s, 200),
                        s.len().div_ceil(1024)
                    );
                    b["input"][key] = json!(short);
                }
            }
        }
    }
}

fn clip_at(s: &str, n: usize) -> &str {
    let mut end = n.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Last-resort ceiling: drop whole turns until the outgoing payload fits.
/// Compaction normally keeps the history small; this handles what it can't
/// (a summary plus a huge tail, a failed summarizer, one very long run).
///
/// The latest message a person typed is never dropped (or, if compaction
/// folded it into the summary, the summary). Drops go, in order: turns
/// between the summary and that message, then the summary, then the oldest
/// complete tool rounds after it, keeping the newest round. The result
/// always starts with a user turn, alternates roles, and pairs every tool
/// result with its call.
pub(crate) fn hard_trim(msgs: &mut Vec<Msg>, cap: usize) {
    normalize(msgs);
    let mut total = total_bytes(msgs);
    if total <= cap || msgs.is_empty() {
        return;
    }
    let Some(mut keep) = msgs
        .iter()
        .rposition(|m| has_origin(m, "user"))
        .or_else(|| msgs.iter().rposition(|m| has_origin(m, "summary")))
    else {
        return;
    };
    let summary_head = has_origin(&msgs[0], "summary") && keep > 0;
    let start = usize::from(summary_head);
    while total > cap && start < keep {
        total -= msg_bytes(&msgs[start]);
        msgs.remove(start);
        keep -= 1;
    }
    if total > cap && summary_head && keep > 0 {
        total -= msg_bytes(&msgs[0]);
        msgs.remove(0);
        keep -= 1;
    }
    let i = keep + 1;
    while total > cap
        && i + 2 < msgs.len()
        && msgs[i].0 == "assistant"
        && msgs[i + 1].0 == "user"
        && !has_origin(&msgs[i + 1], "user")
    {
        total -= msg_bytes(&msgs[i]) + msg_bytes(&msgs[i + 1]);
        msgs.drain(i..i + 2);
    }
    normalize(msgs);
}

/// Repair what removing turns can break: a leading assistant turn, two
/// turns of the same role in a row, a tool result whose call is gone.
pub(crate) fn normalize(msgs: &mut Vec<Msg>) {
    loop {
        while msgs.first().is_some_and(|m| m.0 != "user") {
            msgs.remove(0);
        }
        let mut changed = false;
        let mut k = 1;
        while k < msgs.len() {
            if msgs[k].0 == msgs[k - 1].0 {
                let blocks = msgs.remove(k).1;
                msgs[k - 1].1.extend(blocks);
                changed = true;
                continue;
            }
            k += 1;
        }
        for k in 0..msgs.len() {
            if msgs[k].0 != "user" {
                continue;
            }
            let calls: Vec<String> = if k > 0 {
                msgs[k - 1]
                    .1
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .filter_map(|b| b["id"].as_str().map(String::from))
                    .collect()
            } else {
                Vec::new()
            };
            let before = msgs[k].1.len();
            msgs[k]
                .1
                .retain(|b| b["type"] != "tool_result" || calls.iter().any(|c| b["id"] == **c));
            changed |= msgs[k].1.len() != before;
            // Results go first in a user turn (Claude requires it); merging
            // can have put text ahead of them.
            msgs[k].1.sort_by_key(|b| b["type"] != "tool_result");
        }
        let before = msgs.len();
        msgs.retain(|m| !m.1.is_empty());
        changed |= msgs.len() != before;
        if !changed {
            break;
        }
    }
}

pub(crate) fn close_dangling(msgs: &mut Vec<Msg>, open: &mut Vec<(String, String)>) {
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

pub(crate) fn push_msg(msgs: &mut Vec<Msg>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some((last_role, last_blocks)) = msgs.last_mut()
        && last_role == role
    {
        last_blocks.extend(blocks);
        return;
    }
    msgs.push((role.into(), blocks));
}

pub fn is_image(name: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(role: &str, content: Value) -> Value {
        json!(["m", 0, "dev", "", role, content.to_string()])
    }
    fn user(t: &str) -> Value {
        row("user", json!({"text": t}))
    }
    fn call(id: &str, name: &str, input: Value) -> Value {
        row(
            "assistant",
            json!({"blocks": [{"type": "tool_use", "id": id, "name": name, "input": input}]}),
        )
    }
    fn result(id: &str, content: &str) -> Value {
        row(
            "tool",
            json!({"results": [{"id": id, "name": "x", "content": content}]}),
        )
    }
    fn answer(t: &str) -> Value {
        row(
            "assistant",
            json!({"blocks": [{"type": "text", "text": t}]}),
        )
    }

    /// Every invariant a provider insists on.
    fn assert_valid(msgs: &[Msg]) {
        assert!(msgs.is_empty() || msgs[0].0 == "user", "starts with user");
        for w in msgs.windows(2) {
            assert_ne!(w[0].0, w[1].0, "roles alternate");
        }
        for (k, (role, blocks)) in msgs.iter().enumerate() {
            if role == "user" {
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    assert!(
                        k > 0
                            && msgs[k - 1]
                                .1
                                .iter()
                                .any(|c| c["type"] == "tool_use" && c["id"] == b["id"]),
                        "result {} has its call",
                        b["id"]
                    );
                }
            } else {
                for c in blocks.iter().filter(|b| b["type"] == "tool_use") {
                    assert!(
                        msgs.get(k + 1).is_some_and(|n| n
                            .1
                            .iter()
                            .any(|b| b["type"] == "tool_result" && b["id"] == c["id"])),
                        "call {} has its result",
                        c["id"]
                    );
                }
            }
        }
    }

    #[test]
    fn rows_become_valid_messages() {
        let rows = vec![
            row("summary", json!({"text": "earlier"})),
            result("gone", "orphaned by compaction"),
            answer("hello"),
            row(
                "user",
                json!({"text": "look", "attachments": [{"name": "data/a.png"}]}),
            ),
            row("context", json!({"text": "[app state]"})),
            call("c1", "sql_query", json!({})),
            // crashed before its result was written
            user("next"),
        ];
        let msgs = rows_to_messages(&rows);
        assert_valid(&msgs);
        assert!(!msgs.iter().flat_map(|m| &m.1).any(|b| b["id"] == "gone"));
        let flat: Vec<&Value> = msgs.iter().flat_map(|m| &m.1).collect();
        assert!(
            flat.iter()
                .any(|b| b["type"] == "image" && b["name"] == "data/a.png")
        );
        assert!(flat.iter().any(|b| b["origin"] == "context"));
        assert!(
            flat.iter()
                .any(|b| b["content"] == "(interrupted — no result recorded)")
        );
    }

    #[test]
    fn only_recent_turns_keep_their_images() {
        let mut rows = Vec::new();
        for i in 0..5 {
            rows.push(row("user", json!({"text": format!("u{i}"), "attachments": [{"name": format!("data/{i}.jpg")}]})));
            rows.push(answer("ok"));
        }
        let mut msgs = rows_to_messages(&rows);
        limit_images(&mut msgs, 3, 8);
        let kept: Vec<&str> = msgs
            .iter()
            .flat_map(|m| &m.1)
            .filter(|b| b["type"] == "image")
            .filter_map(|b| b["name"].as_str())
            .collect();
        assert_eq!(kept, ["data/2.jpg", "data/3.jpg", "data/4.jpg"]);
        // Images count at a fixed cost, not their size.
        assert_eq!(
            block_bytes(&json!({"type": "image", "b64": "x".repeat(1 << 20)})),
            IMAGE_COST
        );
    }

    /// A run of `n` steps, each with a tool result of `size` bytes.
    fn long_run(n: usize, size: usize) -> Vec<Value> {
        let mut rows = vec![user("build it")];
        for i in 0..n {
            rows.push(call(&format!("c{i}"), "read_file", json!({"name": "x"})));
            rows.push(result(&format!("c{i}"), &"y".repeat(size)));
        }
        rows
    }

    #[test]
    fn trimming_only_changes_at_quantum_steps() {
        let budget = 20_000;
        let mut prev: Option<Vec<Msg>> = None;
        let mut changes = 0;
        for n in 1..60 {
            let mut msgs = rows_to_messages(&long_run(n, 3000));
            trim_bulky(&mut msgs, budget);
            if let Some(p) = &prev {
                // The previous request must be a prefix of this one, except
                // on the steps where another batch was trimmed.
                if msgs[..p.len()] != p[..] {
                    changes += 1;
                }
                // Trimmed stays trimmed.
                for (a, b) in p.iter().zip(&msgs) {
                    for (x, y) in a.1.iter().zip(&b.1) {
                        if x["content"]
                            .as_str()
                            .is_some_and(|c| c.starts_with("[older"))
                        {
                            assert_eq!(x, y);
                        }
                    }
                }
            }
            let kept: usize = bulky_items(&msgs).iter().map(|i| i.2).sum();
            assert!(kept <= budget + budget / 2 + 3000, "{n}: kept {kept}");
            prev = Some(msgs);
        }
        // 59 steps of 3KB past a 20KB budget: a change every ~10KB, not every step.
        assert!(changes > 0 && changes < 20, "{changes} changes");
    }

    #[test]
    fn old_file_writes_are_trimmed_too() {
        let mut rows = vec![user("go")];
        for i in 0..6 {
            rows.push(call(
                &format!("w{i}"),
                "write_file",
                json!({"name": "a.js", "content": "z".repeat(10_000)}),
            ));
            rows.push(result(&format!("w{i}"), "{\"ok\":true}"));
        }
        let mut msgs = rows_to_messages(&rows);
        trim_bulky(&mut msgs, 20_000);
        let first = &msgs[1].1[0];
        assert!(
            first["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimmed")
        );
        let last = &msgs[msgs.len() - 2].1[0];
        assert_eq!(last["input"]["content"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn hard_trim_keeps_the_latest_user_message_and_valid_order() {
        // Random conversations: user turns, tool rounds, summaries, context.
        let mut rng = fastrand::Rng::with_seed(7);
        for case in 0..300 {
            let mut rows = Vec::new();
            if rng.bool() {
                rows.push(row(
                    "summary",
                    json!({"text": "s".repeat(rng.usize(10..4000))}),
                ));
            }
            let mut last_user = String::new();
            for t in 0..rng.usize(1..6) {
                last_user = format!("request {case}-{t}");
                rows.push(user(&last_user));
                for s in 0..rng.usize(0..8) {
                    let id = format!("c{t}-{s}");
                    rows.push(call(
                        &id,
                        "sql_query",
                        json!({"sql": "x".repeat(rng.usize(10..3000))}),
                    ));
                    rows.push(result(&id, &"r".repeat(rng.usize(10..6000))));
                    if rng.bool() {
                        rows.push(row("context", json!({"text": "ctx"})));
                    }
                }
                if rng.bool() {
                    rows.push(answer("done"));
                }
            }
            let cap = rng.usize(500..40_000);
            let mut msgs = rows_to_messages(&rows);
            hard_trim(&mut msgs, cap);
            assert_valid(&msgs);
            let texts: Vec<&str> = msgs
                .iter()
                .flat_map(|m| &m.1)
                .filter_map(|b| b["text"].as_str())
                .collect();
            assert!(
                texts.contains(&last_user.as_str()),
                "case {case}: lost the latest request"
            );
        }
    }

    #[test]
    fn hard_trim_drops_in_order() {
        let mut rows = vec![row("summary", json!({"text": "S"})), answer("noted")];
        rows.push(user("older request"));
        for i in 0..3 {
            rows.push(call(&format!("o{i}"), "read_file", json!({})));
            rows.push(result(&format!("o{i}"), &"y".repeat(2000)));
        }
        rows.push(answer("done"));
        rows.push(user("latest"));
        for i in 0..4 {
            rows.push(call(&format!("n{i}"), "read_file", json!({})));
            rows.push(result(&format!("n{i}"), &"y".repeat(2000)));
        }
        let ids = |msgs: &[Msg]| -> Vec<String> {
            msgs.iter()
                .flat_map(|m| &m.1)
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str().map(String::from))
                .collect()
        };
        let mut msgs = rows_to_messages(&rows);
        let full = total_bytes(&msgs);
        // Just over: the turns between the summary and the latest request go first.
        hard_trim(&mut msgs, full - 1000);
        assert_valid(&msgs);
        assert!(has_origin(&msgs[0], "summary"));
        assert!(!ids(&msgs).contains(&"o0".to_string()));
        assert!(ids(&msgs).contains(&"n0".to_string()));
        // Much less: the summary goes, then the oldest rounds of this run, but
        // the newest round stays.
        let mut msgs = rows_to_messages(&rows);
        hard_trim(&mut msgs, 3000);
        assert_valid(&msgs);
        assert!(has_origin(&msgs[0], "user") && !has_origin(&msgs[0], "summary"));
        assert_eq!(ids(&msgs), ["n3"]);
    }

    #[test]
    fn image_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(9000u32.to_be_bytes());
        png.extend(20u32.to_be_bytes());
        assert_eq!(image_dims(&png), Some((9000, 20)));
        assert!(image_problem(&png).unwrap().contains("9000x20"));
        let gif = b"GIF89a\x40\x01\xf0\x00".to_vec();
        assert_eq!(image_dims(&gif), Some((320, 240)));
        // JPEG: SOI, an APP0 segment, then SOF0 with height 480, width 640.
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00];
        jpg.extend([0xff, 0xc0, 0x00, 0x11, 0x08, 0x01, 0xe0, 0x02, 0x80, 0x03]);
        assert_eq!(image_dims(&jpg), Some((640, 480)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0u8; 8]);
        webp.extend([0x3f, 0x01, 0x00, 0xdf, 0x00, 0x00]); // 320 x 224
        assert_eq!(image_dims(&webp), Some((320, 224)));
        assert!(image_problem(&vec![0u8; MAX_IMAGE_BYTES + 1]).is_some());
        assert!(image_problem(&gif).is_none());
    }
}
