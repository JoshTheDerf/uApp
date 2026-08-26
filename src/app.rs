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
    /// Outbound message channel per WebSocket connection (for server->client
    /// action invocations). Unused on wasm (the bridge routes invocations).
    pub conns: Mutex<HashMap<u64, tokio::sync::mpsc::Sender<String>>>,
    /// App-registered actions: name -> definition + owning connection.
    pub actions: Mutex<HashMap<String, AppAction>>,
    /// In-flight action invocations awaiting a client reply.
    pub invokes: Mutex<HashMap<String, std::sync::mpsc::SyncSender<Value>>>,
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
    ///
    /// wasm: console lines are buffered on the MAIN thread (the worker is
    /// blocked during AI runs, so log.write RPCs can't land here in time) —
    /// read them over the bridge.
    #[cfg(target_arch = "wasm32")]
    pub fn console_read(&self, only_errors: bool, only_latest: bool, limit: usize) -> Value {
        crate::wasm::bridge_call("console.read", &json!({
            "only_errors": only_errors, "only_latest": only_latest, "limit": limit,
        }))
        .unwrap_or_else(|e| json!({"entries": [], "dropped": 0, "latest_gen": 0,
                                   "note": format!("console unavailable: {e}")}))
    }

    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(target_arch = "wasm32")]
    pub fn console_alert(&self) -> Option<String> {
        crate::wasm::bridge_call("console.alert", &json!({}))
            .ok()
            .and_then(|v| v["note"].as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn console_alert(&self) -> Option<String> {
        let ack = self.console_ack.load(Ordering::Relaxed);
        let buf = self.console.lock().unwrap();
        let mut errs = 0usize;
        let mut warns = 0usize;
        let mut others = 0usize;
        let mut top = 0u64;
        for e in buf.iter().filter(|e| e.seq > ack) {
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

    /// Invoke an app-registered action in the page that registered it and
    /// block (AI thread) until it replies or times out. Writes the handler
    /// makes flow through the normal write path like any user click.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn invoke_action(&self, name: &str, input: &Value) -> anyhow::Result<Value> {
        let conn_id = self
            .actions
            .lock()
            .unwrap()
            .get(name)
            .map(|a| a.conn)
            .ok_or_else(|| anyhow::anyhow!("no app action named '{name}' is currently registered (is the app open?)"))?;
        let sender = self
            .conns
            .lock()
            .unwrap()
            .get(&conn_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the app page that registered '{name}' has disconnected"))?;
        let id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Value>(1);
        self.invokes.lock().unwrap().insert(id.clone(), tx);
        let msg = json!({"method": "action.invoke",
                         "params": {"id": id, "name": name, "input": input}});
        if sender.blocking_send(msg.to_string()).is_err() {
            self.invokes.lock().unwrap().remove(&id);
            anyhow::bail!("the app page disconnected before the action could run");
        }
        let timeout_ms: u64 = std::env::var("UAPP_ACTION_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000);
        let res = rx.recv_timeout(std::time::Duration::from_millis(timeout_ms));
        self.invokes.lock().unwrap().remove(&id);
        match res {
            Ok(v) => {
                if let Some(err) = v["error"].as_str() {
                    anyhow::bail!("action '{name}' failed: {err}");
                }
                Ok(v["result"].clone())
            }
            Err(_) => anyhow::bail!(
                "action '{name}' did not respond within {}s (page busy or reloading?)",
                timeout_ms / 1000
            ),
        }
    }

    /// wasm: route the invocation over the worker bridge — the main thread
    /// glue delivers it to the page that registered the action and blocks us
    /// (Atomics.wait) until it replies.
    #[cfg(target_arch = "wasm32")]
    pub fn invoke_action(&self, name: &str, input: &Value) -> anyhow::Result<Value> {
        let reply = crate::wasm::bridge_call("action", &json!({"name": name, "input": input}))?;
        if let Some(err) = reply["error"].as_str() {
            anyhow::bail!("action '{name}' failed: {err}");
        }
        Ok(reply["result"].clone())
    }

    /// Run JavaScript in a connected page context ("scratchpad" or "app") and
    /// block (AI thread) until the page replies or times out. The code runs
    /// inside the browser with the full uapp API, so any writes it makes flow
    /// through the normal write path.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn invoke_eval(&self, context: &str, code: &str) -> anyhow::Result<Value> {
        let find_conn = || {
            let ctxs = self.contexts.lock().unwrap();
            let conns = self.conns.lock().unwrap();
            ctxs.iter()
                .rev()
                .find(|(c, id)| c == context && conns.contains_key(id))
                .map(|(_, id)| *id)
        };
        let mut conn_id = find_conn();
        // The shell keeps its scratch frame empty until something needs it, so
        // the first scratchpad call of a session normally finds no context at
        // all. Ask the shells to load it and wait for it to register rather
        // than failing a call that would have worked a second later.
        if conn_id.is_none() && context == "scratchpad" {
            self.notify("scratch-load", json!({}));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while conn_id.is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
                conn_id = find_conn();
            }
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
        let sender = self
            .conns
            .lock()
            .unwrap()
            .get(&conn_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the '{context}' page has disconnected"))?;
        let id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Value>(1);
        self.invokes.lock().unwrap().insert(id.clone(), tx);
        let msg = json!({"method": "eval.invoke", "params": {"id": id, "code": code}});
        if sender.blocking_send(msg.to_string()).is_err() {
            self.invokes.lock().unwrap().remove(&id);
            anyhow::bail!("the '{context}' page disconnected before the code could run");
        }
        let timeout_ms: u64 = std::env::var("UAPP_EVAL_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000);
        let res = rx.recv_timeout(std::time::Duration::from_millis(timeout_ms));
        self.invokes.lock().unwrap().remove(&id);
        match res {
            Ok(v) => {
                if let Some(err) = v["error"].as_str() {
                    anyhow::bail!("code failed in {context}: {err}");
                }
                Ok(v["result"].clone())
            }
            Err(_) => anyhow::bail!(
                "code in '{context}' did not finish within {}s (infinite loop, or a promise that never resolves?)",
                timeout_ms / 1000
            ),
        }
    }

    /// wasm: same, over the worker bridge (main-thread glue runs the code in
    /// the right iframe and writes the reply back).
    #[cfg(target_arch = "wasm32")]
    pub fn invoke_eval(&self, context: &str, code: &str) -> anyhow::Result<Value> {
        let reply =
            crate::wasm::bridge_call("eval", &json!({"context": context, "code": code}))?;
        if let Some(err) = reply["error"].as_str() {
            anyhow::bail!("code failed in {context}: {err}");
        }
        Ok(reply["result"].clone())
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
