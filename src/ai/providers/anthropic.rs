//! Anthropic Messages API (also z.ai's Anthropic-compatible endpoint).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{
    open_stream, post, read_sse, retryable, tool_input, transient_kind, Live, Reply, Request, Usage,
};
use crate::ai::{default_max_tokens, AiConfig};

pub(crate) const DEFAULT_MODEL: &str = "claude-opus-5-5";
const ZAI_MODEL: &str = "glm-5.3";

/// Opt-in that lets a request say what to do with a thinking block whose
/// conversation changed since it was produced (see [`thinking_config`]).
const BETA_THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";
/// Server-side retry on another model when a safety classifier declines.
const BETA_FALLBACK: &str = "server-side-fallback-2026-07-01";

/// Models that take adaptive thinking and `output_config.effort`: Claude 4.6
/// and later. Older ones (4.5, Haiku 4.5, 3.x) reject that config, and an
/// unrecognized name (a proxy's alias) gets no thinking config at all.
pub(crate) fn adaptive_model(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("claude-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let family = parts.next().unwrap_or("");
    if !matches!(family, "opus" | "sonnet" | "haiku" | "fable" | "mythos") {
        return false;
    }
    let major: u32 = match parts.next().and_then(|p| p.parse().ok()) {
        Some(m) => m,
        None => return false,
    };
    // A dated snapshot id ("claude-sonnet-4-20250514") has no minor.
    let minor: u32 = parts
        .next()
        .filter(|p| p.len() <= 2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    major >= 5 || (major == 4 && minor >= 6)
}

/// Models whose refusals the API can retry on another model by itself
/// (`fallbacks: "default"`, Claude API only).
fn takes_default_fallback(model: &str) -> bool {
    matches!(
        model,
        "claude-opus-5-5" | "claude-opus-5" | "claude-fable-5-1" | "claude-sonnet-5-5"
    )
}

/// Thinking settings for a current Claude model. Thinking is always on for
/// them; `summarized` returns readable reasoning (and Opus 5.5's progress
/// notes between tool calls) instead of empty blocks, so the chat can show it.
///
/// `drop_block`: a thinking block is bound to the exact conversation before
/// it. History trimming, compaction and the hard ceiling all edit that
/// prefix at times, and without this the API would reject the replayed
/// block with a 400. With it the API drops the stale blocks and answers.
fn thinking_config() -> Value {
    json!({"type": "adaptive", "display": "summarized",
           "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
}

/// Anthropic `system` as one cached block. The prompt is frozen for a run
/// (the changing app snapshot travels in the messages), so the cached prefix
/// stays valid across the run's steps. `cache=false` (z.ai) returns the
/// plain string unchanged.
pub(crate) fn system_blocks(system: &str, cache: bool) -> Value {
    if !cache {
        return json!(system);
    }
    json!([{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}])
}

/// Mark the last tool with `cache_control`, so the whole (stable) tools array
/// caches with the system prefix.
pub(crate) fn cache_last_tool(tools: &[Value], cache: bool) -> Value {
    if !cache || tools.is_empty() {
        return json!(tools);
    }
    let mut out = tools.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    json!(out)
}

/// Cache the conversation prefix by marking the last content block of the last
/// message: everything before this turn's tail is reused on the next
/// iteration instead of re-billed.
pub(crate) fn cache_last_message(messages: &[Value], cache: bool) -> Value {
    if !cache || messages.is_empty() {
        return json!(messages);
    }
    let mut out = messages.to_vec();
    if let Some(content) = out.last_mut().and_then(|m| m["content"].as_array_mut()) {
        if let Some(block) = content.last_mut() {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    json!(out)
}

/// Neutral messages in Anthropic's shape.
///
/// Claude gets every thinking block back exactly as it came: Claude reads
/// earlier turns' reasoning, and stripping blocks that were sent on the
/// previous step would change the cached prefix every step. z.ai only gets
/// the final assistant turn's thinking (the one whose tool results follow,
/// so the model can resume mid-reasoning): replaying a whole run's reasoning
/// makes GLM re-read and extend every prior thought, so thinking snowballs
/// each iteration until it eats the entire budget.
pub(crate) fn to_messages(msgs: &[(String, Vec<Value>)], replay_all_thinking: bool) -> Vec<Value> {
    let last_assistant = msgs.iter().rposition(|(r, _)| r == "assistant");
    msgs.iter()
        .enumerate()
        .map(|(i, (role, blocks))| {
            let keep_thinking = replay_all_thinking || Some(i) == last_assistant;
            let content: Vec<Value> = blocks
                .iter()
                .filter_map(|b| match b["type"].as_str().unwrap_or("") {
                    "text" => Some(json!({"type": "text", "text": b["text"]})),
                    "image" => Some(json!({"type": "image", "source": {"type": "base64",
                        "media_type": b["media"], "data": b["b64"]}})),
                    "tool_use" => Some(json!({"type": "tool_use", "id": b["id"],
                        "name": b["name"], "input": b["input"]})),
                    "tool_result" => Some(json!({"type": "tool_result", "tool_use_id": b["id"],
                        "content": b["content"], "is_error": b["is_error"]})),
                    "thinking" if keep_thinking => Some(json!({"type": "thinking",
                        "thinking": b["thinking"], "signature": b["signature"]})),
                    "redacted_thinking" if keep_thinking => {
                        Some(json!({"type": "redacted_thinking", "data": b["data"]}))
                    }
                    _ => None,
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect()
}

/// A response as read off the wire, before deciding whether it's usable.
#[derive(Default)]
pub(crate) struct Parsed {
    pub blocks: Vec<Value>,
    pub stop_reason: String,
    pub refusal_category: Option<String>,
    pub usage: Usage,
    pub model: String,
}

fn read_usage(u: &Value, usage: &mut Usage) {
    let set = |k: &str, slot: &mut u64| {
        if let Some(n) = u[k].as_u64() {
            *slot = n;
        }
    };
    set("input_tokens", &mut usage.input);
    set("output_tokens", &mut usage.output);
    set("cache_read_input_tokens", &mut usage.cache_read);
    set("cache_creation_input_tokens", &mut usage.cache_write);
}

/// Keep the blocks we can store and replay. After a server-side fallback
/// mid-answer, the declined model's thinking and tool calls before the last
/// `fallback` marker must not be echoed back (its text may).
fn keep_blocks(raw: Vec<Value>) -> Vec<Value> {
    let boundary = raw.iter().rposition(|b| b["type"] == "fallback");
    raw.into_iter()
        .enumerate()
        .filter(|(i, b)| {
            let t = b["type"].as_str().unwrap_or("");
            let before_fallback = boundary.is_some_and(|f| *i < f);
            match t {
                "text" => b["text"].as_str().is_some_and(|s| !s.is_empty()),
                "thinking" | "redacted_thinking" | "tool_use" => !before_fallback,
                _ => false,
            }
        })
        .map(|(_, b)| b)
        .collect()
}

/// One content block from a complete message, in neutral form.
fn neutral_block(b: &Value) -> Value {
    match b["type"].as_str().unwrap_or("") {
        "text" => json!({"type": "text", "text": b["text"]}),
        "tool_use" => json!({"type": "tool_use", "id": b["id"], "name": b["name"],
                             "input": if b["input"].is_object() { b["input"].clone() } else { json!({}) }}),
        "thinking" => json!({"type": "thinking", "thinking": b["thinking"],
                             "signature": b["signature"]}),
        "redacted_thinking" => json!({"type": "redacted_thinking", "data": b["data"]}),
        other => json!({"type": other}),
    }
}

/// A complete (non-streamed) Anthropic message.
pub(crate) fn parse_message(v: &Value) -> Parsed {
    let mut p = Parsed {
        stop_reason: v["stop_reason"].as_str().unwrap_or("").to_string(),
        refusal_category: v["stop_details"]["category"].as_str().map(String::from),
        model: v["model"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    };
    read_usage(&v["usage"], &mut p.usage);
    let raw: Vec<Value> = v["content"]
        .as_array()
        .map(|a| a.iter().map(neutral_block).collect())
        .unwrap_or_default();
    p.blocks = keep_blocks(raw);
    p
}

/// Read an Anthropic SSE stream: assemble content blocks from deltas, pushing
/// partial text and thinking heartbeats to the shell as they arrive.
pub(crate) fn read_stream(
    reader: &mut dyn std::io::BufRead,
    stop: &dyn Fn() -> bool,
    live: &mut Live,
) -> Result<Parsed> {
    #[derive(Default)]
    struct Blk {
        typ: String,
        text: String,
        sig: String,
        data: String,
        id: String,
        name: String,
        json: String,
        start_input: Value,
    }
    let mut blks: std::collections::BTreeMap<u64, Blk> = Default::default();
    let mut p = Parsed::default();
    let mut thinking_open = false;
    read_sse(reader, stop, |data| {
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            return Ok(false);
        };
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                read_usage(&ev["message"]["usage"], &mut p.usage);
                p.model = ev["message"]["model"].as_str().unwrap_or("").to_string();
            }
            "content_block_start" => {
                let idx = ev["index"].as_u64().unwrap_or(0);
                let cb = &ev["content_block"];
                let typ = cb["type"].as_str().unwrap_or("").to_string();
                thinking_open = typ == "thinking" || typ == "redacted_thinking";
                if thinking_open {
                    live.thinking(0, true);
                }
                blks.insert(
                    idx,
                    Blk {
                        text: cb["text"]
                            .as_str()
                            .or(cb["thinking"].as_str())
                            .unwrap_or("")
                            .to_string(),
                        sig: cb["signature"].as_str().unwrap_or("").to_string(),
                        data: cb["data"].as_str().unwrap_or("").to_string(),
                        id: cb["id"].as_str().unwrap_or("").to_string(),
                        name: cb["name"].as_str().unwrap_or("").to_string(),
                        start_input: cb["input"].clone(),
                        typ,
                        ..Default::default()
                    },
                );
            }
            "content_block_delta" => {
                let idx = ev["index"].as_u64().unwrap_or(0);
                let d = &ev["delta"];
                let blk = blks.entry(idx).or_default();
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        blk.text.push_str(t);
                        live.text(t);
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        blk.text.push_str(t);
                        live.thinking(t.len(), false);
                    }
                    "signature_delta" => {
                        blk.sig.push_str(d["signature"].as_str().unwrap_or(""));
                        live.thinking(0, false);
                    }
                    "input_json_delta" => {
                        blk.json.push_str(d["partial_json"].as_str().unwrap_or(""))
                    }
                    _ => {}
                }
            }
            "content_block_stop" => thinking_open = false,
            "ping" if thinking_open => live.thinking(0, false),
            "message_delta" => {
                if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                    p.stop_reason = sr.to_string();
                }
                if let Some(c) = ev["delta"]["stop_details"]["category"].as_str() {
                    p.refusal_category = Some(c.to_string());
                }
                read_usage(&ev["usage"], &mut p.usage);
            }
            "error" => {
                let kind = ev["error"]["type"].as_str().unwrap_or("");
                let msg = format!(
                    "provider stream error: {}",
                    ev["error"]["message"].as_str().unwrap_or("unknown")
                );
                if transient_kind(kind) {
                    return Err(retryable(msg));
                }
                bail!(msg);
            }
            "message_stop" => return Ok(true),
            _ => {}
        }
        Ok(false)
    })?;
    live.flush();

    let mut raw = Vec::new();
    for (_, b) in blks {
        raw.push(match b.typ.as_str() {
            "text" => json!({"type": "text", "text": b.text}),
            "thinking" => json!({"type": "thinking", "thinking": b.text, "signature": b.sig}),
            "redacted_thinking" => json!({"type": "redacted_thinking", "data": b.data}),
            "tool_use" => {
                let input = if b.json.trim().is_empty() {
                    if b.start_input.is_object() {
                        b.start_input
                    } else {
                        json!({})
                    }
                } else {
                    tool_input(&b.json)
                };
                json!({"type": "tool_use", "id": b.id, "name": b.name, "input": input})
            }
            other => json!({"type": other}),
        });
    }
    p.blocks = keep_blocks(raw);
    Ok(p)
}

/// Anthropic Messages API client.
pub(crate) struct Anthropic;

impl super::Provider for Anthropic {
    fn call(&self, app: &Arc<App>, cfg: &AiConfig, req: &Request) -> Result<Reply> {
        let official = cfg.provider == "anthropic";
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| match cfg.provider.as_str() {
                "zai" => "https://api.z.ai/api/anthropic".into(),
                _ => "https://api.anthropic.com".into(),
            });
        let model = if !cfg.model.is_empty() {
            cfg.model.clone()
        } else if official {
            DEFAULT_MODEL.to_string()
        } else {
            ZAI_MODEL.to_string()
        };
        let messages = to_messages(req.msgs, official);
        // z.ai's Anthropic-compatible endpoint mis-streams GLM: tool-call blocks
        // never arrive as deltas, so a streamed turn looks like endless thinking
        // that eats the whole budget without ever acting (the ../chat app hit the
        // same thing and drives z.ai non-streaming for exactly this reason).
        // Non-streaming returns tool calls reliably, so zai defaults to it; real
        // Anthropic streams. Override per instance with config `ai.stream`.
        let stream = cfg.stream.unwrap_or(official);
        let adaptive = official && adaptive_model(&model);
        // Thinking models can exhaust an explicit token budget before emitting
        // any text or tool call. When that happens, retry once with a doubled
        // budget before giving up. zai defaults to NO cap (see AiConfig) — the
        // mere presence of max_tokens makes z.ai hand the whole budget to
        // thinking — so the retry only applies when a cap was requested.
        let mut budget = match cfg.provider.as_str() {
            "zai" => cfg.max_tokens, // None = omit the field entirely
            _ => Some(
                cfg.max_tokens
                    .unwrap_or_else(|| default_max_tokens(cfg, stream)),
            ),
        };
        // Prompt caching pays off across a long tool loop, but only real
        // Anthropic is guaranteed to honour `cache_control`; z.ai's compatible
        // endpoint is left alone.
        let cache = official;
        let mut betas: Vec<&str> = Vec::new();
        if adaptive {
            betas.push(BETA_THINKING_BINDING);
        }
        // Server-side fallback is a Claude API feature; a proxy at another
        // base URL may not know the parameter.
        let fallback = official && cfg.base_url.is_none() && takes_default_fallback(&model);
        if fallback {
            betas.push(BETA_FALLBACK);
        }
        let betas = betas.join(",");
        let auth = format!("Bearer {}", cfg.api_key);
        // z.ai accepts either header; real Anthropic uses x-api-key. Send both.
        let mut headers: Vec<(&str, &str)> = vec![
            ("x-api-key", &cfg.api_key),
            ("Authorization", &auth),
            ("anthropic-version", "2023-06-01"),
            ("content-type", "application/json"),
            (
                "accept",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            ),
        ];
        if !betas.is_empty() {
            headers.push(("anthropic-beta", &betas));
        }
        // Browser build: Anthropic requires this opt-in header before it
        // answers cross-origin requests from a page.
        if cfg!(target_arch = "wasm32") {
            headers.push(("anthropic-dangerous-direct-browser-access", "true"));
        }
        let url = format!("{base}/v1/messages");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut body = json!({
                "model": model,
                "system": system_blocks(req.system, cache),
                "messages": cache_last_message(&messages, cache),
                "tools": cache_last_tool(req.tools, cache),
            });
            if let Some(b) = budget {
                body["max_tokens"] = json!(b);
            }
            if stream {
                body["stream"] = json!(true);
            }
            if adaptive {
                body["thinking"] = thinking_config();
                body["output_config"] =
                    json!({"effort": cfg.effort.as_deref().unwrap_or("medium")});
            }
            if fallback {
                body["fallbacks"] = json!("default");
            }
            let mut notify = |v: Value| app.notify("ai", v);
            let mut live = Live::new(req.session, &mut notify);
            // Streaming: no overall timeout (long thinking runs are legitimate),
            // but a 180s idle-read timeout so a dead connection can't hang.
            // Non-streaming: nothing arrives until the model is done, so the read
            // timeout must cover a full long-thinking generation.
            let p = if stream {
                let resp = open_stream(&url, &headers, &body, 180)?;
                if resp.content_type.contains("event-stream") {
                    let mut reader = resp.reader;
                    read_stream(&mut *reader, req.stop, &mut live)?
                } else {
                    // The provider ignored the stream flag.
                    let text = resp
                        .into_string()
                        .map_err(|e| retryable(format!("provider request failed: {e}")))?;
                    let v: Value = serde_json::from_str(&text)
                        .map_err(|e| anyhow!("provider sent unreadable JSON: {e}"))?;
                    message_or_error(&v)?
                }
            } else {
                let (ctype, text) = post(&url, &headers, &body, 900, req.stop)?;
                if ctype.contains("event-stream") {
                    // Asked not to stream, streamed anyway.
                    let mut reader = std::io::Cursor::new(text.into_bytes());
                    read_stream(&mut reader, req.stop, &mut live)?
                } else {
                    let v: Value = serde_json::from_str(&text)
                        .map_err(|e| anyhow!("provider sent unreadable JSON: {e}"))?;
                    message_or_error(&v)?
                }
            };
            if p.stop_reason == "refusal" {
                bail!(
                    "the model declined this request{}. Rephrasing it may help, or pick \
                     another model in Settings",
                    p.refusal_category
                        .as_deref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default()
                );
            }
            if p.blocks
                .iter()
                .any(|b| b["type"] == "text" || b["type"] == "tool_use")
            {
                return Ok(Reply {
                    blocks: p.blocks,
                    usage: p.usage,
                    model: if p.model.is_empty() { model } else { p.model },
                });
            }
            // Nothing usable. Thinking-only + max_tokens = an explicit budget was
            // exhausted by reasoning; give it one more shot with double the room.
            if let (Some(old), "max_tokens", 1) = (budget, p.stop_reason.as_str(), attempt) {
                let new = old.saturating_mul(2).min(128_000);
                budget = Some(new);
                eprintln!(
                    "uapp: model spent the whole {old}-token budget thinking ({} chars); \
                     retrying with {new}",
                    live.think_chars
                );
                app.notify("ai", json!({"state": "running", "session": req.session,
                    "note": format!("model needed more room to think — retrying with {new} tokens")}));
                continue;
            }
            if let (Some(b), "max_tokens") = (budget, p.stop_reason.as_str()) {
                bail!(
                    "the model used the entire {b}-token budget on internal reasoning without \
                     producing an answer — raise max_tokens in Settings or simplify the request"
                );
            }
            bail!(
                "empty response from provider (stop_reason: {})",
                p.stop_reason
            );
        }
    }
}

/// A complete message, or the error object some endpoints return with 200.
fn message_or_error(v: &Value) -> Result<Parsed> {
    if v.get("error").is_some() {
        let msg = format!(
            "provider error: {}",
            v["error"]["message"].as_str().unwrap_or("unknown")
        );
        if transient_kind(v["error"]["type"].as_str().unwrap_or("")) {
            return Err(retryable(msg));
        }
        bail!(msg);
    }
    Ok(parse_message(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::CallError;

    fn sse(events: &[Value]) -> std::io::Cursor<Vec<u8>> {
        let mut s = String::new();
        for e in events {
            s.push_str(&format!(
                "event: {}\ndata: {}\n\n",
                e["type"].as_str().unwrap(),
                e
            ));
        }
        std::io::Cursor::new(s.into_bytes())
    }

    fn full_stream() -> Vec<Value> {
        vec![
            json!({"type": "message_start", "message": {"model": "claude-opus-5-5",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 900,
                          "cache_creation_input_tokens": 50, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hi"}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "get_schema", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "1}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
            json!({"type": "message_stop"}),
        ]
    }

    #[test]
    fn stream_assembles_blocks_usage_and_heartbeats() {
        let mut events = Vec::new();
        let mut notify = |v: Value| events.push(v);
        let mut live = Live::new("s", &mut notify);
        let p = read_stream(&mut sse(&full_stream()), &|| false, &mut live).unwrap();
        assert_eq!(p.stop_reason, "tool_use");
        assert_eq!(p.model, "claude-opus-5-5");
        assert_eq!(
            p.usage,
            Usage {
                input: 10,
                output: 42,
                cache_read: 900,
                cache_write: 50
            }
        );
        let types: Vec<&str> = p
            .blocks
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["thinking", "text", "tool_use"]);
        assert_eq!(p.blocks[0]["signature"], "sig");
        assert_eq!(p.blocks[2]["input"], json!({"a": 1}));
        // Hidden thinking still produced a heartbeat.
        assert!(events.iter().any(|e| e["state"] == "thinking"));
        assert!(events
            .iter()
            .any(|e| e["state"] == "streaming" && e["text"] == "Hi"));
    }

    #[test]
    fn truncated_stream_is_retryable() {
        let mut ev = full_stream();
        ev.truncate(7);
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let e = read_stream(&mut sse(&ev), &|| false, &mut live)
            .err()
            .unwrap();
        assert!(matches!(
            e.downcast_ref::<CallError>(),
            Some(CallError::Retryable { .. })
        ));
    }

    #[test]
    fn overload_mid_stream_is_retryable_but_invalid_request_is_not() {
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let over = vec![
            json!({"type": "message_start", "message": {}}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ];
        let e = read_stream(&mut sse(&over), &|| false, &mut live)
            .err()
            .unwrap();
        assert!(e.downcast_ref::<CallError>().is_some());
        let bad = vec![
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "no"}}),
        ];
        let e = read_stream(&mut sse(&bad), &|| false, &mut live)
            .err()
            .unwrap();
        assert!(e.downcast_ref::<CallError>().is_none());
    }

    #[test]
    fn fallback_boundary_drops_the_declined_models_tool_calls() {
        let v = json!({"stop_reason": "end_turn", "content": [
            {"type": "thinking", "thinking": "a", "signature": "s"},
            {"type": "text", "text": "partial"},
            {"type": "tool_use", "id": "x", "name": "n", "input": {}},
            {"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}},
            {"type": "text", "text": "rest"}
        ]});
        let p = parse_message(&v);
        let texts: Vec<&str> = p
            .blocks
            .iter()
            .map(|b| b["text"].as_str().unwrap_or("-"))
            .collect();
        assert_eq!(texts, ["partial", "rest"]);
    }

    #[test]
    fn adaptive_models() {
        for m in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-opus-4-8",
        ] {
            assert!(adaptive_model(m), "{m}");
        }
        for m in [
            "claude-haiku-4-5",
            "claude-sonnet-4-5",
            "claude-sonnet-4-20250514",
            "claude-3-7-sonnet-latest",
            "glm-5.3",
            "my-proxy-alias",
        ] {
            assert!(!adaptive_model(m), "{m}");
        }
    }

    #[test]
    fn thinking_replay_depends_on_provider() {
        let msgs = vec![
            (
                "user".to_string(),
                vec![json!({"type": "text", "text": "hi"})],
            ),
            (
                "assistant".to_string(),
                vec![
                    json!({"type": "thinking", "thinking": "a", "signature": "1"}),
                    json!({"type": "tool_use", "id": "t", "name": "x", "input": {}}),
                ],
            ),
            (
                "user".to_string(),
                vec![json!({"type": "tool_result", "id": "t", "content": "r"})],
            ),
            (
                "assistant".to_string(),
                vec![
                    json!({"type": "redacted_thinking", "data": "zz"}),
                    json!({"type": "tool_use", "id": "u", "name": "x", "input": {}}),
                ],
            ),
            (
                "user".to_string(),
                vec![json!({"type": "tool_result", "id": "u", "content": "r"})],
            ),
        ];
        let count = |m: &[Value]| {
            m.iter()
                .flat_map(|m| m["content"].as_array().unwrap())
                .filter(|b| b["type"] == "thinking" || b["type"] == "redacted_thinking")
                .count()
        };
        assert_eq!(count(&to_messages(&msgs, true)), 2);
        assert_eq!(count(&to_messages(&msgs, false)), 1);
    }
}
