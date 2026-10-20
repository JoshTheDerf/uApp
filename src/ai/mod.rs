//! AI assistant: provider clients (Anthropic native; z.ai GLM and OpenRouter
//! via the OpenAI-compatible chat/completions shape) plus the tool loop.
//! Every tool call goes through the same JSON-RPC dispatch the user's shell
//! and the embedded app use — the model has exactly the user's powers.

mod agents;
mod approval;
mod compact;
mod history;
mod prompt;
mod providers;
mod run;
mod toolset;

pub use agents::{agent_run, agent_send};
pub use approval::{app_approval_gate, ask_user};
pub use compact::{compact_session, COMPACT_PROMPT};
pub use run::{run, unanswered};
pub use toolset::run_tool;

use anyhow::{anyhow, bail, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::app::App;
use crate::store;

pub(crate) const MAX_ITERATIONS: usize = 40;
pub(crate) const TOOL_RESULT_LIMIT: usize = 30_000;
/// Byte budget for tool_result content in the OUTGOING messages (newest-first).
/// Stored history is never touched — only what we send this turn.
pub(crate) const OUTGOING_TOOL_BUDGET: usize = 96 * 1024;
/// Serialized-history size that triggers automatic compaction at run start,
/// and the hard ceiling for outgoing messages.
pub(crate) const AUTO_COMPACT_BYTES: usize = 400 * 1024;
/// The same budget for a local server, which typically runs a far smaller
/// context window (8k-32k tokens) than a cloud model. llama.cpp/Ollama drop or
/// error past that window, and a 40-iteration run grows well beyond it. At
/// roughly 4 bytes per token, 96KB lands near 24k tokens, which leaves room
/// for the reply inside a 32k window.
pub(crate) const LOCAL_COMPACT_BYTES: usize = 96 * 1024;
/// Digest cap for a `fork`-mode sub-agent's copy of the parent conversation.
pub(crate) const FORK_DIGEST_BYTES: usize = 32 * 1024;

/// One assistant run (or one sub-agent loop inside it).
#[derive(Clone)]
pub struct RunCtx {
    /// The chat session this loop reads and writes.
    pub session: String,
    /// The top-level run: equal to `session` for a user chat, the parent chat
    /// session for a sub-agent loop. It is the key in `app.ai_runs` and the
    /// session stamped on approval prompts, so `ai.stop {session}` reaches
    /// nested loops too.
    pub root: String,
    /// Shared with every nested loop: one stop flag per run tree.
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    /// 0 = user-facing chat, 1 = sub-agent (agent tools are removed there,
    /// which bounds recursion to depth 1).
    pub depth: usize,
}

impl RunCtx {
    pub fn root(session: String, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        RunCtx {
            root: session.clone(),
            session,
            stop,
            depth: 0,
        }
    }
    pub(crate) fn child(&self, session: String) -> Self {
        RunCtx {
            session,
            root: self.root.clone(),
            stop: self.stop.clone(),
            depth: self.depth + 1,
        }
    }
    pub(crate) fn stopped(&self) -> bool {
        // wasm: the worker is blocked inside this run, so ai.stop can't arrive
        // as an RPC — the shell raises a SharedArrayBuffer flag instead.
        #[cfg(target_arch = "wasm32")]
        if crate::wasm::stop_requested() {
            return true;
        }
        self.stop.load(Ordering::Relaxed)
    }
}

thread_local! {
    /// The loop running on this thread, so a tool (agent_run/agent_send) can
    /// see which session/run it belongs to. Sub-agent loops are synchronous on
    /// the same thread, so this is always the innermost loop.
    static CURRENT_RUN: std::cell::RefCell<Option<RunCtx>> = const { std::cell::RefCell::new(None) };
}

/// Restores the enclosing loop's ctx when a nested loop returns.
pub(crate) struct CtxGuard(Option<RunCtx>);
impl CtxGuard {
    pub(crate) fn enter(ctx: &RunCtx) -> CtxGuard {
        CtxGuard(CURRENT_RUN.with(|c| c.borrow_mut().replace(ctx.clone())))
    }
}
impl Drop for CtxGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT_RUN.with(|c| *c.borrow_mut() = prev);
    }
}

/// The loop a tool call belongs to. A tool invoked outside any run (a human
/// click through tools.call, or an app calling uapp.tool) gets a detached
/// top-level ctx on the main session.
pub(crate) fn current_ctx() -> RunCtx {
    CURRENT_RUN.with(|c| c.borrow().clone()).unwrap_or_else(|| {
        RunCtx::root(
            "main".to_string(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    })
}

pub struct AiConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
    /// None = provider default. For zai that means OMITTING max_tokens
    /// entirely: when the field is present, z.ai's Anthropic layer hands the
    /// whole budget to thinking and GLM-5.3 happily reasons until the cap
    /// with zero text/tool output (verified: 16384 -> 68k chars of pure
    /// thinking, nothing usable; omitted -> 48s, tool call, sane reasoning).
    /// Every working GLM client (../chat, Claude Code) omits max_tokens.
    pub max_tokens: Option<u32>,
    /// Ask the provider to stream. None = per-provider default: OFF for z.ai
    /// (its Anthropic-compatible endpoint drops tool-call blocks from streamed
    /// responses, so streamed GLM turns look like endless thinking and never
    /// act), ON for real Anthropic, OpenAI and custom servers. Override via
    /// config `ai.stream`.
    pub stream: Option<bool>,
    /// How hard a current Claude model thinks: `output_config.effort`
    /// ("low" … "max"). None = "medium", the recommended start for agentic
    /// work. Config `ai.effort`.
    pub effort: Option<String>,
    /// The model's context window in tokens, for a server whose window we
    /// can't know (local, OpenAI-compatible). Sizes the history budget.
    /// Config `ai.context_tokens`.
    pub context_tokens: Option<u64>,
}

/// GLM-5.3 via z.ai's Anthropic-compatible endpoint treats a present
/// max_tokens as the thinking budget: given 16384 it reasoned for 68k chars
/// and returned NOTHING usable (and `thinking.budget_tokens` / `disabled`
/// are ignored by that layer). Omitting max_tokens entirely — what every
/// other working GLM client does — gets z.ai's own sane default and normal
/// behavior. So zai sends no cap unless the user sets `ai.max_tokens`.
///
/// Current Claude models always think, and the thinking counts against
/// max_tokens, so a streamed Claude turn gets room for both (64K is what
/// long agentic turns need). A non-streamed request keeps a smaller cap, so
/// the blocking request finishes inside its timeout.
pub(crate) fn default_max_tokens(cfg: &AiConfig, stream: bool) -> u32 {
    match cfg.provider.as_str() {
        "anthropic" if stream => 64_000,
        // A local server is usually a quantized model on consumer hardware
        // emitting a few tokens/sec, where 16384 tokens is over an hour of
        // generation for a single turn - and a thinking model will happily
        // spend all of it reasoning. Keep the cap low enough that a turn
        // actually finishes; raise it with `ai.max_tokens` in Settings.
        "local" => 2048,
        "openai-compatible" => 8192,
        _ => 16384,
    }
}

/// Serialized-history budget for this provider: compaction trigger and the
/// outgoing hard-trim ceiling. A configured context size wins, at roughly
/// 3 bytes per token after setting aside room for the reply.
pub(crate) fn compact_bytes(cfg: &AiConfig) -> usize {
    if let Some(tokens) = cfg.context_tokens {
        let reply = cfg.max_tokens.unwrap_or(default_max_tokens(cfg, true)) as u64;
        return (tokens.saturating_sub(reply) as usize * 3).max(16 * 1024);
    }
    match cfg.provider.as_str() {
        "local" | "openai-compatible" => LOCAL_COMPACT_BYTES,
        _ => AUTO_COMPACT_BYTES,
    }
}

pub(crate) fn load_config(app: &Arc<App>) -> Result<AiConfig> {
    let cfg = {
        let eng = app.eng();
        store::config_get(&eng.db, "ai")?
    }
    .ok_or_else(|| anyhow!("no AI provider configured — open Settings in the shell"))?;
    let provider = cfg["provider"].as_str().unwrap_or("").to_string();
    if provider.is_empty() {
        bail!("AI config missing provider");
    }
    let max_tokens = cfg["max_tokens"].as_u64().map(|v| v as u32);
    Ok(AiConfig {
        provider,
        api_key: crate::secrets::ai_key(app, &cfg),
        model: cfg["model"].as_str().unwrap_or("").to_string(),
        base_url: cfg["base_url"].as_str().map(|s| s.to_string()),
        max_tokens,
        stream: cfg["stream"].as_bool(),
        effort: cfg["effort"].as_str().map(String::from),
        context_tokens: cfg["context_tokens"].as_u64().filter(|t| *t > 0),
    })
}

/// Plan mode = read-only toolset: gated builtins can write, and agents can
/// write inside their own loop, so both are withheld.
pub(crate) fn plan_mode(app: &Arc<App>) -> bool {
    crate::app::lock(&app.ai_mode).as_str() == "plan"
}
