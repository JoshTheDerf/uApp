//! OpenAI's Responses API, for direct OpenAI.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{ai_timeout, http_post, tool_input};
use crate::ai::{default_max_tokens, AiConfig};

/// OpenAI's Responses API (POST {base}/responses) — the endpoint OpenAI now
/// recommends over Chat Completions for tool-using/agentic workloads. Used
/// only for direct OpenAI calls: OpenRouter and local servers (Ollama, LM
/// Studio, llama.cpp's server, vLLM, ...) only implement the older
/// chat/completions shape, so they stay on call_openai_compat.
pub(crate) fn call_openai_responses(
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    let base = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let default_model = "gpt-5.1";

    // Unlike chat/completions, the system prompt is its own top-level field
    // ("instructions"), not an input item.
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

    // Responses API tools are flat (no nested "function" key, unlike
    // chat/completions).
    let oa_tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({"type": "function", "name": t["name"],
            "description": t["description"], "parameters": t["input_schema"]})
        })
        .collect();

    let body = json!({
        "model": if cfg.model.is_empty() { default_model } else { cfg.model.as_str() },
        "instructions": system,
        "input": input,
        "tools": oa_tools,
        "max_output_tokens": cfg.max_tokens.unwrap_or_else(|| default_max_tokens(&cfg.provider)),
        // Every device shares one chat history in the .uapp file already —
        // OpenAI's own server-side conversation storage would be redundant.
        "store": false,
    });
    let resp = http_post(
        &format!("{base}/responses"),
        &[
            ("Authorization", &format!("Bearer {}", cfg.api_key)),
            ("content-type", "application/json"),
        ],
        body,
        ai_timeout(cfg),
    )?;
    if resp["status"].as_str() == Some("failed") {
        bail!(
            "provider error: {}",
            resp["error"]["message"].as_str().unwrap_or("unknown")
        );
    }
    let mut out = Vec::new();
    for item in resp["output"].as_array().cloned().unwrap_or_default() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for c in item["content"].as_array().cloned().unwrap_or_default() {
                    if c["type"] == "output_text" {
                        if let Some(t) = c["text"].as_str() {
                            if !t.is_empty() {
                                out.push(json!({"type": "text", "text": t}));
                            }
                        }
                    }
                }
            }
            "function_call" => {
                let args = tool_input(item["arguments"].as_str().unwrap_or(""));
                out.push(json!({"type": "tool_use", "id": item["call_id"],
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
                    out.push(json!({"type": "thinking", "thinking": summary, "signature": ""}));
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        bail!(
            "empty response from provider: {}",
            resp.to_string().chars().take(300).collect::<String>()
        );
    }
    Ok(out)
}

/// Responses API client.
pub(crate) struct OpenAiResponses;

impl super::Provider for OpenAiResponses {
    fn call(
        &self,
        _app: &Arc<App>,
        _session: &str,
        cfg: &AiConfig,
        system: &str,
        msgs: &[(String, Vec<Value>)],
        tools: &[Value],
    ) -> Result<Vec<Value>> {
        call_openai_responses(cfg, system, msgs, tools)
    }
}
