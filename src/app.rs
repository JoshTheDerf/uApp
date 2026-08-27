//! The shared, transport-agnostic state of one open .uapp: the engine plus
//! every registry the RPC layer and the AI loop need (runs, approvals,
//! questions, app actions, eval contexts, console buffer).
//!
//! Moved out of `server.rs` so the browser (wasm32) build — which has no HTTP
//! server at all — can share the exact same core. Transports differ per
//! platform: native serves WebSocket connections (`server.rs`); wasm forwards
//! notifications to JS and routes page invocations over the worker bridge
//! (`wasm.rs`).

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::broadcast;

use crate::engine::{ChangeSet, Engine};

/// A gated tool call waiting for the user's decision (manual mode).
/// Lives only on the device that started the AI run.
pub struct PendingApproval {
    pub name: String,
    /// The run (top-level chat session) whose loop is blocked on this prompt —
    /// `ai.stop {session}` only resolves its own prompts.
    pub session: String,
    pub input: Value,
    pub created: u64,
    /// Why this call needs an explicit prompt even in auto mode (e.g. it
    /// attaches a local database file or contacts a local network address).
    pub note: Option<String>,
    pub tx: std::sync::mpsc::SyncSender<(bool, bool)>, // (allow, always)
}

/// A question the assistant asked the user (ask_user tool): the run blocks
/// until the user answers, the prompt times out, or the run is stopped.
/// Lives only on the device that started the AI run, like PendingApproval.
pub struct PendingQuestion {
    /// 1-4 questions, each `{question, header?, options: [{label, description?}],
    /// multiSelect?}` — a free-form "Other" answer is always available per
    /// question, so the user is never boxed in.
    pub questions: Value,
    /// The run's ROOT session — the chat where the prompt is shown/answered.
    pub session: String,
    pub created: u64,
    pub tx: std::sync::mpsc::SyncSender<Value>, // {"answers":[{choices:[..],other:".."}]}
}

/// One live AI run. `stop` is an Arc so the run's own loop (and every
/// sub-agent loop below it) can watch the same flag after `ai.stop`.
pub struct RunHandle {
    pub stop: std::sync::Arc<AtomicBool>,
    pub started: u64,
}

/// A business-logic action registered by the running app page via
/// uapp.action(). The handler lives in the browser; the server routes
/// invocations (from the AI tool loop) to the registering connection.
#[derive(Clone)]
pub struct AppAction {
    pub description: String,
    pub schema: Value,
    pub readonly: bool,
    pub conn: u64,
}

/// One console line or uncaught error forwarded from the live app iframe over
/// `/ws` (method `log.write`). Kept in a bounded ring buffer on `App` so the
/// chat-tab AI can read the app's real runtime output via the `read_console`
/// tool — no devtools and no debug build needed.
#[derive(Clone)]
pub struct ConsoleEntry {
    /// Monotonic id, so the AI (and the auto-alert) can ask "what's new".
    pub seq: u64,
    /// Which page load produced this. Bumps on every iframe (re)load, so the
    /// AI can filter to just the latest reload after an edit.
    pub gen: u64,
    /// "log" | "info" | "warn" | "error" | "debug".
    pub level: String,
    pub text: String,
    pub ts: i64,
}

/// Newest N console lines kept for the AI. A busy app can log a lot; this is a
/// debugging aid, not a full log, so an older-than-N line just falls off.
const CONSOLE_CAP: usize = 500;
/// How long `reload_app` waits for the reloaded page to finish loading.
pub const RELOAD_WAIT_MS: u64 = 10_000;
/// How long an `app__*` call waits for a page that is still bootstrapping
/// (right after reload_app) to register the action before giving up.
pub const ACTION_REGISTER_WAIT_MS: u64 = 8_000;

/// Pushes one JSON text message to a connected page; `false` once it is gone.
pub type ConnSender = std::sync::Arc<dyn Fn(String) -> bool + Send + Sync>;

/// Knobs of the PUBLIC ("hosted site") server mode — `uapp serve`. `None` on
/// an `App` means the private desktop server, where every route needs the
/// session token. Defaults are the conservative ones.
#[derive(Clone, Copy, Debug)]
pub struct PublicOpts {
    /// Send COOP/COEP so pages are cross-origin isolated from birth (no
    /// service-worker stamp, no boot reload). Costs cross-origin `<iframe>`
    /// embeds — YouTube, CodePen and friends stop rendering unless they opt
    /// in — so it is off unless the site wants the SharedArrayBuffer bridge
    /// in the editing chrome.
    pub coi: bool,
    /// Expose `GET /site.uapp` (the sanitized archive the in-browser engine
    /// boots from). Off means "plain static site, no editing chrome".
    pub archive: bool,
    /// Include `data/` in that archive — the content sources a site's build
    /// pipeline reads, so visitors can re-run the build in their browser.
    /// Anything included is PUBLIC: the archive is downloaded by every visitor.
    /// Serving `data/` over HTTP stays blocked either way. Also decides what a
    /// publish (`PUT /site.uapp`) replaces: with it, `data/` comes from the
    /// uploaded copy; without it, the server's `data/` is kept.
    pub export_data: bool,
    /// max-age for non-HTML assets. HTML is always revalidated.
    pub asset_max_age: u32,
}

impl Default for PublicOpts {
    fn default() -> Self {
        Self { coi: false, archive: true, export_data: false, asset_max_age: 3600 }
    }
}

/// The browser build (`dist-web`: wasm engine + boot + service worker) a
/// public site serves so its pages get the "Edit this site" chrome.
pub struct Chrome {
    /// Assets keyed by the root path they are served at (`/boot.js`, …).
    ///
    /// Served at the ROOT rather than under a prefix for two reasons: a
    /// service worker's scope is its own directory, and `/sw.js` is the only
    /// place it can be registered from to control `/app/*`; and boot.js
    /// resolves everything it loads relative to its own URL. A site file with
    /// one of these names is shadowed — the same trade the shell routes make.
    pub bundle: HashMap<String, Vec<u8>>,
    /// Short content hash, appended as `?v=` to the injected script tag.
    /// Cloudflare (and other CDNs) rewrite Cache-Control on .js to hours of
    /// browser caching, so without it a deploy leaves visitors on the previous
    /// chrome until it expires.
    pub version: String,
}

/// One built `/site.uapp`: bytes, ETag, newest sqlar mtime (unix seconds).
pub struct PublicArchive {
    pub bytes: Vec<u8>,
    /// The one identity of a served site version: what `/site.uapp` returns
    /// as ETag, what a publish sends back as If-Match, and what the browser
    /// copy remembers to know whether the server has moved on.
    pub etag: String,
}

/// A dropped template .uapp held between "here's what this would change" and
/// the user pressing Update.
pub struct StagedTemplate {
    pub ts: u64,
    /// The file name it was dropped as — shown in the confirmation dialog.
    pub label: String,
    pub bytes: Vec<u8>,
    /// Master password, when the source is an encrypted full app copy.
    pub password: Option<String>,
}

pub struct App {
    pub engine: Mutex<Engine>,
    pub events: broadcast::Sender<Value>,
    pub token: String,
    pub name: String,
    /// Live AI runs, keyed by chat session id: one run per session, runs on
    /// different sessions go in parallel. A run's stop flag is shared with any
    /// sub-agent loop it starts, so stopping a session stops its whole tree.
    pub ai_runs: Mutex<HashMap<String, RunHandle>>,
    /// True for a scratch app opened with no file (lives only in a temp file).
    /// The shell shows a "not saved" banner + a Download button.
    pub unsaved: AtomicBool,
    /// "auto" | "manual" — set per chat.send from the shell's toggle.
    pub ai_mode: Mutex<String>,
    pub pending: Mutex<HashMap<String, PendingApproval>>,
    /// Open ask_user prompts, keyed by question id.
    pub questions: Mutex<HashMap<String, PendingQuestion>>,
    /// Session-scoped "always allow" tool names (this device only).
    pub always_allow: Mutex<std::collections::HashSet<String>>,
    /// Connected shell/app WebSocket clients. When this stays at zero past
    /// the linger period the server exits (last tab closed).
    pub clients: std::sync::atomic::AtomicUsize,
    /// Bound port, set right after the listener binds. Used to name the auth
    /// cookie per port — multiple apps share 127.0.0.1 and would otherwise
    /// overwrite each other's `uapp_t` cookie (seen as "missing or bad
    /// token" on iframe reloads).
    pub port: std::sync::atomic::AtomicU16,
    /// Connected pages, by connection id: how to push a message to each one.
    /// Native: a WebSocket's outbound queue; wasm: a postMessage to the iframe
    /// boot.js gave that id. Everything above this map — actions, contexts,
    /// run_js, reload_app — is transport-neutral.
    pub conns: Mutex<HashMap<u64, ConnSender>>,
    /// App-registered actions: name -> definition + owning connection.
    pub actions: Mutex<HashMap<String, AppAction>>,
    /// In-flight page invocations (actions, run_js): `None` until the page's
    /// `*.result` message lands, then its params. Polled by [`App::page_call`].
    pub invokes: Mutex<HashMap<String, Option<Value>>>,
    /// Files dropped onto the native window, keyed by a one-shot id: the page
    /// is told the id and the file names, never the OS paths, and asks the
    /// server to ingest them by id. Paths therefore always come from the real
    /// OS drop and never from something the page made up.
    pub drops: Mutex<HashMap<String, (u64, Vec<std::path::PathBuf>)>>,
    /// A template .uapp that was dropped on the window and inspected, waiting
    /// for the user to confirm the update (see `crate::template`). The bytes
    /// stay server-side under a one-shot token — the page only ever sees the
    /// token and the plan — and expire, like `drops`.
    pub templates: Mutex<HashMap<String, StagedTemplate>>,
    /// JS execution contexts announced by connected pages via ctx.register:
    /// "app" (the live app iframe) and "scratchpad" (the shell's hidden
    /// scratch iframe). Push-ordered; the newest still-connected registrant
    /// wins, so a reopened window takes over cleanly.
    pub contexts: Mutex<Vec<(String, u64)>>,
    /// Connections whose page has fired its `load` event (`ctx.loaded`).
    /// `invoke_reload` waits for a NEW "app" connection to appear here.
    pub ctx_loaded: Mutex<std::collections::HashSet<u64>>,
    /// Ring buffer of console output / uncaught errors forwarded from the live
    /// app iframe, read by the AI via `read_console`. See [`ConsoleEntry`].
    pub console: Mutex<VecDeque<ConsoleEntry>>,
    /// Next console seq to hand out.
    pub console_seq: AtomicU64,
    /// Current page-load generation (bumped when a page announces `reset`).
    pub console_gen: AtomicU64,
    /// Highest error/warn seq already surfaced to the AI (via read_console or
    /// the auto-alert), so the alert fires only for genuinely new problems.
    pub console_ack: AtomicU64,
    /// Set when this server is a public website (`uapp serve`): unauthenticated
    /// requests get the site's `app/` pages; the token still unlocks everything
    /// else. `None` is the private desktop server.
    pub public: Option<PublicOpts>,
    /// The browser build served alongside a public site (`--chrome`).
    pub chrome: Option<Chrome>,
    /// `/site.uapp`, built lazily and kept until the engine's write counter
    /// moves (`Engine::writes`), so an unchanged site never rebuilds it and a
    /// publish is visible on the very next request.
    pub archive_cache: Mutex<Option<(u64, std::sync::Arc<PublicArchive>)>>,
}

impl App {
    /// Every field an `App` starts with; the transports only differ in what
    /// they set afterwards (`public`, `chrome`, `clients`).
    pub fn new(engine: Engine, token: String, unsaved: bool) -> App {
        let name = crate::store::meta_get(&engine.db, "name")
            .ok()
            .flatten()
            .unwrap_or_else(|| "uapp".into());
        let (events, _) = broadcast::channel(1024);
        App {
            engine: Mutex::new(engine),
            events,
            token,
            name,
            ai_runs: Mutex::new(HashMap::new()),
            unsaved: AtomicBool::new(unsaved),
            ai_mode: Mutex::new("auto".into()),
            pending: Mutex::new(HashMap::new()),
            questions: Mutex::new(HashMap::new()),
            always_allow: Mutex::new(std::collections::HashSet::new()),
            clients: std::sync::atomic::AtomicUsize::new(0),
            port: std::sync::atomic::AtomicU16::new(0),
            conns: Mutex::new(HashMap::new()),
            actions: Mutex::new(HashMap::new()),
            invokes: Mutex::new(HashMap::new()),
            drops: Mutex::new(HashMap::new()),
            templates: Mutex::new(HashMap::new()),
            contexts: Mutex::new(Vec::new()),
            ctx_loaded: Mutex::new(std::collections::HashSet::new()),
            console: Mutex::new(VecDeque::new()),
            console_seq: AtomicU64::new(0),
            console_gen: AtomicU64::new(0),
            console_ack: AtomicU64::new(0),
            public: None,
            chrome: None,
            archive_cache: Mutex::new(None),
        }
    }
}

impl App {
    /// Best-effort cleanup before the process exits: a final rescue snapshot
    /// if anything changed, and drop this file's registry addr so future opens
    /// stop dialing a dead server. Every exit path — quit from the shell,
    /// last-client linger, ctrl-c, the native window closing — funnels through
    /// here. Idempotent.
    pub fn graceful_cleanup(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut eng = self.engine.lock().unwrap();
            eng.snapshot_if_dirty();
            crate::registry::remove_addr(&eng.path);
        }
    }

    /// Is any session's assistant run live? (`app.info.aiBusy`, kept global for
    /// compatibility with clients that predate per-session runs.)
    pub fn ai_busy_any(&self) -> bool {
        !self.ai_runs.lock().unwrap().is_empty()
    }

    // ---- console forwarding (live app iframe -> AI) ------------------------

    /// Record one forwarded console line. A `reset` message (sent by the
    /// forwarder on page load) opens a new generation instead of logging.
    pub fn console_push(&self, level: &str, text: &str) {
        if level == "reset" {
            self.console_gen.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let seq = self.console_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = ConsoleEntry {
            seq,
            gen: self.console_gen.load(Ordering::Relaxed),
            level: level.to_string(),
            // A single runaway line shouldn't be able to pin megabytes.
            text: text.chars().take(8000).collect(),
            ts: crate::store::now_ms() as i64,
        };
        let mut buf = self.console.lock().unwrap();
        buf.push_back(entry);
        while buf.len() > CONSOLE_CAP {
            buf.pop_front();
        }
    }

    /// Read recent console entries for the AI. `min_error`/`only_latest`/`limit`
    /// filter; reading acks everything returned so the auto-alert won't repeat
    /// it. Returns `{entries, dropped, latest_gen}`.
    pub fn console_read(&self, only_errors: bool, only_latest: bool, limit: usize) -> Value {
        let latest_gen = self.console_gen.load(Ordering::Relaxed);
        let buf = self.console.lock().unwrap();
        let mut rows: Vec<&ConsoleEntry> = buf
            .iter()
            .filter(|e| !only_latest || e.gen == latest_gen)
            .filter(|e| !only_errors || e.level == "error" || e.level == "warn")
            .collect();
        let total = rows.len();
        if rows.len() > limit {
            rows = rows.split_off(rows.len() - limit);
        }
        let max_seq = buf.iter().map(|e| e.seq).max().unwrap_or(0);
        let entries: Vec<Value> = rows
            .iter()
            .map(|e| json!({"level": e.level, "text": e.text, "gen": e.gen, "ts": e.ts}))
            .collect();
        drop(buf);
        // Ack up to the newest line that EXISTS (not just what we returned), so
        // a filtered/limited read still silences the alert for older noise.
        self.console_ack.fetch_max(max_seq, Ordering::Relaxed);
        json!({
            "entries": entries,
            "dropped": total.saturating_sub(entries.len()),
            "latest_gen": latest_gen,
        })
    }

    /// A one-line nudge for the AI when the live app has produced anything new
    /// since it last looked — errors, warnings, or plain log lines — or `None`
    /// when nothing new has appeared (so a quiet console is never mentioned).
    /// Marks the new lines acked so it fires once per batch, not every turn.
    pub fn console_alert(&self) -> Option<String> {
        let ack = self.console_ack.load(Ordering::Relaxed);
        let buf = self.console.lock().unwrap();
        // A line the AI has already been shown (same text, e.g. a framework
        // warning repeated on every page load) is not news again.
        let seen: std::collections::HashSet<&str> =
            buf.iter().filter(|e| e.seq <= ack).map(|e| e.text.as_str()).collect();
        let mut errs = 0usize;
        let mut warns = 0usize;
        let mut others = 0usize;
        let mut top = 0u64;
        for e in buf.iter().filter(|e| e.seq > ack) {
            if seen.contains(e.text.as_str()) {
                top = top.max(e.seq);
                continue;
            }
            match e.level.as_str() {
                "error" => errs += 1,
                "warn" => warns += 1,
                _ => others += 1,
            }
            top = top.max(e.seq);
        }
        drop(buf);
        // Nothing new at all: stay silent — don't report "console status".
        if errs == 0 && warns == 0 && others == 0 {
            self.console_ack.fetch_max(top, Ordering::Relaxed);
            return None;
        }
        self.console_ack.fetch_max(top, Ordering::Relaxed);
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut parts = Vec::new();
        if errs > 0 {
            parts.push(format!("{errs} error{}", plural(errs)));
        }
        if warns > 0 {
            parts.push(format!("{warns} warning{}", plural(warns)));
        }
        if others > 0 {
            parts.push(format!("{others} log line{}", plural(others)));
        }
        Some(format!(
            "\n\n[console] {} in the live app since you last checked — call read_console to see the messages{}.",
            parts.join(", "),
            if errs > 0 { " and stack traces" } else { "" }
        ))
    }

    /// Sessions with a live run, oldest first.
    pub fn ai_sessions(&self) -> Vec<String> {
        let runs = self.ai_runs.lock().unwrap();
        let mut v: Vec<(u64, String)> =
            runs.iter().map(|(s, h)| (h.started, s.clone())).collect();
        v.sort();
        v.into_iter().map(|(_, s)| s).collect()
    }

    /// Raise the stop flag of one session's run (None = every run) and resolve
    /// its blocked approval prompts as denied so the loop unblocks now.
    pub fn ai_stop(&self, session: Option<&str>) {
        // Runs are keyed by their ROOT session: stopping an agent conversation
        // means stopping the parent run that spawned it (agents are depth-1).
        let session = session.map(|s| {
            let runs = self.ai_runs.lock().unwrap();
            if runs.contains_key(s) {
                return s.to_string();
            }
            drop(runs);
            let eng = self.engine.lock().unwrap();
            crate::store::query(
                &eng.db,
                "SELECT parent FROM uapp_sessions WHERE id = ?1 AND kind = 'agent'",
                &[serde_json::json!(s)],
            )
            .ok()
            .and_then(|r| r["rows"][0][0].as_str().map(|p| p.to_string()))
            .unwrap_or_else(|| s.to_string())
        });
        let session = session.as_deref();
        for (id, h) in self.ai_runs.lock().unwrap().iter() {
            if session.map_or(true, |s| s == id) {
                h.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let drained: Vec<_> = {
            let mut p = self.pending.lock().unwrap();
            let ids: Vec<String> = p
                .iter()
                .filter(|(_, e)| session.map_or(true, |s| s == e.session))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter().filter_map(|id| p.remove(&id)).collect()
        };
        for e in drained {
            let _ = e.tx.try_send((false, false));
        }
        self.notify_pending();
        // Open ask_user prompts in the stopped session resolve as unanswered
        // so the blocked tool call returns immediately.
        let dropped: Vec<_> = {
            let mut q = self.questions.lock().unwrap();
            let ids: Vec<String> = q
                .iter()
                .filter(|(_, e)| session.map_or(true, |s| s == e.session))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter().filter_map(|id| q.remove(&id)).collect()
        };
        for e in dropped {
            let _ = e.tx.try_send(json!({"stopped": true}));
        }
        self.notify_questions();
    }

    pub fn questions_list(&self) -> Value {
        let q = self.questions.lock().unwrap();
        let mut list: Vec<Value> = q
            .iter()
            .map(|(id, e)| json!({"id": id, "questions": e.questions,
                                  "created": e.created, "session": e.session}))
            .collect();
        list.sort_by_key(|v| v["created"].as_u64().unwrap_or(0));
        Value::Array(list)
    }
    pub fn notify_questions(&self) {
        let list = self.questions_list();
        self.notify("question", json!({"pending": list}));
    }

    pub fn pending_list(&self) -> Value {
        let p = self.pending.lock().unwrap();
        let mut list: Vec<Value> = p
            .iter()
            .map(|(id, e)| json!({"id": id, "name": e.name, "input": e.input,
                                  "created": e.created, "note": e.note, "session": e.session}))
            .collect();
        list.sort_by_key(|v| v["created"].as_u64().unwrap_or(0));
        Value::Array(list)
    }
    pub fn notify_pending(&self) {
        let list = self.pending_list();
        self.notify("approval", json!({"pending": list}));
    }

    pub fn actions_snapshot(&self) -> Vec<(String, AppAction)> {
        let mut v: Vec<(String, AppAction)> = self
            .actions
            .lock()
            .unwrap()
            .iter()
            .map(|(k, a)| (k.clone(), a.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    // ---- connected pages (transport-neutral) --------------------------------
    //
    // A "connection" is one live document that included uapp.js: natively a
    // WebSocket, in the browser build an iframe boot.js assigned an id to (a
    // reload of the same frame is a NEW connection — the old one is closed).
    // Both transports feed the same three entry points below and everything
    // the AI does with pages (actions, run_js, reload_app) is written once.

    /// A page connected. `send` delivers server→page messages to it.
    pub fn conn_open(&self, id: u64, send: ConnSender) {
        self.clients.fetch_add(1, Ordering::SeqCst);
        self.conns.lock().unwrap().insert(id, send);
    }

    /// A page went away: everything it registered dies with it.
    pub fn conn_close(&self, id: u64) {
        if self.conns.lock().unwrap().remove(&id).is_some() {
            self.clients.fetch_sub(1, Ordering::SeqCst);
        }
        self.actions.lock().unwrap().retain(|_, a| a.conn != id);
        self.contexts.lock().unwrap().retain(|(_, c)| *c != id);
        self.ctx_loaded.lock().unwrap().remove(&id);
    }

    /// Transport-level messages a page sends about ITSELF (they need the
    /// connection's identity, so they never reach `rpc::dispatch`). Returns
    /// `None` for anything else, `Some(reply)` for a handled one.
    pub fn conn_message(&self, id: u64, method: &str, params: &Value) -> Option<Value> {
        match method {
            "ctx.register" => {
                let ctx = params["context"].as_str().unwrap_or("");
                if !ctx.is_empty() && ctx.len() <= 32 {
                    let mut ctxs = self.contexts.lock().unwrap();
                    ctxs.retain(|(c, cid)| !(c == ctx && *cid == id));
                    ctxs.push((ctx.to_string(), id));
                }
                Some(json!({"ok": true}))
            }
            "ctx.loaded" => {
                self.ctx_loaded.lock().unwrap().insert(id);
                Some(json!({"ok": true}))
            }
            "actions.register" => {
                let mut reg = self.actions.lock().unwrap();
                reg.retain(|_, a| a.conn != id);
                let mut count = 0;
                for a in params["actions"].as_array().cloned().unwrap_or_default() {
                    let Some(name) = a["name"].as_str() else { continue };
                    if name.is_empty() || name.len() > 64 {
                        continue;
                    }
                    reg.insert(
                        name.to_string(),
                        AppAction {
                            description: a["description"].as_str().unwrap_or("").to_string(),
                            schema: if a["schema"].is_object() {
                                a["schema"].clone()
                            } else {
                                json!({"type": "object", "properties": {}})
                            },
                            readonly: a["readonly"].as_bool().unwrap_or(false),
                            conn: id,
                        },
                    );
                    count += 1;
                }
                Some(json!({"ok": true, "count": count}))
            }
            "actions.result" | "eval.result" => {
                let iid = params["id"].as_str().unwrap_or("");
                if let Some(slot) = self.invokes.lock().unwrap().get_mut(iid) {
                    *slot = Some(params.clone());
                }
                Some(json!({"ok": true}))
            }
            // Console output / uncaught errors forwarded from the live app
            // iframe, buffered for the AI's read_console tool.
            "log.write" => {
                self.console_push(
                    params["level"].as_str().unwrap_or("log"),
                    params["text"].as_str().unwrap_or(""),
                );
                Some(json!({"ok": true}))
            }
            _ => None,
        }
    }

    /// Newest still-connected page that registered `context`.
    fn context_conn(&self, context: &str) -> Option<u64> {
        let ctxs = self.contexts.lock().unwrap();
        let conns = self.conns.lock().unwrap();
        ctxs.iter()
            .rev()
            .find(|(c, id)| c == context && conns.contains_key(id))
            .map(|(_, id)| *id)
    }

    /// Poll `pred` every `step_ms` until it holds (`true`) or `timeout_ms`
    /// passes (`false`). The one blocking primitive under every page wait:
    /// natively a thread sleep, in the browser build a wait that keeps
    /// servicing page RPCs (the reply we wait for arrives through them).
    pub fn wait_until(&self, timeout_ms: u64, step_ms: u64, mut pred: impl FnMut() -> bool) -> anyhow::Result<bool> {
        let start = crate::store::now_ms();
        loop {
            if pred() {
                return Ok(true);
            }
            if crate::store::now_ms().saturating_sub(start) >= timeout_ms {
                return Ok(false);
            }
            sleep_ms(step_ms)?;
        }
    }

    /// Block until `name` is registered, returning the owning connection.
    /// Right after reload_app the page has loaded but may still be running
    /// its bootstrap chain, so its actions register a moment later; waiting
    /// here beats failing with "unknown action".
    pub fn wait_for_action(&self, name: &str) -> anyhow::Result<u64> {
        let find = || self.actions.lock().unwrap().get(name).map(|a| a.conn);
        if self.wait_until(ACTION_REGISTER_WAIT_MS, 100, || find().is_some())? {
            return Ok(find().unwrap());
        }
        anyhow::bail!(
            "no app action named '{name}' is registered. If you just called reload_app the \
             page may still be initialising — wait a moment and retry; otherwise the app does \
             not register that action (check its uapp.action(...) calls and read_console for \
             load errors)"
        )
    }

    /// Send `method` to one page and block until it answers (`*.result` with
    /// the same id) or `timeout_ms` passes. `what` names the call in errors.
    fn page_call(&self, conn_id: u64, method: &str, mut params: Value, timeout_ms: u64, what: &str) -> anyhow::Result<Value> {
        let send = self
            .conns
            .lock()
            .unwrap()
            .get(&conn_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the page for {what} has disconnected"))?;
        let id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        params["id"] = json!(id);
        self.invokes.lock().unwrap().insert(id.clone(), None);
        let msg = json!({"method": method, "params": params}).to_string();
        if !send(msg) {
            self.invokes.lock().unwrap().remove(&id);
            anyhow::bail!("the page disconnected before {what} could run");
        }
        let done = || self.invokes.lock().unwrap().get(&id).map(|v| v.is_some()).unwrap_or(true);
        let finished = self.wait_until(timeout_ms, 20, done);
        let reply = self.invokes.lock().unwrap().remove(&id).flatten();
        finished?;
        match reply {
            Some(v) => {
                if let Some(err) = v["error"].as_str() {
                    anyhow::bail!("{err}");
                }
                Ok(v["result"].clone())
            }
            None => anyhow::bail!("{what} did not respond within {}s", timeout_ms / 1000),
        }
    }

    /// Invoke an app-registered action in the page that registered it and
    /// block until it replies or times out. Writes the handler makes flow
    /// through the normal write path like any user click.
    pub fn invoke_action(&self, name: &str, input: &Value) -> anyhow::Result<Value> {
        let conn_id = self.wait_for_action(name)?;
        let timeout_ms = env_ms("UAPP_ACTION_TIMEOUT_MS", 60_000);
        self.page_call(conn_id, "action.invoke", json!({"name": name, "input": input}), timeout_ms, &format!("action '{name}'"))
            .map_err(|e| {
                let m = e.to_string();
                if m.contains("did not respond") {
                    anyhow::anyhow!("{m} (page busy or reloading?)")
                } else if m.contains("disconnected") {
                    e
                } else {
                    anyhow::anyhow!("action '{name}' failed: {m}")
                }
            })
    }

    /// Run JavaScript in a connected page context ("scratchpad" or "app") and
    /// block until the page replies or times out. The code runs inside the
    /// browser with the full uapp API, so any writes it makes flow through
    /// the normal write path.
    pub fn invoke_eval(&self, context: &str, code: &str) -> anyhow::Result<Value> {
        let mut conn_id = self.context_conn(context);
        // The shell keeps its scratch frame empty until something needs it, so
        // the first scratchpad call of a session normally finds no context at
        // all. Ask the shells to load it and wait for it to register rather
        // than failing a call that would have worked a second later.
        if conn_id.is_none() && context == "scratchpad" {
            self.notify("scratch-load", json!({}));
            self.wait_until(5_000, 50, || {
                conn_id = self.context_conn(context);
                conn_id.is_some()
            })?;
        }
        let conn_id = conn_id.ok_or_else(|| {
            if context == "app" {
                anyhow::anyhow!(
                    "no 'app' page is connected — the app must be open in a browser and its page must include <script src=\"/uapp.js\"></script> (use the scratchpad context otherwise)"
                )
            } else {
                anyhow::anyhow!(
                    "no '{context}' page is connected — the app must be open in a browser (the shell loads the scratchpad frame on demand, so this means no shell is connected)"
                )
            }
        })?;
        let timeout_ms = env_ms("UAPP_EVAL_TIMEOUT_MS", 60_000);
        self.page_call(conn_id, "eval.invoke", json!({"code": code}), timeout_ms, &format!("code in '{context}'"))
            .map_err(|e| {
                let m = e.to_string();
                if m.contains("did not respond") {
                    anyhow::anyhow!("{} (infinite loop, or a promise that never resolves?)", m.replace("did not respond", "did not finish"))
                } else if m.contains("disconnected") {
                    e
                } else {
                    anyhow::anyhow!("code failed in {context}: {m}")
                }
            })
    }

    /// Reload the live app page and wait until the NEW document has finished
    /// loading (its uapp.js sent `ctx.loaded` over a fresh connection), so a
    /// run_js / read_console right after this sees the new page. Without a
    /// connected app page there is nothing to wait for; a page that never
    /// finishes (throws on load, very slow) returns `loaded: false` after
    /// [`RELOAD_WAIT_MS`] instead of hanging the turn.
    pub fn invoke_reload(&self) -> Value {
        let app_conns = || -> std::collections::HashSet<u64> {
            let ctxs = self.contexts.lock().unwrap();
            let conns = self.conns.lock().unwrap();
            ctxs.iter()
                .filter(|(c, id)| c == "app" && conns.contains_key(id))
                .map(|(_, id)| *id)
                .collect()
        };
        let before = app_conns();
        self.notify("reload", json!({}));
        if before.is_empty() {
            return json!({"ok": true, "loaded": false,
                "note": "no app page is connected, so there was nothing to wait for"});
        }
        let start = crate::store::now_ms();
        let loaded = self.wait_until(RELOAD_WAIT_MS, 50, || {
            let fresh = app_conns();
            let loaded = self.ctx_loaded.lock().unwrap();
            fresh.iter().any(|id| !before.contains(id) && loaded.contains(id))
        });
        match loaded {
            Ok(true) => json!({"ok": true, "loaded": true, "ms": crate::store::now_ms() - start}),
            Ok(false) => json!({"ok": true, "loaded": false,
                "note": format!("the app page did not finish reloading within {}s — it may be slow or throwing on load; read_console will tell", RELOAD_WAIT_MS / 1000)}),
            Err(e) => json!({"ok": true, "loaded": false, "note": format!("could not confirm the reload: {e}")}),
        }
    }
}

fn env_ms(var: &str, default: u64) -> u64 {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Sleep without holding any `App` lock. Native: a plain thread sleep (page
/// replies arrive on the WebSocket tasks meanwhile). wasm: the worker blocks
/// on the shared-memory wait cell while draining the page's RPC inbox, which
/// is how `ctx.loaded` / `actions.result` reach us mid-run; without
/// SharedArrayBuffer there is no way to wait, so the call errors out at once.
fn sleep_ms(ms: u64) -> anyhow::Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    }
    #[cfg(target_arch = "wasm32")]
    {
        if crate::wasm::sleep_ms(ms) {
            Ok(())
        } else {
            anyhow::bail!("page bridge unavailable (the page is not cross-origin isolated — run_js, app actions and reload waits need COOP/COEP headers)")
        }
    }
}

impl App {
    pub fn notify_changes(&self, c: ChangeSet) {
        if !c.any() {
            return;
        }
        self.push_event(json!({
            "method": "event",
            "params": {"type": "changes", "data": c.data, "files": c.files,
                        "chat": c.chat, "config": c.config}
        }));
    }
    /// Relay one native (OS) file drop to the connected shells. Called by the
    /// desktop window's drag-drop handler — the webview never sees these as
    /// HTML5 drag events, so this is what makes drag-and-drop work there.
    /// Dropped paths are kept server-side under a one-shot id; the page only
    /// learns the id and the file names, and asks for them back with
    /// `drop.ingest`.
    pub fn native_drop(&self, info: crate::native::DropInfo) {
        use crate::native::DropPhase;
        let names: Vec<String> = info
            .paths
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.to_string_lossy().to_string())
            })
            .collect();
        match info.phase {
            // Names ride along on hover too (the drag's Enter event carries
            // them, later moves don't), so the shell can tell a .uapp — an
            // app update — from files to file away, and not light up a drop
            // zone that won't be used. Still no paths: the page never sees one.
            DropPhase::Hover => {
                self.notify("drop_hover", json!({"x": info.x, "y": info.y, "names": names}))
            }
            DropPhase::Leave => self.notify("drop_leave", json!({})),
            DropPhase::Drop => {
                let id: String = (0..12).map(|_| fastrand::alphanumeric()).collect();
                {
                    let mut drops = self.drops.lock().unwrap();
                    if drops.len() > 8 {
                        drops.clear(); // ids expire anyway; keep this bounded
                    }
                    drops.insert(id.clone(), (crate::store::now_ms(), info.paths));
                }
                self.notify("drop_files", json!({"id": id, "names": names,
                                                 "x": info.x, "y": info.y}));
            }
        }
    }
    pub fn notify(&self, typ: &str, extra: Value) {
        let mut params = json!({"type": typ});
        if let (Some(obj), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                obj.insert(k.clone(), v.clone());
            }
        }
        self.push_event(json!({"method": "event", "params": params}));
    }

    /// One event to every client: the broadcast channel on native (WebSocket
    /// fan-out), a direct JS callback on wasm (the worker posts it to the
    /// page, which fans out to the shell and every iframe).
    fn push_event(&self, v: Value) {
        #[cfg(target_arch = "wasm32")]
        crate::wasm::forward_event(&v);
        let _ = self.events.send(v);
    }
}
