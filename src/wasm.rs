//! Browser (wasm32) entry: the whole uapp core compiled to WebAssembly and
//! driven from JS instead of a local HTTP server.
//!
//! Runs inside a dedicated Web Worker. The page keeps the exact same shell UI;
//! its JSON-RPC calls arrive here through `rpc_dispatch` (postMessage instead
//! of a WebSocket), notifications go back out through `js_notify`, and the
//! two things an AI run blocks on — HTTP to the provider and page invocations
//! (run_js, app actions, approval prompts) — go through SharedArrayBuffer
//! waits that the main thread satisfies (`js_http_*`, `js_bridge_call`).
//! While the worker waits it keeps servicing the page's RPCs (the glue
//! re-enters `rpc_dispatch` from an inbox), so the app, the shell and the
//! service worker all keep working mid-run; the run itself stays a plain
//! synchronous loop.

use crate::app::App;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(raw_module = "./uapp_glue.js")]
extern "C" {
    /// Date.now() (SystemTime is unavailable on wasm32-unknown-unknown).
    pub fn js_now_ms() -> f64;
    /// Forward one server->client event (JSON envelope) to the page.
    pub fn js_notify(json: &str);
    /// Start an HTTP request. The fetch itself runs on the main thread; this
    /// blocks (servicing page RPCs meanwhile) until the response headers are
    /// in. Returns JSON {id, status, contentType, headers: [[k,v]...]} — or,
    /// when the page isn't cross-origin isolated (no SharedArrayBuffer) and
    /// a sync XHR had to be used, the whole body too as `bodyB64` — or
    /// {error}.
    pub fn js_http_open(method: &str, url: &str, headers_json: &str, body: Option<String>, connect_timeout_ms: f64) -> String;
    /// Next body chunk of an open request (empty = end of body). Blocks —
    /// servicing page RPCs meanwhile — until data arrives or the read timeout
    /// passes. Throws on a transport failure.
    #[wasm_bindgen(catch)]
    pub fn js_http_read(id: u32, read_timeout_ms: f64) -> Result<Vec<u8>, JsValue>;
    /// Release a request (cancels the fetch if the body wasn't consumed).
    pub fn js_http_close(id: u32);
    /// Blocking call to the main thread over SharedArrayBuffer + Atomics.wait.
    /// Returns the reply JSON, or {"error": "..."} (including when SAB is
    /// unavailable because the site isn't cross-origin isolated).
    pub fn js_bridge_call(kind: &str, payload_json: &str) -> String;
    /// Push one message to the page connection boot.js knows by this id.
    pub fn js_conn_send(conn: f64, msg: &str) -> bool;
    /// Block for `ms` while servicing the page's RPC inbox; false when there is
    /// no SharedArrayBuffer to wait on.
    pub fn js_sleep(ms: f64) -> bool;
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

pub fn http_open(method: &str, url: &str, headers_json: &str, body: Option<&str>, connect_timeout_ms: u64) -> String {
    js_http_open(method, url, headers_json, body.map(|s| s.to_string()), connect_timeout_ms as f64)
}

pub fn http_read(id: u32, read_timeout_ms: u64) -> std::io::Result<Vec<u8>> {
    js_http_read(id, read_timeout_ms as f64).map_err(|e| {
        let msg = e.as_string().unwrap_or_else(|| format!("{e:?}"));
        std::io::Error::other(msg)
    })
}

pub fn http_close(id: u32) {
    js_http_close(id);
}

pub fn bridge_call(kind: &str, payload: &Value) -> Result<Value> {
    let reply = js_bridge_call(kind, &payload.to_string());
    let v: Value = serde_json::from_str(&reply).map_err(|_| anyhow!("bad bridge reply"))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    Ok(v)
}

pub fn sleep_ms(ms: u64) -> bool {
    js_sleep(ms as f64)
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
        let app = Arc::new(App::new(eng, String::new(), false));
        // The page is the one, always-connected client.
        app.clients.store(1, std::sync::atomic::Ordering::SeqCst);
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
        // Page-connection bookkeeping (server.rs does the same before its
        // dispatch): boot.js wraps each iframe's transport messages with the
        // connection id it assigned to that document.
        match method.as_str() {
            "conn.close" => {
                app.conn_close(params["conn"].as_u64().unwrap_or(0));
                return Ok(json!({"ok": true}));
            }
            "conn.msg" => {
                let id = params["conn"].as_u64().unwrap_or(0);
                // First message from a document opens its connection: one
                // message shape, no open/msg ordering to get wrong between
                // the postMessage and inbox paths.
                if !app.conns.lock().unwrap().contains_key(&id) {
                    app.conn_open(id, Arc::new(move |m: String| js_conn_send(id as f64, &m)));
                }
                let inner = params["method"].as_str().unwrap_or("");
                return app
                    .conn_message(id, inner, &params["params"])
                    .ok_or_else(|| anyhow!("{inner} is not a connection message"));
            }
            _ => {}
        }
        crate::rpc::dispatch(&app, &method, params)
    })();
    match out {
        Ok(v) => json!({"result": v}).to_string(),
        Err(e) => json!({"error": {"message": e.to_string()}}).to_string(),
    }
}

/// Run one queued AI session to completion (scheduled by `js_schedule_ai`).
/// Occupies the worker for the whole run, but every wait inside it (provider
/// HTTP, bridge calls) services page RPCs re-entrantly — see the glue — so
/// the run must never hold an `App` lock across such a call (none do: every
/// guard here is scoped to a single query).
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
