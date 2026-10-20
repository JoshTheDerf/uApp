//! OpenAI chat/completions: OpenRouter, local servers, and any other
//! OpenAI-compatible endpoint.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::app::App;

use super::{
    Live, Reply, Request, Usage, ai_timeout, open_stream, post_json, read_sse, retryable,
    tool_input, transient_kind,
};
use crate::ai::{AiConfig, default_max_tokens};

const OPENROUTER_MODEL: &str = "anthropic/claude-opus-5.5";

/// A stream as read off the wire.
#[derive(Default)]
pub(crate) struct Parsed {
    pub blocks: Vec<Value>,
    pub finish: String,
    pub think_chars: usize,
    pub usage: Usage,
    pub model: String,
}

fn read_usage(u: &Value, usage: &mut Usage) {
    if !u.is_object() {
        return;
    }
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    usage.input = prompt.saturating_sub(cached);
    usage.cache_read = cached;
    usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
}

/// An error object some servers put in a 200 body or mid-stream.
fn error_object(v: &Value) -> Option<anyhow::Error> {
    let err = v.get("error")?;
    let msg = format!(
        "provider stream error: {}",
        err["message"]
            .as_str()
            .or(err.as_str())
            .unwrap_or("unknown")
    );
    let kind = err["type"].as_str().or(err["code"].as_str()).unwrap_or("");
    let code = err["code"].as_u64().unwrap_or(0);
    Some(if transient_kind(kind) || code == 429 || code >= 500 {
        retryable(msg)
    } else {
        anyhow!(msg)
    })
}

/// Read an OpenAI chat/completions SSE stream (the shape Ollama, LM Studio,
/// llama.cpp's server, vLLM, OpenRouter all emit). Accumulates streamed text
/// (pushed to the shell as it arrives, like the Anthropic path) and tool-call
/// fragments — `tool_calls[]` deltas carry an `index`; `id`/`name` appear once,
/// `arguments` arrive as string fragments to concatenate — then assembles the
/// same neutral blocks the non-streamed branch returns. The finish reason and
/// how many chars of `reasoning_content` streamed by let the caller tell a
/// model that ran out of room mid-thought from one that returned nothing.
pub(crate) fn read_stream(
    reader: &mut dyn std::io::BufRead,
    stop: &dyn Fn() -> bool,
    live: &mut Live,
) -> Result<Parsed> {
    #[derive(Default)]
    struct Call {
        id: String,
        name: String,
        args: String,
    }
    let mut text = String::new();
    let mut calls: std::collections::BTreeMap<u64, Call> = Default::default();
    let mut p = Parsed::default();
    read_sse(reader, stop, |data| {
        if data == "[DONE]" {
            return Ok(true);
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            return Ok(false);
        };
        // Some servers surface errors mid-stream as a JSON object with `error`.
        if let Some(e) = error_object(&ev) {
            return Err(e);
        }
        if let Some(m) = ev["model"].as_str() {
            p.model = m.to_string();
        }
        read_usage(&ev["usage"], &mut p.usage);
        if let Some(f) = ev["choices"][0]["finish_reason"].as_str()
            && !f.is_empty()
        {
            p.finish = f.to_string();
        }
        let delta = &ev["choices"][0]["delta"];
        // Thinking models (Qwen3, DeepSeek-R1, GLM) stream reasoning in a
        // separate field. It is not part of the answer and must not be
        // concatenated into it, but it has to be observed: without it a long
        // think looks like a stalled connection, and a turn that spends its
        // whole budget reasoning would come back as a bare "empty response".
        for key in ["reasoning_content", "reasoning"] {
            if let Some(r) = delta[key].as_str()
                && !r.is_empty()
            {
                live.thinking(r.len(), false);
            }
        }
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty()
        {
            text.push_str(t);
            live.text(t);
        }
        for tc in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
            let idx = tc["index"].as_u64().unwrap_or(0);
            let call = calls.entry(idx).or_default();
            if let Some(id) = tc["id"].as_str()
                && !id.is_empty()
            {
                call.id = id.to_string();
            }
            if let Some(n) = tc["function"]["name"].as_str()
                && !n.is_empty()
            {
                call.name = n.to_string();
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                call.args.push_str(a);
            }
        }
        Ok(false)
    })?;
    live.flush();
    p.think_chars = live.think_chars;
    if !text.is_empty() {
        p.blocks.push(json!({"type": "text", "text": text}));
    }
    for (i, c) in calls {
        let args = tool_input(&c.args);
        // A tool call needs an id for the follow-up tool_result to reference;
        // synthesize a stable one if the server omitted it.
        let id = if c.id.is_empty() {
            format!("call_{i}")
        } else {
            c.id
        };
        p.blocks
            .push(json!({"type": "tool_use", "id": id, "name": c.name, "input": args}));
    }
    Ok(p)
}

/// A complete (non-streamed) chat/completions response.
pub(crate) fn parse_response(resp: &Value) -> Result<Parsed> {
    if let Some(e) = error_object(resp) {
        return Err(e);
    }
    let m = &resp["choices"][0]["message"];
    let mut p = Parsed {
        finish: resp["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        think_chars: m["reasoning_content"]
            .as_str()
            .or(m["reasoning"].as_str())
            .map(|t| t.len())
            .unwrap_or(0),
        model: resp["model"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    };
    read_usage(&resp["usage"], &mut p.usage);
    if let Some(t) = m["content"].as_str()
        && !t.is_empty()
    {
        p.blocks.push(json!({"type": "text", "text": t}));
    }
    for tc in m["tool_calls"].as_array().cloned().unwrap_or_default() {
        let args = tool_input(tc["function"]["arguments"].as_str().unwrap_or(""));
        p.blocks.push(json!({"type": "tool_use", "id": tc["id"],
            "name": tc["function"]["name"], "input": args}));
    }
    Ok(p)
}

/// Neutral messages in chat/completions shape, system prompt first.
pub(crate) fn to_messages(system: &str, msgs: &[(String, Vec<Value>)]) -> Vec<Value> {
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
    for (role, blocks) in msgs {
        if role == "assistant" {
            let mut text = String::new();
            let mut tool_calls = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "text" => text.push_str(b["text"].as_str().unwrap_or("")),
                    "tool_use" => tool_calls.push(json!({
                        "id": b["id"], "type": "function",
                        "function": {"name": b["name"], "arguments": b["input"].to_string()}
                    })),
                    _ => {}
                }
            }
            let mut m = json!({"role": "assistant"});
            m["content"] = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            if !tool_calls.is_empty() {
                m["tool_calls"] = json!(tool_calls);
            }
            messages.push(m);
        } else {
            // user turn: tool results become role:"tool" messages first
            let mut parts = Vec::new();
            for b in blocks {
                match b["type"].as_str().unwrap_or("") {
                    "tool_result" => messages.push(json!({
                        "role": "tool", "tool_call_id": b["id"],
                        "content": b["content"].as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| b["content"].to_string()),
                    })),
                    "text" => parts.push(json!({"type": "text", "text": b["text"]})),
                    "image" => parts.push(json!({"type": "image_url", "image_url": {"url":
                        format!("data:{};base64,{}", b["media"].as_str().unwrap_or("image/png"),
                                b["b64"].as_str().unwrap_or(""))}})),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                messages.push(json!({"role": "user", "content": parts}));
            }
        }
    }
    messages
}

/// chat/completions client.
pub(crate) struct OpenAiChat;

impl super::Provider for OpenAiChat {
    fn call(&self, app: &Arc<App>, cfg: &AiConfig, req: &Request) -> Result<Reply> {
        let custom = matches!(cfg.provider.as_str(), "local" | "openai-compatible");
        let base = match (cfg.base_url.clone(), cfg.provider.as_str()) {
            (Some(b), _) => b.trim_end_matches('/').to_string(),
            (None, "openrouter") => "https://openrouter.ai/api/v1".into(),
            // Ollama's OpenAI-compatible endpoint, by far the most common local
            // setup. LM Studio, llama.cpp's server, vLLM etc. all listen on a
            // different port — Base URL in Settings overrides this.
            (None, "local") => "http://localhost:11434/v1".into(),
            _ => bail!(
                "set Base URL in Settings to your server's OpenAI-compatible endpoint \
                 (the part before /chat/completions, e.g. https://api.example.com/v1)"
            ),
        };
        // OpenRouter has a sane default model; a custom server can be running
        // anything, so there's no default worth guessing — the name must match
        // a model already loaded there.
        let model = match (cfg.model.as_str(), custom) {
            ("", true) => bail!(
                "set Model in Settings to the exact name of a model your server offers \
                 (e.g. \"qwen3.8\" for Ollama)"
            ),
            ("", false) => OPENROUTER_MODEL.to_string(),
            (m, _) => m.to_string(),
        };
        let messages = to_messages(req.system, req.msgs);
        let oa_tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t["name"], "description": t["description"],
                    "parameters": t["input_schema"]}})
            })
            .collect();
        // Stream custom servers by default: a slow local model (a few
        // tokens/sec on CPU) otherwise looks stalled for minutes, Stop answers
        // between chunks, and streaming turns the overall deadline into an
        // idle-read one so a long-but-progressing generation never trips it.
        // OpenRouter stays non-streaming unless `ai.stream` opts in.
        let stream = cfg.stream.unwrap_or(custom);
        // Thinking models can burn an entire token budget on reasoning before
        // emitting any text or tool call. Retry once with a doubled budget,
        // since local servers run precisely those models (Qwen3, DeepSeek-R1).
        let mut budget = cfg
            .max_tokens
            .unwrap_or_else(|| default_max_tokens(cfg, stream));
        let auth = format!("Bearer {}", cfg.api_key);
        let mut headers: Vec<(&str, &str)> = vec![("content-type", "application/json")];
        // A local server usually wants no key, and some reject an empty one.
        if !cfg.api_key.is_empty() {
            headers.push(("Authorization", &auth));
        }
        let url = format!("{base}/chat/completions");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut body = json!({
                "model": model,
                "messages": messages,
                "max_tokens": budget,
            });
            if !oa_tools.is_empty() {
                body["tools"] = json!(oa_tools);
            }
            let mut notify = |v: Value| app.notify("ai", v);
            let mut live = Live::new(req.session, &mut notify);
            let p = if !stream {
                parse_response(&post_json(
                    &url,
                    &headers,
                    &body,
                    ai_timeout(cfg),
                    req.stop,
                )?)?
            } else {
                body["stream"] = json!(true);
                // Usage arrives in a final chunk (ignored by servers that don't know it).
                body["stream_options"] = json!({"include_usage": true});
                let mut hdrs = headers.clone();
                hdrs.push(("accept", "text/event-stream"));
                // The read timeout is per-chunk (idle), not overall, so a model
                // that keeps emitting tokens can run as long as it needs;
                // ai_timeout is a generous ceiling on any single silent gap
                // (covers slow prompt prefill).
                let resp = open_stream(&url, &hdrs, &body, ai_timeout(cfg))?;
                if resp.content_type.contains("event-stream") {
                    let mut reader = resp.reader;
                    read_stream(&mut *reader, req.stop, &mut live)?
                } else {
                    let text = resp
                        .into_string()
                        .map_err(|e| retryable(format!("provider request failed: {e}")))?;
                    parse_response(
                        &serde_json::from_str(&text)
                            .map_err(|e| anyhow!("provider sent unreadable JSON: {e}"))?,
                    )?
                }
            };
            if !p.blocks.is_empty() {
                return Ok(Reply {
                    blocks: p.blocks,
                    usage: p.usage,
                    model: if p.model.is_empty() { model } else { p.model },
                });
            }
            // Nothing usable. If the model was still reasoning when it hit the cap,
            // one retry with double the room usually lands an answer.
            if p.finish == "length" && p.think_chars > 0 && attempt == 1 {
                let old = budget;
                budget = budget.saturating_mul(2).min(131_072);
                eprintln!(
                    "uapp: model spent the whole {old}-token budget thinking ({} chars); \
                     retrying with {budget}",
                    p.think_chars
                );
                app.notify("ai", json!({"state": "running", "session": req.session,
                    "note": format!("model needed more room to think — retrying with {budget} tokens")}));
                continue;
            }
            if p.finish == "length" && p.think_chars > 0 {
                bail!(
                    "the model used the entire {budget}-token budget on internal reasoning \
                     without producing an answer — raise max_tokens in Settings or simplify \
                     the request"
                );
            }
            bail!("empty response from provider (finish_reason: {})", p.finish);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::providers::CallError;

    fn sse(chunks: &[&str]) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(
            chunks
                .iter()
                .map(|c| format!("data: {c}\n\n"))
                .collect::<String>()
                .into_bytes(),
        )
    }

    const CHUNKS: [&str; 6] = [
        r#"{"model":"m1","choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
        r#"{"choices":[{"delta":{"content":"Doing it."}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"sql_query","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"sql\":\"SELECT 1\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":60}}}"#,
    ];

    #[test]
    fn stream_assembles_text_tool_calls_and_usage() {
        let mut chunks = CHUNKS.to_vec();
        chunks.push("[DONE]");
        let mut events = Vec::new();
        let mut notify = |v: Value| events.push(v);
        let mut live = Live::new("s", &mut notify);
        let p = read_stream(&mut sse(&chunks), &|| false, &mut live).unwrap();
        assert_eq!(p.finish, "tool_calls");
        assert_eq!(p.model, "m1");
        assert_eq!(p.think_chars, 3);
        assert_eq!(
            p.usage,
            Usage {
                input: 40,
                output: 7,
                cache_read: 60,
                cache_write: 0
            }
        );
        assert_eq!(p.blocks[0]["text"], "Doing it.");
        assert_eq!(p.blocks[1]["input"], json!({"sql": "SELECT 1"}));
        assert!(events.iter().any(|e| e["state"] == "thinking"));
    }

    #[test]
    fn stream_without_done_is_retryable() {
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let e = read_stream(&mut sse(&CHUNKS), &|| false, &mut live)
            .err()
            .unwrap();
        assert!(matches!(
            e.downcast_ref::<CallError>(),
            Some(CallError::Retryable { .. })
        ));
    }

    #[test]
    fn mid_stream_errors_are_classified() {
        let mut notify = |_v: Value| {};
        let mut live = Live::new("s", &mut notify);
        let e = read_stream(
            &mut sse(&[r#"{"error":{"message":"busy","type":"server_error"}}"#]),
            &|| false,
            &mut live,
        )
        .err()
        .unwrap();
        assert!(e.downcast_ref::<CallError>().is_some());
        let e = read_stream(
            &mut sse(&[r#"{"error":{"message":"bad model","type":"invalid_request_error"}}"#]),
            &|| false,
            &mut live,
        )
        .err()
        .unwrap();
        assert!(e.downcast_ref::<CallError>().is_none());
    }
}
