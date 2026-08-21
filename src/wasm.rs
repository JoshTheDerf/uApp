//! Browser (wasm32) entry: the whole uapp core compiled to WebAssembly and
//! driven from JS instead of a local HTTP server.
//!
//! Runs inside a dedicated Web Worker. The page keeps the exact same shell UI;
//! its JSON-RPC calls arrive here through `rpc_dispatch` (postMessage instead
//! of a WebSocket), notifications go back out through `js_notify`, HTTP is a
//! synchronous XHR in the worker, and page invocations that must complete
//! while the worker is blocked inside an AI run (run_js, app actions, approval
//! prompts) travel over a SharedArrayBuffer bridge (`js_bridge_call`).

use crate::app::App;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(raw_module = "./uapp_glue.js")]
extern "C" {
    /// Date.now() (SystemTime is unavailable on wasm32-unknown-unknown).
    pub fn js_now_ms() -> f64;
    /// Forward one server->client event (JSON envelope) to the page.
    pub fn js_notify(json: &str);
    /// Synchronous HTTP (sync XHR in the worker). Returns JSON:
    /// {status, contentType, headers: [[k,v]...], bodyB64} or {error}.
    pub fn js_http_request(method: &str, url: &str, headers_json: &str, body: Option<String>) -> String;
    /// Blocking call to the main thread over SharedArrayBuffer + Atomics.wait.
    /// Returns the reply JSON, or {"error": "..."} (including when SAB is
    /// unavailable because the site isn't cross-origin isolated).
    pub fn js_bridge_call(kind: &str, payload_json: &str) -> String;
    /// Ask the glue to call `ai_tick(session)` as a fresh task, after the
    /// current RPC has replied.
    pub fn js_schedule_ai(session: &str);
    /// Has the shell raised the stop flag (SharedArrayBuffer cell)?
    pub fn js_stop_requested() -> bool;
    /// Clear the stop flag (called when a run starts).
    pub fn js_clear_stop();
}

thread_local! {
    static APP: RefCell<Option<Arc<App>>> = const { RefCell::new(None) };
}

fn app() -> Result<Arc<App>> {
    APP.with(|a| a.borrow().clone())
        .ok_or_else(|| anyhow!("no app is open"))
}

// ---- helpers used by the rest of the core ----------------------------------

pub fn now_ms() -> u64 {
    js_now_ms() as u64
}

pub fn forward_event(v: &Value) {
    js_notify(&v.to_string());
}

pub fn http_request(method: &str, url: &str, headers_json: &str, body: Option<&str>) -> String {
    js_http_request(method, url, headers_json, body.map(|s| s.to_string()))
}

pub fn bridge_call(kind: &str, payload: &Value) -> Result<Value> {
    let reply = js_bridge_call(kind, &payload.to_string());
    let v: Value = serde_json::from_str(&reply).map_err(|_| anyhow!("bad bridge reply"))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    Ok(v)
}

pub fn schedule_ai(session: &str) {
    js_schedule_ai(session);
}

pub fn stop_requested() -> bool {
    js_stop_requested()
}

// ---- exports ----------------------------------------------------------------

/// Open (or create) the app. `bytes` = a complete .uapp file, or null for a
/// fresh blank app. Returns app.info as JSON, or throws with a message.
#[wasm_bindgen]
pub fn open_app(bytes: Option<Vec<u8>>, name: String, user: String) -> Result<String, JsValue> {
    let opened = (|| -> Result<String> {
        let eng = crate::engine::Engine::open_from_bytes(
            &name,
            bytes,
            "browser".to_string(),
            if user.trim().is_empty() { "You".to_string() } else { user },
        )?;
        let app_name = crate::store::meta_get(&eng.db, "name")?.unwrap_or_else(|| name.clone());
        let (events, _) = tokio::sync::broadcast::channel(16);
        let app = Arc::new(App {
            engine: Mutex::new(eng),
            events,
            token: String::new(),
            name: app_name,
            ai_runs: Mutex::new(std::collections::HashMap::new()),
            unsaved: AtomicBool::new(false),
            ai_mode: Mutex::new("auto".into()),
            pending: Mutex::new(std::collections::HashMap::new()),
            questions: Mutex::new(std::collections::HashMap::new()),
            always_allow: Mutex::new(std::collections::HashSet::new()),
            clients: std::sync::atomic::AtomicUsize::new(1),
            port: std::sync::atomic::AtomicU16::new(0),
            conns: Mutex::new(std::collections::HashMap::new()),
            actions: Mutex::new(std::collections::HashMap::new()),
            invokes: Mutex::new(std::collections::HashMap::new()),
            drops: Mutex::new(std::collections::HashMap::new()),
        templates: Mutex::new(std::collections::HashMap::new()),
            contexts: Mutex::new(Vec::new()),
            console: Mutex::new(std::collections::VecDeque::new()),
            console_seq: std::sync::atomic::AtomicU64::new(0),
            console_gen: std::sync::atomic::AtomicU64::new(0),
            console_ack: std::sync::atomic::AtomicU64::new(0),
        });
        let info = crate::rpc::dispatch(&app, "app.info", json!({}))?;
        APP.with(|a| *a.borrow_mut() = Some(app));
        Ok(info.to_string())
    })();
    opened.map_err(|e| JsValue::from_str(&format!("{e:#}")))
}

/// One JSON-RPC call. Returns {"result": ...} or {"error": {"message": ...}}.
#[wasm_bindgen]
pub fn rpc_dispatch(method: String, params_json: String) -> String {
    let out = (|| -> Result<Value> {
        let app = app()?;
        let params: Value = serde_json::from_str(&params_json).unwrap_or(json!({}));
        // Same trust rule as the native transport: these markers are only
        // ever set server-side.
        let mut params = params;
        if let Some(o) = params.as_object_mut() {
            o.remove("_user_approved");
            o.remove("_assistant");
        }
        crate::rpc::dispatch(&app, &method, params)
    })();
    match out {
        Ok(v) => json!({"result": v}).to_string(),
        Err(e) => json!({"error": {"message": e.to_string()}}).to_string(),
    }
}

/// Run one queued AI session to completion (scheduled by `js_schedule_ai`).
/// Blocks the worker for the whole run — notifications still flow out via
/// postMessage, and the SAB bridge serves the run's page invocations.
#[wasm_bindgen]
pub fn ai_tick(session: String) {
    let Ok(app) = app() else { return };
    js_clear_stop();
    let stop = {
        let runs = app.ai_runs.lock().unwrap();
        match runs.get(&session) {
            Some(h) => h.stop.clone(),
            None => return, // stopped/removed before we got scheduled
        }
    };
    let ctx = crate::ai::RunCtx::root(session.clone(), stop);
    let result = crate::ai::run(&app, &ctx);
    app.ai_runs.lock().unwrap().remove(&session);
    match result {
        Ok(_) => app.notify("ai", json!({"state": "idle", "session": session})),
        Err(e) => {
            app.notify("ai", json!({"state": "error", "message": e.to_string(),
                                    "session": session}));
            let _ = crate::rpc::local_op(
                &app,
                "chat",
                json!({"mid": format!("err-{}-{}", crate::store::now_ms(), fastrand::u32(..)),
                       "role": "system", "session": session,
                       "content": {"text": format!("AI error: {e}")}}),
            );
        }
    }
}
