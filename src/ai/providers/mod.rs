//! Provider clients. Each speaks one wire format and returns assistant
//! blocks in the neutral shape history.rs builds.
//!
//! Every client honours the run's stop flag while it waits (per streamed
//! line, or by polling around a blocking request), and classifies failures:
//! [`CallError::Stopped`], [`CallError::Retryable`] (rate limits, overload,
//! 5xx, dropped or truncated streams) or a plain error that retrying cannot fix.

mod anthropic;
mod openai_chat;
mod openai_responses;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::BufRead;
use std::sync::Arc;

use crate::app::App;

use super::{AiConfig, RunCtx};

/// One provider call: what to send, and how to report progress.
pub(crate) struct Request<'a> {
    /// The chat session live progress (streamed text, thinking) is shown in.
    pub session: &'a str,
    pub system: &'a str,
    pub msgs: &'a [(String, Vec<Value>)],
    /// Neutral tool definitions: `{name, description, input_schema}`.
    pub tools: &'a [Value],
    pub stop: &'a dyn Fn() -> bool,
}

/// Token counts for one provider call, normalized across providers:
/// `input` is the uncached part of the prompt.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Usage::default()
    }
    pub(crate) fn to_json(self) -> Value {
        json!({"input_tokens": self.input, "output_tokens": self.output,
               "cache_read_tokens": self.cache_read, "cache_write_tokens": self.cache_write})
    }
}

/// What a provider call produced.
pub(crate) struct Reply {
    pub blocks: Vec<Value>,
    pub usage: Usage,
    /// The model that answered, as the provider reported it.
    pub model: String,
}

/// One provider wire format.
pub(crate) trait Provider {
    fn call(&self, app: &Arc<App>, cfg: &AiConfig, req: &Request) -> Result<Reply>;
}

/// Failures the caller handles differently from "this request is wrong".
#[derive(Debug)]
pub(crate) enum CallError {
    /// The user pressed Stop while we waited.
    Stopped,
    /// Worth sending again: rate limits, overload, 5xx, a dropped or
    /// truncated stream. `retry_after_ms` is the server's own hint.
    Retryable {
        msg: String,
        retry_after_ms: Option<u64>,
    },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Stopped => write!(f, "stopped by the user"),
            CallError::Retryable { msg, .. } => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CallError {}

pub(crate) fn retryable(msg: impl Into<String>) -> anyhow::Error {
    CallError::Retryable {
        msg: msg.into(),
        retry_after_ms: None,
    }
    .into()
}

pub(crate) fn stopped() -> anyhow::Error {
    CallError::Stopped.into()
}

/// Did this error come from a stop request?
pub(crate) fn is_stopped(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<CallError>(), Some(CallError::Stopped))
}

/// Error kinds providers send inside an otherwise healthy stream (Anthropic's
/// `error` event, OpenAI's `error` object) that mean "try again".
pub(crate) fn transient_kind(kind: &str) -> bool {
    matches!(
        kind,
        "overloaded_error"
            | "api_error"
            | "rate_limit_error"
            | "timeout_error"
            | "server_error"
            | "rate_limit_exceeded"
            | "overloaded"
    )
}

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

/// Feed each SSE `data:` payload to `f` until it returns `Ok(true)` — the
/// stream's terminal event. Checks the stop flag before every line. A stream
/// that ends without its terminal event was cut off (a proxy timeout, a
/// mobile network handoff), and saving what arrived as if it were the whole
/// answer would be wrong, so that is a retryable error, like a read failure.
pub(crate) fn read_sse(
    reader: &mut dyn BufRead,
    stop: &dyn Fn() -> bool,
    mut f: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    let mut line = String::new();
    loop {
        if stop() {
            return Err(stopped());
        }
        line.clear();
        // "stream read failed" is the response socket dying mid-SSE — seen
        // on every mobile background/resume cycle (the OS freezes the
        // process and kills its sockets) and on wifi↔cell handoffs.
        let n = reader
            .read_line(&mut line)
            .map_err(|e| retryable(format!("stream read failed: {e}")))?;
        if n == 0 {
            return Err(retryable(
                "the response stream ended before the reply was complete",
            ));
        }
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        if f(data.trim())? {
            return Ok(());
        }
    }
}

/// Throttles live progress pushes to the shell.
pub(crate) struct Live<'a> {
    notify: &'a mut dyn FnMut(Value),
    session: &'a str,
    last_text: u64,
    last_think: u64,
    pub text: String,
    pub think_chars: usize,
}

impl<'a> Live<'a> {
    pub(crate) fn new(session: &'a str, notify: &'a mut dyn FnMut(Value)) -> Self {
        Live {
            notify,
            session,
            last_text: 0,
            last_think: 0,
            text: String::new(),
            think_chars: 0,
        }
    }
    /// Streamed answer text, pushed at most every 250ms.
    pub(crate) fn text(&mut self, t: &str) {
        self.text.push_str(t);
        if crate::store::now_ms().saturating_sub(self.last_text) > 250 {
            self.flush();
        }
    }
    /// A sign of life while the model reasons. Counted per event, not per
    /// character: thinking whose text is hidden (Claude's default display)
    /// streams empty deltas, and the shell still needs to know it's working.
    pub(crate) fn thinking(&mut self, chars: usize, force: bool) {
        self.think_chars += chars;
        if force || crate::store::now_ms().saturating_sub(self.last_think) > 900 {
            (self.notify)(json!({"state": "thinking", "chars": self.think_chars,
                                 "session": self.session}));
            self.last_think = crate::store::now_ms();
        }
    }
    pub(crate) fn flush(&mut self) {
        if !self.text.is_empty() {
            (self.notify)(json!({"state": "streaming", "text": self.text,
                                 "session": self.session}));
            self.last_text = crate::store::now_ms();
        }
    }
}

/// The client for a configured provider name.
pub(crate) fn for_config(cfg: &AiConfig) -> Result<Box<dyn Provider>> {
    Ok(match cfg.provider.as_str() {
        // z.ai coding-plan keys only work on the Anthropic-compatible
        // endpoint (paas/v4 rejects them with "insufficient balance"),
        // and API-plan keys work there too — so zai always speaks
        // Anthropic wire format.
        "anthropic" | "zai" => Box::new(anthropic::Anthropic),
        // OpenRouter, a server on this machine (Ollama, LM Studio, llama.cpp's
        // server, vLLM, ...) and any other OpenAI-compatible endpoint all
        // implement the chat/completions shape.
        "openrouter" | "local" | "openai-compatible" => Box::new(openai_chat::OpenAiChat),
        // Direct OpenAI gets their newer Responses API.
        "openai" => Box::new(openai_responses::OpenAiResponses),
        other => bail!(
            "unknown provider '{other}' (use anthropic, zai, openrouter, openai, \
             openai-compatible or local)"
        ),
    })
}

/// One provider call, no retries.
pub(crate) fn call_provider(app: &Arc<App>, cfg: &AiConfig, req: &Request) -> Result<Reply> {
    for_config(cfg)?.call(app, cfg, req)
}

/// Backoff between retries, before jitter.
const RETRY_DELAYS_MS: [u64; 5] = [1_000, 3_000, 8_000, 20_000, 40_000];
/// Most time spent waiting between retries of one call. A `Retry-After`
/// that would push past this gives up instead of stalling the chat.
const RETRY_BUDGET_MS: u64 = 120_000;

/// The wait before retry number `attempt` (0-based): the backoff step plus up
/// to 25% jitter, or the server's own `Retry-After` when that is longer.
pub(crate) fn retry_delay(attempt: usize, retry_after_ms: Option<u64>) -> Option<u64> {
    let base = *RETRY_DELAYS_MS.get(attempt)?;
    let jittered = base + fastrand::u64(0..=base / 4);
    Some(retry_after_ms.unwrap_or(0).max(jittered))
}

/// Retry transient provider failures (rate limits, overload, 5xx, dropped
/// or truncated streams) with jittered backoff, within a total wait budget.
/// A stop request ends the wait early.
pub(crate) fn call_provider_retrying(
    app: &Arc<App>,
    ctx: &RunCtx,
    cfg: &AiConfig,
    req: &Request,
) -> Result<Reply> {
    let provider = for_config(cfg)?;
    let mut waited = 0u64;
    for attempt in 0.. {
        let e = match provider.call(app, cfg, req) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let Some(CallError::Retryable {
            msg,
            retry_after_ms,
        }) = e.downcast_ref::<CallError>()
        else {
            return Err(e);
        };
        let Some(wait) = retry_delay(attempt, *retry_after_ms) else {
            return Err(e);
        };
        if ctx.stopped() || waited + wait > RETRY_BUDGET_MS {
            return Err(e);
        }
        app.notify(
            "ai",
            json!({"state": "retry", "session": ctx.session,
                   "message": format!("{msg} — retrying in {}s", wait.div_ceil(1000))}),
        );
        let _ = app.wait_until(wait, 250, || ctx.stopped());
        waited += wait;
        if ctx.stopped() {
            return Err(stopped());
        }
    }
    unreachable!()
}

/// Overall request timeout for a non-streamed provider call. This is the whole
/// blocking POST — nothing comes back until the model is completely done — so
/// it must cover a full generation. A local server can be genuinely slow (a
/// big model on CPU may emit only a few tokens/sec), where a long answer takes
/// many minutes; 5 minutes was far too tight and cut off any non-trivial
/// request. Local therefore gets a very generous ceiling; cloud providers keep
/// the tighter one. `UAPP_AI_TIMEOUT_SECS` overrides both.
pub(crate) fn ai_timeout(cfg: &AiConfig) -> u64 {
    std::env::var("UAPP_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(match cfg.provider.as_str() {
            "local" | "openai-compatible" => 3600,
            _ => 300,
        })
}

/// The server's retry hint: `retry-after-ms`, or `retry-after` in seconds
/// (the HTTP-date form is rare from APIs and ignored).
fn retry_after(headers: &[(String, String)]) -> Option<u64> {
    let get = |k: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.trim())
    };
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.parse::<f64>().ok()) {
        return Some(ms.max(0.0) as u64);
    }
    get("retry-after")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|s| (s.max(0.0) * 1000.0) as u64)
}

/// One place to turn a failed provider HTTP response into an error, so the
/// message shape ("provider returned 429: …") is identical on every path.
/// Rate limits, overload and server errors come back retryable, carrying the
/// server's `Retry-After`.
pub(crate) fn http_error(status: u16, headers: &[(String, String)], body: &str) -> anyhow::Error {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or(v["message"].as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    let msg = format!("provider returned {status}: {msg}");
    if matches!(status, 408 | 409 | 425 | 429) || status >= 500 {
        CallError::Retryable {
            msg,
            retry_after_ms: retry_after(headers),
        }
        .into()
    } else {
        anyhow!(msg)
    }
}

pub(crate) fn provider_error(resp: crate::net::Resp) -> anyhow::Error {
    let status = resp.status;
    let headers = resp.headers.clone();
    let body = resp.into_string().unwrap_or_default();
    http_error(status, &headers, &body)
}

/// Open a streaming POST. Transport failures retry; HTTP errors are
/// classified by [`http_error`].
pub(crate) fn open_stream(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    idle_timeout: u64,
) -> Result<crate::net::Resp> {
    let resp = crate::net::request(
        "POST",
        url,
        headers,
        Some(&body.to_string()),
        20,
        idle_timeout,
    )
    .map_err(|e| retryable(format!("provider request failed: {e}")))?;
    if resp.status >= 400 {
        return Err(provider_error(resp));
    }
    Ok(resp)
}

/// A non-streamed POST, returning the parsed JSON body. Nothing arrives until
/// the model is done, so natively the request runs on its own thread and this
/// one polls the stop flag meanwhile: Stop answers at once, and the abandoned
/// request finishes in the background. (The browser build can't spawn
/// threads; there the stop flag is checked once the reply is in.)
pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: u64,
    stop: &dyn Fn() -> bool,
) -> Result<Value> {
    let (_, text) = post(url, headers, body, timeout, stop)?;
    serde_json::from_str(&text).map_err(|e| anyhow!("provider sent unreadable JSON: {e}"))
}

/// [`post_json`] without the parsing: `(content type, body)`, for a caller
/// that copes with a server streaming although it was asked not to.
pub(crate) fn post(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: u64,
    stop: &dyn Fn() -> bool,
) -> Result<(String, String)> {
    type Posted = Result<(u16, Vec<(String, String)>, String, String)>;
    let run = {
        let url = url.to_string();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = body.to_string();
        move || -> Posted {
            let hdrs: Vec<(&str, &str)> = headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let resp = crate::net::request("POST", &url, &hdrs, Some(&body), 20, timeout)
                .map_err(|e| retryable(format!("provider request failed: {e}")))?;
            let status = resp.status;
            let headers = resp.headers.clone();
            let ctype = resp.content_type.clone();
            let text = resp
                .into_string()
                .map_err(|e| retryable(format!("provider request failed: {e}")))?;
            Ok((status, headers, ctype, text))
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    let posted: Posted = {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Posted>(1);
        std::thread::spawn(move || {
            let _ = tx.send(run());
        });
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(r) => break r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if stop() {
                        return Err(stopped());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(retryable("provider request failed: worker ended"));
                }
            }
        }
    };
    #[cfg(target_arch = "wasm32")]
    let posted: Posted = run();
    let (status, headers, ctype, text) = posted?;
    if stop() {
        return Err(stopped());
    }
    if status >= 400 {
        return Err(http_error(status, &headers, &text));
    }
    Ok((ctype, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_needs_its_terminal_event() {
        let never = || false;
        let mut seen = Vec::new();
        let mut r = std::io::Cursor::new(b"data: 1\n\ndata: 2\n\n".to_vec());
        let e = read_sse(&mut r, &never, |d| {
            seen.push(d.to_string());
            Ok(d == "3")
        })
        .unwrap_err();
        assert_eq!(seen, ["1", "2"]);
        assert!(matches!(
            e.downcast_ref::<CallError>(),
            Some(CallError::Retryable { .. })
        ));
        let mut r = std::io::Cursor::new(b"event: x\ndata: 3\r\n\r\ndata: 4\n".to_vec());
        read_sse(&mut r, &never, |d| Ok(d == "3")).unwrap();
    }

    #[test]
    fn sse_stops_between_lines() {
        let mut r = std::io::Cursor::new(b"data: 1\n".to_vec());
        let e = read_sse(&mut r, &|| true, |_| Ok(false)).unwrap_err();
        assert!(is_stopped(&e));
    }

    #[test]
    fn http_errors_are_classified() {
        let hdr = vec![("retry-after".to_string(), "7".to_string())];
        let e = http_error(429, &hdr, r#"{"error":{"message":"slow down"}}"#);
        match e.downcast_ref::<CallError>() {
            Some(CallError::Retryable {
                msg,
                retry_after_ms,
            }) => {
                assert_eq!(msg, "provider returned 429: slow down");
                assert_eq!(*retry_after_ms, Some(7000));
            }
            _ => panic!("429 should retry"),
        }
        assert!(
            http_error(529, &[], "overloaded")
                .downcast_ref::<CallError>()
                .is_some()
        );
        let e = http_error(400, &[], r#"{"error":{"message":"bad"}}"#);
        assert!(e.downcast_ref::<CallError>().is_none());
        assert_eq!(e.to_string(), "provider returned 400: bad");
    }

    #[test]
    fn retry_delays_grow_honour_retry_after_and_run_out() {
        let d0 = retry_delay(0, None).unwrap();
        assert!((1000..=1250).contains(&d0));
        assert_eq!(retry_delay(0, Some(30_000)), Some(30_000));
        assert!(retry_delay(4, None).unwrap() >= 40_000);
        assert_eq!(retry_delay(5, None), None);
    }
}
