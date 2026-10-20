//! Provider clients. Each speaks one wire format and returns assistant
//! blocks in the neutral shape history.rs builds.

mod anthropic;
mod openai_chat;
mod openai_responses;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::app::App;

use super::{AiConfig, RunCtx};

/// One provider wire format.
pub(crate) trait Provider {
    fn call(
        &self,
        app: &Arc<App>,
        session: &str,
        cfg: &AiConfig,
        system: &str,
        msgs: &[(String, Vec<Value>)],
        tools: &[Value],
    ) -> Result<Vec<Value>>;
}

/// Returns assistant blocks in neutral form.
/// Parse a tool call's argument JSON as the provider sent it. A stream cut
/// off mid-arguments (or a model emitting junk) must NOT turn into a call with
/// empty arguments — `write_file {}` is far worse than no call — so the
/// unparsable text is kept under `_invalid` and the loop reports it instead
/// of running the tool.
pub(crate) fn tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => json!({"_invalid": raw}),
    }
}

/// Retry transient provider failures (rate limits, 5xx, dropped
/// connections) with a short backoff before giving up on the turn; a stop
/// request ends the wait early.
pub(crate) fn call_provider_retrying(
    app: &Arc<App>,
    ctx: &RunCtx,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    const DELAYS_MS: [u64; 3] = [1_000, 4_000, 12_000];
    let mut attempt = 0;
    loop {
        match call_provider(app, &ctx.session, cfg, system, msgs, tools) {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = e.to_string();
                // "stream read failed" is the response socket dying mid-SSE —
                // seen on every mobile background/resume cycle (the OS freezes
                // the process and kills its sockets) and on wifi↔cell handoffs.
                // The request is re-sent from scratch, so it retries cleanly.
                let transient = msg.starts_with("provider request failed")
                    || msg.starts_with("stream read failed")
                    || [
                        "provider returned 408",
                        "provider returned 429",
                        "provider returned 5",
                    ]
                    .iter()
                    .any(|p| msg.starts_with(p));
                if !transient || attempt >= DELAYS_MS.len() || ctx.stopped() {
                    return Err(e);
                }
                app.notify(
                    "ai",
                    json!({"state": "retry", "session": ctx.session,
                    "message": format!("{msg} — retrying in {}s", DELAYS_MS[attempt] / 1000)}),
                );
                let _ = app.wait_until(DELAYS_MS[attempt], 250, || ctx.stopped());
                attempt += 1;
            }
        }
    }
}

/// The client for a configured provider name.
pub(crate) fn for_config(cfg: &AiConfig) -> Result<Box<dyn Provider>> {
    Ok(match cfg.provider.as_str() {
        // z.ai coding-plan keys only work on the Anthropic-compatible
        // endpoint (paas/v4 rejects them with "insufficient balance"),
        // and API-plan keys work there too — so zai always speaks
        // Anthropic wire format. Streamed: GLM-5.3 thinks for a long time
        // and a non-streaming request looks like a stall (or times out).
        "anthropic" | "zai" => Box::new(anthropic::Anthropic),
        // "local" is any OpenAI-compatible server on this machine (Ollama, LM
        // Studio, llama.cpp's server, vLLM, text-generation-webui, ...) — they
        // only implement the older chat/completions shape, same as OpenRouter.
        "openrouter" | "local" => Box::new(openai_chat::OpenAiChat),
        // Direct OpenAI gets their newer Responses API (see call_openai_responses).
        "openai" => Box::new(openai_responses::OpenAiResponses),
        other => {
            bail!("unknown provider '{other}' (use anthropic, zai, openrouter, openai or local)")
        }
    })
}

pub(crate) fn call_provider(
    app: &Arc<App>,
    session: &str,
    cfg: &AiConfig,
    system: &str,
    msgs: &[(String, Vec<Value>)],
    tools: &[Value],
) -> Result<Vec<Value>> {
    for_config(cfg)?.call(app, session, cfg, system, msgs, tools)
}

/// Overall request timeout for a non-streamed provider call. This is the whole
/// blocking POST — nothing comes back until the model is completely done — so
/// it must cover a full generation. A local server can be genuinely slow (a
/// big model on CPU may emit only a few tokens/sec), where a long answer takes
/// many minutes; 5 minutes was far too tight and cut off any non-trivial
/// request. Local therefore gets a very generous ceiling; cloud providers keep
/// the tighter one. `UAPP_AI_TIMEOUT_SECS` overrides both.
pub(crate) fn ai_timeout(cfg: &AiConfig) -> std::time::Duration {
    let secs = std::env::var("UAPP_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(if cfg.provider == "local" { 3600 } else { 300 });
    std::time::Duration::from_secs(secs)
}

/// One place to turn a failed provider HTTP response into an error, so the
/// message shape ("provider returned 429: …") is identical on every path.
pub(crate) fn provider_error(resp: crate::net::Resp) -> anyhow::Error {
    let code = resp.status;
    let body = resp.into_string().unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or(v["message"].as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    anyhow!("provider returned {code}: {msg}")
}

pub(crate) fn http_post(
    url: &str,
    headers: &[(&str, &str)],
    body: Value,
    timeout: std::time::Duration,
) -> Result<Value> {
    let mut hdrs = headers.to_vec();
    hdrs.push(("content-type", "application/json"));
    let resp = crate::net::request(
        "POST",
        url,
        &hdrs,
        Some(&body.to_string()),
        20,
        timeout.as_secs(),
    )
    .map_err(|e| anyhow!("provider request failed: {e}"))?;
    if resp.status >= 400 {
        return Err(provider_error(resp));
    }
    Ok(serde_json::from_str(&resp.into_string()?)?)
}
