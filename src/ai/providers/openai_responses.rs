//! OpenAI's Responses API (POST {base}/responses), the endpoint OpenAI
//! recommends for tool-using work. Used only for direct OpenAI: OpenRouter
//! and local servers only implement chat/completions.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{
    ai_timeout, open_stream, post_json, read_sse, retryable, tool_input, transient_kind, Live,
    Reply, Request, Usage,
};
use crate::ai::{default_max_tokens, AiConfig};

const DEFAULT_MODEL: &str = "gpt-5.1";

/// Neutral messages as Responses API input items. Unlike chat/completions,
/// the system prompt is its own top-level field ("instructions").
pub(crate) fn to_input(msgs: &[(String, Vec<Value>)]) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    for (role, blocks) in msgs {
        if role == "assistant" {
            let mut text = String::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                    // Each tool call is its own top-level input item here
                    // (not nested in the message), matching how the API
                    // hands them back in `output`.
                    "tool_use" => input.push(json!({
                        "type": "function_call", "call_id": b["id"],
                        "name": b["name"], "arguments": b["input"].to_string(),
                    })),
                    // Reasoning items are optional to replay; the model
                    // reasons fresh each turn when they're omitted, so we
                    // drop them rather than round-trip encrypted content.
                    _ => {}
                }
            }
            if !text.is_empty() {
                input.push(json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]}));
            }
        } else {
            let mut parts = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "tool_result" => input.push(json!({
                        "type": "function_call_output", "call_id": b["id"],
                        "output": b["content"].as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| b["content"].to_string()),
                    })),
                    "text" => parts.push(json!({"type": "input_text", "text": b["text"]})),
                    "image" => parts.push(json!({"type": "input_image", "image_url":
                        format!("data:{};base64,{}", b["media"].as_str().unwrap_or("image/png"),
                                b["b64"].as_str().unwrap_or(""))})),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                input.push(json!({"type": "message", "role": "user", "content": parts}));
            }
        }
    }
    input
}

/// A finished response object, as blocks plus its status.
pub(crate) struct Parsed {
    pub blocks: Vec<Value>,
    pub status: String,
    pub incomplete_reason: String,
    pub usage: Usage,
    pub model: String,
}

pub(crate) fn parse_response(resp: &Value) -> Result<Parsed> {
    if resp["status"].as_str() == Some("failed") || resp["error"].is_object() {
        let msg = format!(
            "provider error: {}",
            resp["error"]["message"].as_str().unwrap_or("unknown")
        );
        if transient_kind(resp["error"]["code"].as_str().unwrap_or("")) {
            return Err(retryable(msg));
        }
        bail!(msg);
    }
    let mut blocks = Vec::new();
    for item in resp["output"].as_array().cloned().unwrap_or_default() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for c in item["content"].as_array().cloned().unwrap_or_default() {
                    if c["type"] == "output_text" {
                        if let Some(t) = c["text"].as_str() {
                            if !t.is_empty() {
                                blocks.push(json!({"type": "text", "text": t}));
                            }
                        }
                    }
                }
            }
            "function_call" => {
                let args = tool_input(item["arguments"].as_str().unwrap_or(""));
                blocks.push(json!({"type": "tool_use", "id": item["call_id"],
                    "name": item["name"], "input": args}));
            }
            // Surfaced as a "thinking" block so the chat UI's existing
            // thinking-line renderer picks it up; not replayed back (see above).
            "reasoning" => {
                let summary: String = item["summary"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|s| s["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !summary.is_empty() {
                    blocks.push(json!({"type": "thinking", "thinking": summary, "signature": ""}));
                }
            }
            _ => {}
        }
    }
    let u = &resp["usage"];
    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    Ok(Parsed {
        blocks,
        status: resp["status"].as_str().unwrap_or("").to_string(),
        incomplete_reason: resp["incomplete_details"]["reason"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        usage: Usage {
            input: u["input_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cached),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            cache_write: 0,
        },
        model: resp["model"].as_str().unwrap_or("").to_string(),
    })
}

/// Read a Responses API event stream: show text and reasoning as they
/// arrive, then take the final response object from the terminal event.
pub(crate) fn read_stream(
    reader: &mut dyn std::io::BufRead,
    stop: &dyn Fn() -> bool,
    live: &mut Live,
) -> Result<Value> {
    let mut done = Value::Null;
    read_sse(reader, stop, |data| {
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            return Ok(false);
        };
        match ev["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => live.text(ev["delta"].as_str().unwrap_or("")),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                live.thinking(ev["delta"].as_str().unwrap_or("").len(), false)
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                done = ev["response"].clone();
                return Ok(true);
            }
            "error" => {
                let msg = format!(
                    "provider stream error: {}",
                    ev["message"]
                        .as_str()
                        .or(ev["error"]["message"].as_str())
                        .unwrap_or("unknown")
                );
                let code = ev["code"].as_str().or(ev["error"]["code"].as_str());
                if transient_kind(code.unwrap_or("")) {
                    return Err(retryable(msg));
                }
                bail!(msg);
            }
            _ => {}
        }
        Ok(false)
    })?;
    live.flush();
    Ok(done)
}

/// Responses API client.
pub(crate) struct OpenAiResponses;

impl super::Provider for OpenAiResponses {
    fn call(&self, app: &Arc<App>, cfg: &AiConfig, req: &Request) -> Result<Reply> {
        let base = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let model = if cfg.model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            cfg.model.clone()
        };
        let input = to_input(req.msgs);
        // Responses API tools are flat (no nested "function" key, unlike
        // chat/completions).
        let oa_tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "name": t["name"],
                "description": t["description"], "parameters": t["input_schema"]})
            })
            .collect();
        let stream = cfg.stream.unwrap_or(true);
        let mut budget = cfg
            .max_tokens
            .unwrap_or_else(|| default_max_tokens(cfg, stream));
        let auth = format!("Bearer {}", cfg.api_key);
        let headers: Vec<(&str, &str)> = vec![
            ("Authorization", &auth),
            ("content-type", "application/json"),
        ];
        let url = format!("{base}/responses");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut body = json!({
                "model": model,
                "instructions": req.system,
                "input": input,
                "tools": oa_tools,
                "max_output_tokens": budget,
                // The chat history lives in the .uapp file already — OpenAI's
                // own server-side conversation storage would be redundant.
                "store": false,
            });
            let mut notify = |v: Value| app.notify("ai", v);
            let mut live = Live::new(req.session, &mut notify);
            let resp = if stream {
                body["stream"] = json!(true);
                let mut hdrs = headers.clone();
                hdrs.push(("accept", "text/event-stream"));
                let r = open_stream(&url, &hdrs, &body, ai_timeout(cfg))?;
                if r.content_type.contains("event-stream") {
                    let mut reader = r.reader;
                    read_stream(&mut *reader, req.stop, &mut live)?
                } else {
                    let text = r
                        .into_string()
                        .map_err(|e| retryable(format!("provider request failed: {e}")))?;
                    serde_json::from_str(&text)
                        .map_err(|e| anyhow!("provider sent unreadable JSON: {e}"))?
                }
            } else {
                post_json(&url, &headers, &body, ai_timeout(cfg), req.stop)?
            };
            let mut p = parse_response(&resp)?;
            let usable = p
                .blocks
                .iter()
                .any(|b| b["type"] == "text" || b["type"] == "tool_use");
            if p.status == "incomplete" {
                // Out of room before anything usable: one retry with double.
                if p.incomplete_reason == "max_output_tokens" && !usable && attempt == 1 {
                    budget = budget.saturating_mul(2).min(131_072);
                    app.notify("ai", json!({"state": "running", "session": req.session,
                        "note": format!("model needed more room to think — retrying with {budget} tokens")}));
                    continue;
                }
                if p.incomplete_reason != "max_output_tokens" {
                    bail!(
                        "the provider stopped the reply early ({})",
                        if p.incomplete_reason.is_empty() {
                            "no reason given"
                        } else {
                            &p.incomplete_reason
                        }
                    );
                }
                if !usable {
                    bail!(
                        "the model used the entire {budget}-token budget without producing an \
                         answer — raise max_tokens in Settings or simplify the request"
                    );
                }
                // A cut-off answer is kept, but marked, so nobody takes it
                // for the whole thing. A tool call cut off mid-arguments
                // already arrives marked invalid and is not run.
                p.blocks.push(json!({"type": "text", "text":
                    "(This reply hit the output limit and was cut off. Raising max tokens \
                     in Settings avoids that.)"}));
            }
            if !usable {
                bail!(
                    "empty response from provider: {}",
                    resp.to_string().chars().take(300).collect::<String>()
                );
            }
            return Ok(Reply {
                blocks: p.blocks,
                usage: p.usage,
                model: if p.model.is_empty() { model } else { p.model },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::CallError;

    #[test]
    fn stream_takes_the_final_response() {
        let final_resp = json!({"status": "completed", "model": "gpt-x", "output": [
            {"type": "function_call", "call_id": "r1", "name": "get_schema", "arguments": "{}"}],
            "usage": {"input_tokens": 50, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 20}}});
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.completed", "response": final_resp})
        );
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let v = read_stream(
            &mut std::io::Cursor::new(body.into_bytes()),
            &|| false,
            &mut live,
        )
        .unwrap();
        let p = parse_response(&v).unwrap();
        assert_eq!(p.blocks[0]["name"], "get_schema");
        assert_eq!(
            p.usage,
            Usage {
                input: 30,
                output: 5,
                cache_read: 20,
                cache_write: 0
            }
        );
    }

    #[test]
    fn stream_without_terminal_event_is_retryable() {
        let body = format!(
            "data: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "hi"})
        );
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let e = read_stream(
            &mut std::io::Cursor::new(body.into_bytes()),
            &|| false,
            &mut live,
        )
        .err()
        .unwrap();
        assert!(matches!(
            e.downcast_ref::<CallError>(),
            Some(CallError::Retryable { .. })
        ));
    }
}
