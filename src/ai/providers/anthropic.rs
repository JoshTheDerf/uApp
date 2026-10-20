//! Anthropic Messages API (also z.ai's Anthropic-compatible endpoint).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{provider_error, tool_input};
use crate::ai::{default_max_tokens, AiConfig};

/// Anthropic `system` as cacheable blocks: the big stable prompt (instructions
/// + tool docs) is one cached block, the volatile tail (current files/schema,
/// which change as work proceeds) a second uncached one — so the cached prefix
/// stays valid across a turn's many iterations. `cache=false` (z.ai) returns
/// the plain string unchanged.
pub(crate) fn system_blocks(system: &str, cache: bool) -> Value {
    if !cache {
        return json!(system);
    }
    let split = system
        .find("\n\nCURRENT APP FILES:")
        .unwrap_or(system.len());
    let (stable, volatile) = system.split_at(split);
    let mut blocks = vec![json!({
        "type": "text", "text": stable,
        "cache_control": {"type": "ephemeral"},
    })];
    if !volatile.is_empty() {
        blocks.push(json!({"type": "text", "text": volatile}));
    }
    json!(blocks)
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

/// Extract neutral blocks from a complete (non-streamed) Anthropic message.
/// Lenient — returns an empty vec when there is nothing usable (e.g. the whole
/// budget went to reasoning); the caller decides whether that's worth a
/// budget-bump retry or an error.
pub(crate) fn parse_anthropic_message(resp: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for b in resp["content"].as_array().cloned().unwrap_or_default() {
        match b["type"].as_str().unwrap_or("") {
            "text" => out.push(json!({"type": "text", "text": b["text"]})),
            "tool_use" => out.push(json!({"type": "tool_use", "id": b["id"],
                "name": b["name"], "input": b["input"]})),
            "thinking" => out.push(json!({"type": "thinking",
                "thinking": b["thinking"], "signature": b["signature"]})),
            _ => {}
        }
    }
    if !out
        .iter()
        .any(|b| b["type"] == "text" || b["type"] == "tool_use")
    {
        out.clear();
    }
    out
}

/// Read an Anthropic SSE stream: assemble content blocks from deltas, pushing
/// partial text and thinking heartbeats to the shell as they arrive.
/// Returns (blocks, stop_reason, thinking_chars).
pub(crate) fn read_anthropic_sse(
    app: &Arc<App>,
    session: &str,
    reader: Box<dyn std::io::BufRead>,
) -> Result<(Vec<Value>, String, usize)> {
    #[derive(Default)]
    struct Blk {
        typ: String,
        text: String,
        sig: String,
        id: String,
        name: String,
        json: String,
        start_input: Value,
    }
    let mut blks: std::collections::BTreeMap<u64, Blk> = Default::default();
    let mut live_text = String::new();
    let mut think_chars: usize = 0;
    let mut stop_reason = String::new();
    let mut last_push: u64 = 0;
    use std::io::BufRead as _;
    for line in reader.lines() {
        let line = line.map_err(|e| anyhow!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(ev) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match ev["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let idx = ev["index"].as_u64().unwrap_or(0);
                let cb = &ev["content_block"];
                blks.insert(
                    idx,
                    Blk {
                        typ: cb["type"].as_str().unwrap_or("").to_string(),
                        text: cb["text"].as_str().unwrap_or("").to_string(),
                        id: cb["id"].as_str().unwrap_or("").to_string(),
                        name: cb["name"].as_str().unwrap_or("").to_string(),
                        start_input: cb["input"].clone(),
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
                        live_text.push_str(t);
                        if crate::store::now_ms().saturating_sub(last_push) > 250 {
                            app.notify(
                                "ai",
                                json!({"state": "streaming", "text": live_text,
                                                    "session": session}),
                            );
                            last_push = crate::store::now_ms();
                        }
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        blk.text.push_str(t);
                        think_chars += t.len();
                        // Heartbeat so long reasoning doesn't look stalled.
                        if crate::store::now_ms().saturating_sub(last_push) > 900 {
                            app.notify(
                                "ai",
                                json!({"state": "thinking", "chars": think_chars,
                                                    "session": session}),
                            );
                            last_push = crate::store::now_ms();
                        }
                    }
                    "signature_delta" => blk.sig.push_str(d["signature"].as_str().unwrap_or("")),
                    "input_json_delta" => {
                        blk.json.push_str(d["partial_json"].as_str().unwrap_or(""))
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                    stop_reason = sr.to_string();
                }
            }
            "error" => bail!(
                "provider stream error: {}",
                ev["error"]["message"].as_str().unwrap_or("unknown")
            ),
            "message_stop" => break,
            _ => {}
        }
    }
    if !live_text.is_empty() {
        app.notify(
            "ai",
            json!({"state": "streaming", "text": live_text, "session": session}),
        );
    }

    let mut out = Vec::new();
    for (_, b) in blks {
        match b.typ.as_str() {
            "text" if !b.text.is_empty() => out.push(json!({"type": "text", "text": b.text})),
            "thinking" => {
                out.push(json!({"type": "thinking", "thinking": b.text, "signature": b.sig}))
            }
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
                out.push(json!({"type": "tool_use", "id": b.id, "name": b.name, "input": input}));
            }
            _ => {}
        }
    }
    Ok((out, stop_reason, think_chars))
}

pub(crate) fn call_anthropic(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    let base = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| match cfg.provider.as_str() {
            "zai" => "https://api.z.ai/api/anthropic".into(),
            _ => "https://api.anthropic.com".into(),
        });
    let default_model = match cfg.provider.as_str() {
        "zai" => "glm-5.3",
        _ => "claude-sonnet-5",
    };
    // Replay thinking blocks ONLY for the final assistant turn (the one whose
    // tool results follow) so the model can resume mid-reasoning — that's the
    // Anthropic tool-use rule. Older turns' thinking is stripped: replaying a
    // whole run's reasoning makes GLM re-read and extend every prior thought,
    // so thinking snowballs each iteration until it eats the entire budget.
    let last_assistant = msgs.iter().rposition(|(r, _)| r == "assistant");
    let messages: Vec<Value> = msgs
        .iter()
        .enumerate()
        .map(|(i, (role, blocks))| {
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
                    // Extended-thinking blocks must be replayed verbatim
                    // (GLM-5.3 and Claude both emit them with tool use) —
                    // but only for the final assistant turn, see above.
                    "thinking" if Some(i) == last_assistant => Some(json!({"type": "thinking",
                        "thinking": b["thinking"], "signature": b["signature"]})),
                    _ => None,
                })
                .collect();
            json!({"role": role, "content": content})
        })
        .collect();
    // z.ai's Anthropic-compatible endpoint mis-streams GLM: tool-call blocks
    // never arrive as deltas, so a streamed turn looks like endless thinking
    // that eats the whole budget without ever acting (the ../chat app hit the
    // same thing and drives z.ai non-streaming for exactly this reason).
    // Non-streaming returns tool calls reliably, so zai defaults to it; real
    // Anthropic streams. Override per instance with config `ai.stream`.
    let stream = cfg.stream.unwrap_or(cfg.provider != "zai");

    // Thinking models can exhaust an explicit token budget before emitting
    // any text or tool call. When that happens, retry once with a doubled
    // budget before giving up. zai defaults to NO cap (see AiConfig) — the
    // mere presence of max_tokens makes z.ai hand the whole budget to
    // thinking — so the retry only applies when a cap was requested.
    let mut budget = match cfg.provider.as_str() {
        "zai" => cfg.max_tokens, // None = omit the field entirely
        _ => Some(
            cfg.max_tokens
                .unwrap_or_else(|| default_max_tokens(&cfg.provider)),
        ),
    };
    // Prompt caching pays off across a long tool loop, but only real
    // Anthropic is guaranteed to honour `cache_control`; z.ai's compatible
    // endpoint is left alone.
    let cache = cfg.provider == "anthropic";
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut body = json!({
            "model": if cfg.model.is_empty() { default_model } else { &cfg.model },
            "system": system_blocks(system, cache),
            "messages": cache_last_message(&messages, cache),
            "tools": cache_last_tool(tools, cache),
        });
        if let Some(b) = budget {
            body["max_tokens"] = json!(b);
        }
        if stream {
            body["stream"] = json!(true);
        }
        // z.ai accepts either header; real Anthropic uses x-api-key. Send both.
        let auth = format!("Bearer {}", cfg.api_key);
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
        // Browser build: Anthropic requires this opt-in header before it
        // answers cross-origin requests from a page.
        if cfg!(target_arch = "wasm32") {
            headers.push(("anthropic-dangerous-direct-browser-access", "true"));
        }
        // Streaming: no overall timeout (long thinking runs are legitimate),
        // but a 180s idle-read timeout so a dead connection can't hang.
        // Non-streaming: nothing arrives until the model is done, so the read
        // timeout must cover a full long-thinking generation.
        let resp = crate::net::request(
            "POST",
            &format!("{base}/v1/messages"),
            &headers,
            Some(&body.to_string()),
            20,
            if stream { 180 } else { 900 },
        )
        .map_err(|e| anyhow!("provider request failed: {e}"))?;
        if resp.status >= 400 {
            return Err(provider_error(resp));
        }
        let (out, stop_reason, think_chars) = if resp.content_type.contains("event-stream") {
            read_anthropic_sse(app, session, resp.reader)?
        } else {
            // One JSON message: we asked non-streaming (the zai default) or
            // the provider ignored the stream flag.
            let v: Value = serde_json::from_str(&resp.into_string()?)?;
            if v.get("error").is_some() {
                bail!(
                    "provider error: {}",
                    v["error"]["message"].as_str().unwrap_or("unknown")
                );
            }
            let stop = v["stop_reason"].as_str().unwrap_or("").to_string();
            (parse_anthropic_message(&v), stop, 0)
        };
        if out
            .iter()
            .any(|b| b["type"] == "text" || b["type"] == "tool_use")
        {
            return Ok(out);
        }
        // Nothing usable. Thinking-only + max_tokens = an explicit budget was
        // exhausted by reasoning; give it one more shot with double the room.
        if stop_reason == "max_tokens" && attempt == 1 && budget.is_some() {
            let old = budget.unwrap();
            budget = Some(old.saturating_mul(2).min(131_072));
            eprintln!(
                "uapp: model spent the whole {}-token budget thinking ({} chars); retrying with {}",
                old,
                think_chars,
                budget.unwrap()
            );
            app.notify("ai", json!({"state": "running", "session": session,
                "note": format!("model needed more room to think — retrying with {} tokens", budget.unwrap())}));
            continue;
        }
        if stop_reason == "max_tokens" && budget.is_some() {
            bail!(
                "the model used the entire {}-token budget on internal reasoning without \
                 producing an answer — raise max_tokens in Settings or simplify the request",
                budget.unwrap()
            );
        }
        bail!("empty response from provider (stop_reason: {stop_reason})");
    }
}

/// Anthropic Messages API client.
pub(crate) struct Anthropic;

impl super::Provider for Anthropic {
    fn call(
        &self,
        app: &Arc<App>,
        session: &str,
        cfg: &AiConfig,
        system: &str,
        msgs: &[(String, Vec<Value>)],
        tools: &[Value],
    ) -> Result<Vec<Value>> {
        call_anthropic(app, session, cfg, system, msgs, tools)
    }
}
