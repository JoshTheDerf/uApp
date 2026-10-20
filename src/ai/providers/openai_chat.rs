//! OpenAI chat/completions: OpenRouter and local servers.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{ai_timeout, http_post, provider_error, tool_input};
use crate::ai::{default_max_tokens, AiConfig};

/// Read an OpenAI chat/completions SSE stream (the shape Ollama, LM Studio,
/// llama.cpp's server, vLLM, OpenRouter all emit). Accumulates streamed text
/// (pushed to the shell as it arrives, like the Anthropic path) and tool-call
/// fragments — `tool_calls[]` deltas carry an `index`; `id`/`name` appear once,
/// `arguments` arrive as string fragments to concatenate — then assembles the
/// same neutral blocks the non-streamed branch returns.
/// Returns the neutral blocks, the final `finish_reason`, and how many chars
/// of `reasoning_content` streamed by. The last two let the caller tell a model
/// that ran out of room mid-thought from one that genuinely returned nothing.
pub(crate) fn read_openai_sse(
    app: &Arc<App>,
    session: &str,
    reader: Box<dyn std::io::BufRead>,
) -> Result<(Vec<Value>, String, usize)> {
    #[derive(Default)]
    struct Call {
        id: String,
        name: String,
        args: String,
    }
    let mut text = String::new();
    let mut live_text = String::new();
    let mut calls: std::collections::BTreeMap<u64, Call> = Default::default();
    let mut last_push: u64 = 0;
    let mut think_chars: usize = 0;
    let mut last_think_push: u64 = 0;
    let mut finish = String::new();
    use std::io::BufRead as _;
    for line in reader.lines() {
        let line = line.map_err(|e| anyhow!("stream read failed: {e}"))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // Some servers surface errors mid-stream as a JSON object with `error`.
        if let Some(msg) = ev["error"]["message"].as_str() {
            bail!("provider stream error: {msg}");
        }
        if let Some(f) = ev["choices"][0]["finish_reason"].as_str() {
            if !f.is_empty() {
                finish = f.to_string();
            }
        }
        let delta = &ev["choices"][0]["delta"];
        // Thinking models (Qwen3, DeepSeek-R1, GLM) stream reasoning in a
        // separate field. It is not part of the answer and must not be
        // concatenated into it, but it has to be observed: without it a long
        // think looks like a stalled connection, and a turn that spends its
        // whole budget reasoning would come back as a bare "empty response".
        if let Some(r) = delta["reasoning_content"].as_str() {
            if !r.is_empty() {
                think_chars += r.len();
                // Heartbeat so long reasoning doesn't look stalled.
                if crate::store::now_ms().saturating_sub(last_think_push) > 900 {
                    app.notify(
                        "ai",
                        json!({"state": "thinking", "chars": think_chars,
                                            "session": session}),
                    );
                    last_think_push = crate::store::now_ms();
                }
            }
        }
        if let Some(t) = delta["content"].as_str() {
            if !t.is_empty() {
                text.push_str(t);
                live_text.push_str(t);
                if crate::store::now_ms().saturating_sub(last_push) > 250 {
                    app.notify(
                        "ai",
                        json!({"state": "streaming", "text": live_text, "session": session}),
                    );
                    last_push = crate::store::now_ms();
                }
            }
        }
        for tc in delta["tool_calls"].as_array().cloned().unwrap_or_default() {
            let idx = tc["index"].as_u64().unwrap_or(0);
            let call = calls.entry(idx).or_default();
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() {
                    call.id = id.to_string();
                }
            }
            if let Some(n) = tc["function"]["name"].as_str() {
                if !n.is_empty() {
                    call.name = n.to_string();
                }
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                call.args.push_str(a);
            }
        }
    }
    if !live_text.is_empty() {
        app.notify(
            "ai",
            json!({"state": "streaming", "text": live_text, "session": session}),
        );
    }

    let mut out = Vec::new();
    if !text.is_empty() {
        out.push(json!({"type": "text", "text": text}));
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
        out.push(json!({"type": "tool_use", "id": id, "name": c.name, "input": args}));
    }
    Ok((out, finish, think_chars))
}

pub(crate) fn call_openai_compat(
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
            "zai" => "https://api.z.ai/api/paas/v4".into(),
            "openrouter" => "https://openrouter.ai/api/v1".into(),
            // Ollama's OpenAI-compatible endpoint, by far the most common local
            // setup. LM Studio, llama.cpp's server, vLLM etc. all listen on a
            // different port — Base URL in Settings overrides this.
            "local" => "http://localhost:11434/v1".into(),
            _ => "https://api.openai.com/v1".into(),
        });
    let default_model = match cfg.provider.as_str() {
        "zai" => "glm-4.6",
        "openrouter" => "anthropic/claude-sonnet-4.5",
        _ => "gpt-4o",
    };
    // Cloud providers have one sane default model; a local server can be
    // running anything, so there's no default worth guessing — the name must
    // match a model already pulled/loaded there.
    if cfg.provider == "local" && cfg.model.is_empty() {
        bail!(
            "set Model in Settings to the exact name of a model already loaded on your \
             local server (e.g. \"qwen3.8\" for Ollama)"
        );
    }
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
    let oa_tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({"type": "function", "function": {
                "name": t["name"], "description": t["description"],
                "parameters": t["input_schema"]}})
        })
        .collect();
    // Stream local servers by default: a slow local model (a few tokens/sec on
    // CPU) otherwise looks stalled for minutes, and streaming turns the overall
    // deadline into an idle-read one so a long-but-progressing generation never
    // trips it. OpenRouter/OpenAI-compat stay non-streaming unless `ai.stream`
    // opts in. Override per instance with config `ai.stream`.
    let stream = cfg.stream.unwrap_or(cfg.provider == "local");
    // Thinking models can burn an entire token budget on reasoning before
    // emitting any text or tool call. The Anthropic path already retries once
    // with a doubled budget for exactly that failure; do the same here, since
    // local servers run precisely those models (Qwen3, DeepSeek-R1, GLM).
    let mut budget = cfg
        .max_tokens
        .unwrap_or_else(|| default_max_tokens(&cfg.provider));
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut body = json!({
            "model": if cfg.model.is_empty() { default_model } else { cfg.model.as_str() },
            "messages": messages.clone(),
            "tools": oa_tools.clone(),
            "max_tokens": budget,
        });

        let (out, finish, think_chars) = if !stream {
            let resp = http_post(
                &format!("{base}/chat/completions"),
                &[
                    ("Authorization", &format!("Bearer {}", cfg.api_key)),
                    ("content-type", "application/json"),
                ],
                body,
                ai_timeout(cfg),
            )?;
            let m = &resp["choices"][0]["message"];
            let mut out = Vec::new();
            if let Some(t) = m["content"].as_str() {
                if !t.is_empty() {
                    out.push(json!({"type": "text", "text": t}));
                }
            }
            for tc in m["tool_calls"].as_array().cloned().unwrap_or_default() {
                let args = tool_input(tc["function"]["arguments"].as_str().unwrap_or(""));
                out.push(json!({"type": "tool_use", "id": tc["id"],
                    "name": tc["function"]["name"], "input": args}));
            }
            let finish = resp["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let think = m["reasoning_content"]
                .as_str()
                .map(|t| t.len())
                .unwrap_or(0);
            (out, finish, think)
        } else {
            body["stream"] = json!(true);
            // Ask for usage in the final chunk too (harmless if the server ignores it).
            body["stream_options"] = json!({"include_usage": true});
            // Streaming: the read timeout is per-chunk (idle), not overall, so a model
            // that keeps emitting tokens can run as long as it needs; ai_timeout is a
            // generous ceiling on any single silent gap (covers slow prompt prefill).
            let auth = format!("Bearer {}", cfg.api_key);
            let resp = crate::net::request(
                "POST",
                &format!("{base}/chat/completions"),
                &[
                    ("Authorization", &auth),
                    ("content-type", "application/json"),
                    ("accept", "text/event-stream"),
                ],
                Some(&body.to_string()),
                20,
                ai_timeout(cfg).as_secs(),
            )
            .map_err(|e| anyhow!("provider request failed: {e}"))?;
            if resp.status >= 400 {
                return Err(provider_error(resp));
            }
            read_openai_sse(app, session, resp.reader)?
        };

        if !out.is_empty() {
            return Ok(out);
        }
        // Nothing usable. If the model was still reasoning when it hit the cap,
        // one retry with double the room usually lands an answer.
        if finish == "length" && think_chars > 0 && attempt == 1 {
            let old = budget;
            budget = budget.saturating_mul(2).min(131_072);
            eprintln!(
                "uapp: model spent the whole {old}-token budget thinking ({think_chars} chars); \
                 retrying with {budget}"
            );
            app.notify("ai", json!({"state": "running", "session": session,
                "note": format!("model needed more room to think — retrying with {budget} tokens")}));
            continue;
        }
        if finish == "length" && think_chars > 0 {
            bail!(
                "the model used the entire {budget}-token budget on internal reasoning \
                 without producing an answer — raise max_tokens in Settings or simplify \
                 the request"
            );
        }
        bail!("empty response from provider (finish_reason: {finish})");
    }
}

/// chat/completions client.
pub(crate) struct OpenAiChat;

impl super::Provider for OpenAiChat {
    fn call(
        &self,
        app: &Arc<App>,
        session: &str,
        cfg: &AiConfig,
        system: &str,
        msgs: &[(String, Vec<Value>)],
        tools: &[Value],
    ) -> Result<Vec<Value>> {
        call_openai_compat(app, session, cfg, system, msgs, tools)
    }
}
