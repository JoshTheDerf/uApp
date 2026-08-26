//! HTTP + WebSocket server for one open .uapp.
//! Everything binds to 127.0.0.1 with a per-session token.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::App;
pub use crate::app::{AppAction, ConsoleEntry, PendingApproval, PendingQuestion, RunHandle};

const SHELL_HTML: &str = include_str!("shell/index.html");
const SHELL_CSS: &str = include_str!("shell/shell.css");
/// The shell's ES modules (one Web Component or shared module per file),
/// served under /shell/<name>. Adding a file here is all it takes.
const SHELL_MODULES: &[(&str, &str)] = &[
    ("main.js", include_str!("shell/js/main.js")),
    ("core.js", include_str!("shell/js/core.js")),
    ("ui.js", include_str!("shell/js/ui.js")),
    ("markdown.js", include_str!("shell/js/markdown.js")),
    ("topbar.js", include_str!("shell/js/topbar.js")),
    ("chat-panel.js", include_str!("shell/js/chat-panel.js")),
    ("sql-panel.js", include_str!("shell/js/sql-panel.js")),
    ("files-panel.js", include_str!("shell/js/files-panel.js")),
    ("viewer.js", include_str!("shell/js/viewer.js")),
    ("export.js", include_str!("shell/js/export.js")),
    ("settings-panel.js", include_str!("shell/js/settings-panel.js")),
    ("tools-panel.js", include_str!("shell/js/tools-panel.js")),
    ("mirror.js", include_str!("shell/js/mirror.js")),
    ("template-update.js", include_str!("shell/js/template-update.js")),
    ("strings.js", include_str!("shell/js/strings.js")),
    ("lang/en.js", include_str!("shell/js/lang/en.js")),
    ("lang/fr.js", include_str!("shell/js/lang/fr.js")),
    ("lang/es.js", include_str!("shell/js/lang/es.js")),
    ("lang/de.js", include_str!("shell/js/lang/de.js")),
    ("lang/zh-Hans.js", include_str!("shell/js/lang/zh-Hans.js")),
];
const UAPP_JS: &str = include_str!("shell/uapp.js");
const ICONS_JS: &str = include_str!("shell/icons.js");
const SCRATCH_HTML: &str = include_str!("shell/scratch.html");

fn cookie_name(app: &App) -> String {
    format!("uapp_t_{}", app.port.load(std::sync::atomic::Ordering::Relaxed))
}

/// DNS-rebinding guard: the server binds 127.0.0.1 only, so a legitimate
/// request always arrives with a loopback Host. A rebinding attack (attacker
/// domain re-pointed at 127.0.0.1) carries the attacker's hostname here.
fn host_is_local(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return true; // HTTP/1.0 / some native webviews omit Host
    };
    let hostname = host
        .strip_prefix('[') // [::1]:port
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| host.split(':').next().unwrap_or(host));
    matches!(hostname, "localhost" | "127.0.0.1" | "::1") || hostname.starts_with("127.")
}

fn authed(app: &App, headers: &HeaderMap, q: &HashMap<String, String>) -> bool {
    if !host_is_local(headers) {
        return false;
    }
    if q.get("t").map(|t| t == &app.token).unwrap_or(false) {
        return true;
    }
    let want = format!("{}={}", cookie_name(app), app.token);
    headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .map(|c| c.split(';').any(|kv| kv.trim() == want))
        .unwrap_or(false)
}

fn deny() -> Response {
    (StatusCode::FORBIDDEN, "uapp: missing or bad token").into_response()
}

fn html_escape(s: &str) -> String {
    s.chars().map(|c| match c {
        '&' => "&amp;".to_string(),
        '<' => "&lt;".to_string(),
        '>' => "&gt;".to_string(),
        '"' => "&quot;".to_string(),
        '\'' => "&#39;".to_string(),
        c => c.to_string(),
    }).collect()
}

async fn shell(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    // A root-absolute `href="/"` followed INSIDE the app frame means the app's
    // front page, not a second shell nested in the first. WebViews and
    // browsers label frame navigations with Sec-Fetch-Dest: iframe.
    if headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("iframe") {
        return serve_sqlar(&app, "index.html", &headers);
    }
    // The name comes from the shared .uapp file — escape it, or a hostile
    // file gets stored XSS in the (privileged) shell origin.
    let mut resp = Html(SHELL_HTML.replace("{{APP_NAME}}", &html_escape(&app.name))).into_response();
    // Refresh the cookie so the iframe + ws can auth without the token in URL.
    resp.headers_mut().insert(
        header::SET_COOKIE,
        format!("{}={}; Path=/; SameSite=Strict", cookie_name(&app), app.token)
            .parse()
            .unwrap(),
    );
    // The shell is embedded in the binary and changes on every upgrade;
    // stale-cached shells would keep showing the old UI.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

fn static_asset(body: &'static str, ctype: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

pub fn content_type_for(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "csv" => "text/csv",
        "txt" => "text/plain; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        // Media types must be exact for <video>/<audio> to play at all.
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "m4v" => "video/x-m4v",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        // instantiateStreaming() requires the exact MIME type
        "wasm" => "application/wasm",
        "map" => "application/json",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Parse a single-range `Range: bytes=…` header against a known body length.
/// `None` = no usable header (multi-range and malformed specs included), so the
/// caller answers 200 with the full body; `Some(Err(()))` = unsatisfiable and
/// must become a 416 per RFC 9110.
fn parse_range(headers: &HeaderMap, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers
        .get(header::RANGE)?
        .to_str()
        .ok()?
        .trim()
        .strip_prefix("bytes=")?
        .to_string();
    if spec.contains(',') {
        return None; // multi-range: not worth the multipart body, serve it all
    }
    let (first, last) = spec.split_once('-')?;
    let (start, end) = if first.trim().is_empty() {
        // suffix form "-N": the last N bytes
        let n: u64 = last.trim().parse().ok()?;
        if n == 0 {
            return Some(Err(()));
        }
        (len.saturating_sub(n), len.saturating_sub(1))
    } else {
        let start: u64 = first.trim().parse().ok()?;
        let end = if last.trim().is_empty() {
            len.saturating_sub(1)
        } else {
            last.trim().parse::<u64>().ok()?.min(len.saturating_sub(1))
        };
        (start, end)
    };
    if len == 0 || start >= len || start > end {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

fn serve_sqlar(app: &App, name: &str, headers: &HeaderMap) -> Response {
    if name.contains("..") {
        return deny();
    }
    // Read + inflate under the lock, then release it: building the response
    // (and slicing a range out of a large video) must not block every other
    // request on the engine mutex.
    // Pretty URLs, as a static host would resolve them: "posts/x/" and
    // "posts/x" -> posts/x/index.html, "admin" -> admin.html. Generated sites
    // (and the public `uapp serve` router) link this way.
    let (data, name) = {
        let eng = app.engine.lock().unwrap();
        let mut found = (crate::store::sqlar_read(&eng.db, name), name.to_string());
        if matches!(found.0, Ok(None)) {
            let bare = name.trim_end_matches('/');
            let last = bare.rsplit('/').next().unwrap_or(bare);
            if !last.contains('.') || name.ends_with('/') {
                let mut tries = vec![format!("{bare}/index.html")];
                if !name.ends_with('/') && !bare.is_empty() {
                    tries.insert(0, format!("{bare}.html"));
                }
                for t in tries {
                    match crate::store::sqlar_read(&eng.db, &t) {
                        Ok(Some(b)) => { found = (Ok(Some(b)), t); break; }
                        Ok(None) => {}
                        Err(e) => { found = (Err(e), t); break; }
                    }
                }
            }
        }
        found
    };
    let name = name.as_str();
    match data {
        Ok(Some(bytes)) => {
            let ctype = content_type_for(name);
            // App HTML written without a viewport meta renders at a ~980px
            // desktop layout width inside the mobile WebView's iframe, which
            // is unreadable on a phone. Inject one when missing (harmless on
            // desktop). Covers the starter page and AI-generated apps alike.
            let bytes = if ctype.starts_with("text/html") {
                inject_viewport(bytes)
            } else {
                bytes
            };
            let total = bytes.len() as u64;
            match parse_range(headers, total) {
                Some(Ok((start, end))) => {
                    let slice = bytes[start as usize..=end as usize].to_vec();
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            (header::CONTENT_TYPE, ctype.to_string()),
                            (header::CACHE_CONTROL, "no-store".to_string()),
                            (header::ACCEPT_RANGES, "bytes".to_string()),
                            (header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}")),
                        ],
                        slice,
                    )
                        .into_response()
                }
                Some(Err(())) => (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                    "range not satisfiable",
                )
                    .into_response(),
                None => (
                    [
                        (header::CONTENT_TYPE, ctype),
                        (header::CACHE_CONTROL, "no-store"),
                        // Seeking in <video>/<audio> needs this on the 200 too.
                        (header::ACCEPT_RANGES, "bytes"),
                    ],
                    bytes,
                )
                    .into_response(),
            }
        }
        Ok(None) => (StatusCode::NOT_FOUND, format!("no such file in app: {name}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn app_file(
    State(app): State<Arc<App>>,
    AxPath(path): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let name = if path.is_empty() { "index.html".to_string() } else { path };
    serve_sqlar(&app, &name, &headers)
}

/// Inject `<script src="/uapp.js">` (unless the page already loads it) and a
/// `<meta name="viewport">` (only when the document lacks one — apps written
/// without it render at a ~980px desktop width inside the mobile WebView,
/// unreadable on a phone). Both go right after the opening `<head>` tag, or
/// are prepended if there is none.
///
/// uapp.js is injected into every app page so its console-forwarding (the
/// app's console.* + uncaught errors, streamed to the server for the chat-tab
/// AI's `read_console` tool) is always present — even in a half-written draft
/// that hasn't added the tag yet. It reuses uapp.js's existing WebSocket, so
/// no second connection is opened. Safe against a double-load: injection is
/// skipped when the tag is already present, and uapp.js self-guards anyway.
///
/// The viewer never executes app HTML (it renders source as text), so this
/// only ever runs uapp.js inside the real app iframe — no spurious "app" eval
/// context is registered. Covers the starter page and AI-generated apps alike.
fn inject_viewport(bytes: Vec<u8>) -> Vec<u8> {
    const META: &str = r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#;
    const UAPP_TAG: &str = r#"<script src="/uapp.js"></script>"#;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => return e.into_bytes(), // non-utf8: serve untouched
    };
    let lower = text.to_ascii_lowercase();
    let has_vp = lower.contains("name=\"viewport\"") || lower.contains("name='viewport'");
    let has_uapp = lower.contains("uapp.js");
    if has_vp && has_uapp {
        return text.into_bytes(); // nothing to add
    }
    let mut inject = String::new();
    if !has_vp {
        inject.push('\n');
        inject.push_str(META);
    }
    if !has_uapp {
        inject.push('\n');
        inject.push_str(UAPP_TAG);
    }
    if let Some(pos) = lower.find("<head") {
        // insert after the closing '>' of the <head ...> tag
        if let Some(gt) = text[pos..].find('>') {
            let at = pos + gt + 1;
            let mut out = String::with_capacity(text.len() + inject.len());
            out.push_str(&text[..at]);
            out.push_str(&inject);
            out.push_str(&text[at..]);
            return out.into_bytes();
        }
    }
    format!("{inject}\n{text}").into_bytes()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fallback: serve archive files at the root too, so apps written with
/// root-absolute references (`/styles.css`, `/js/app.js` — the same shape as
/// the documented `/uapp.js`) work exactly like relative ones. Shell routes
/// are registered explicitly and always win.
async fn root_file(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let name = percent_decode(uri.path().trim_start_matches('/'));
    if name.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    serve_sqlar(&app, &name, &headers)
}

async fn app_root(
    state: State<Arc<App>>,
    q: Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    app_file(state, AxPath(String::new()), q, headers).await
}

async fn health() -> &'static str {
    "ok"
}

/// Download this app as a template: app-role files + empty tables, no user
/// data, no chat, no API key, fresh app_id. Served as an attachment so the
/// shell button just navigates here.
async fn template_download(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let result = {
        let eng = app.engine.lock().unwrap();
        let name = crate::store::meta_get(&eng.db, "name")
            .ok()
            .flatten()
            .unwrap_or_else(|| "app".into());
        crate::store::export_template(&eng.db, &name).map(|b| (b, name))
    };
    match result {
        Ok((bytes, name)) => {
            let safe: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
                .collect();
            let fname = format!("{} template.uapp", safe.trim());
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{fname}\"")),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template export failed: {e}")).into_response(),
    }
}

/// Download the CURRENT app as a complete .uapp (all data included). Used to
/// save a scratch/unsaved app, but works for any app.
async fn app_download(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let result = {
        let eng = app.engine.lock().unwrap();
        let name = crate::store::meta_get(&eng.db, "name")
            .ok()
            .flatten()
            .unwrap_or_else(|| "app".into());
        crate::store::export_full(&eng.db, eng.crypt.as_ref().map(|k| k.passphrase.as_str()))
            .map(|b| (b, name))
    };
    match result {
        Ok((bytes, name)) => {
            let safe: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
                .collect();
            let fname = format!("{}.uapp", safe.trim());
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{fname}\"")),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("download failed: {e}")).into_response(),
    }
}

/// The hidden scratchpad page the shell embeds: an empty document with only
/// uapp.js loaded, registered as the "scratchpad" eval context. Code the AI
/// runs there has every tool and file but no app code — and its globals
/// persist between run_js calls until the shell tab closes.
///
/// The shell requests this only when a run_js scratchpad call needs it (see
/// `invoke_eval` and main.js), so it is not fetched at all in a session that
/// never uses the tool.
async fn scratch_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let mut resp = Html(SCRATCH_HTML).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

async fn ws_route(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // File uploads travel as base64 inside an RPC frame, so the socket has to
    // carry MAX_UPLOAD_BYTES * 4/3 plus JSON overhead (the defaults are 64/16
    // MiB, and an over-size frame kills the whole connection).
    let ws = ws
        .max_message_size(crate::rpc::MAX_UPLOAD_B64 + 4 * 1024 * 1024)
        .max_frame_size(crate::rpc::MAX_UPLOAD_B64 + 4 * 1024 * 1024);
    if !authed(&app, &headers, &q) {
        return deny();
    }
    // Cross-origin WebSocket connects aren't blocked by the same-origin policy,
    // so reject any Origin that isn't our own loopback (defense in depth on top
    // of the token). A missing Origin is a non-browser client (native webview,
    // tests) and is allowed.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        let ok = origin
            .split("//")
            .nth(1)
            .map(|hostport| {
                let host = hostport
                    .strip_prefix('[')
                    .and_then(|h| h.split(']').next())
                    .unwrap_or_else(|| hostport.split(':').next().unwrap_or(hostport));
                matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
            })
            .unwrap_or(false);
        if !ok {
            return deny();
        }
    }
    ws.on_upgrade(move |socket| ws_conn(app, socket))
}

static CONN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

async fn ws_conn(app: Arc<App>, socket: WebSocket) {
    struct ClientGuard(Arc<App>, u64);
    impl Drop for ClientGuard {
        fn drop(&mut self) {
            self.0.clients.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.0.conns.lock().unwrap().remove(&self.1);
            // Actions and contexts registered by this page die with it.
            self.0.actions.lock().unwrap().retain(|_, a| a.conn != self.1);
            self.0.contexts.lock().unwrap().retain(|(_, c)| *c != self.1);
        }
    }
    let conn_id = CONN_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    app.clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _guard = ClientGuard(app.clone(), conn_id);
    let (mut tx, mut rx) = socket.split();
    let mut events = app.events.subscribe();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    app.conns.lock().unwrap().insert(conn_id, out_tx.clone());

    // Fan-in: broadcast events + rpc responses -> socket. The periodic Ping
    // detects half-open connections: a suspended/frozen webview (e.g. Android
    // backgrounded) can leave the TCP socket looking alive. If the peer is
    // really gone the ping send eventually errors or times out and the socket
    // closes, firing the ClientGuard cleanup above.
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(v) => { if tx.send(Message::Text(v.to_string())).await.is_err() { break; } }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                resp = out_rx.recv() => match resp {
                    Some(s) => { if tx.send(Message::Text(s)).await.is_err() { break; } }
                    None => break,
                },
                _ = ping.tick() => {
                    if tx.send(Message::Ping(vec![])).await.is_err() { break; }
                }
            }
        }
    });

    while let Some(Ok(msg)) = rx.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or("").to_string();
        let params = req["params"].clone();
        // Transport-level messages that need this connection's identity.
        if method == "actions.register" {
            let count = {
                let mut reg = app.actions.lock().unwrap();
                reg.retain(|_, a| a.conn != conn_id);
                let mut count = 0;
                for a in params["actions"].as_array().cloned().unwrap_or_default() {
                    if let Some(name) = a["name"].as_str() {
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
                                conn: conn_id,
                            },
                        );
                        count += 1;
                    }
                }
                count
            };
            if !id.is_null() {
                let _ = out_tx.send(json!({"id": id, "result": {"ok": true, "count": count}}).to_string()).await;
            }
            continue;
        }
        if method == "log.write" {
            // Console output / uncaught errors forwarded from the live app
            // iframe. Fire-and-forget: buffered for the AI's read_console tool,
            // never echoed to a reply.
            let level = params["level"].as_str().unwrap_or("log");
            let text = params["text"].as_str().unwrap_or("");
            app.console_push(level, text);
            continue;
        }
        if method == "ctx.register" {
            let ctx = params["context"].as_str().unwrap_or("").to_string();
            if !ctx.is_empty() && ctx.len() <= 32 {
                let mut ctxs = app.contexts.lock().unwrap();
                ctxs.retain(|(c, id)| !(c == &ctx && *id == conn_id));
                ctxs.push((ctx, conn_id));
            }
            if !id.is_null() {
                let _ = out_tx.send(json!({"id": id, "result": {"ok": true}}).to_string()).await;
            }
            continue;
        }
        if method == "actions.result" || method == "eval.result" {
            let iid = params["id"].as_str().unwrap_or("").to_string();
            {
                let entry = app.invokes.lock().unwrap().remove(&iid);
                if let Some(tx2) = entry {
                    let _ = tx2.try_send(params.clone());
                }
            }
            if !id.is_null() {
                let _ = out_tx.send(json!({"id": id, "result": {"ok": true}}).to_string()).await;
            }
            continue;
        }
        // Server-internal markers: `_user_approved` is set by the AI loop after
        // the user's permission prompt, `_assistant` by builtin tools that act
        // on the model's behalf. Never trusted from the wire.
        let mut params = params;
        if let Some(o) = params.as_object_mut() {
            o.remove("_user_approved");
            o.remove("_assistant");
        }
        let app2 = app.clone();
        let out = out_tx.clone();
        // Each request handled on the blocking pool (SQLite is sync).
        tokio::spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || crate::rpc::dispatch(&app2, &method, params))
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("task panic: {e}")));
            let resp = match result {
                Ok(v) => json!({"id": id, "result": v}),
                Err(e) => json!({"id": id, "error": {"message": e.to_string()}}),
            };
            let _ = out.send(resp.to_string()).await;
        });
    }
    writer.abort();
}

// ---- unlock gate for encrypted files ----------------------------------
//
// An encrypted app can't be served until we have the password. We already own
// the app's listener, so we answer it with a tiny password page until the right
// password arrives, then hand the SAME socket to the app server (same port, so
// the page just reloads). It's a hand-rolled HTTP/1.1 handler for two trivial
// routes — cheaper than standing up a second server.

/// Serve the unlock page on `listener` until the correct master password for
/// `path` is entered, then return it. The listener stays open for the caller
/// to hand to the real app server.
pub async fn run_unlock(
    listener: &tokio::net::TcpListener,
    path: &std::path::Path,
) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    loop {
        tokio::select! {
            pw = rx.recv() => return pw.ok_or_else(|| anyhow::anyhow!("unlock cancelled")),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(handle_unlock_conn(stream, path.to_path_buf(), tx.clone()));
            }
        }
    }
}

/// One unlock connection: GET anything → the password page; POST /unlock →
/// verify. On success, reply then hand the password to `run_unlock` via `tx`.
async fn handle_unlock_conn(
    mut stream: tokio::net::TcpStream,
    path: std::path::PathBuf,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    use tokio::io::AsyncReadExt;
    // Read the request head (up to the blank line) plus any body bytes with it.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut req = head.split("\r\n").next().unwrap_or("").split(' ');
    let method = req.next().unwrap_or("");
    let route = req.next().unwrap_or("/").split('?').next().unwrap_or("/");

    if method == "POST" && route == "/unlock" {
        let clen = head
            .split("\r\n")
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("content-length") {
                    v.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < clen {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        let pw = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["password"].as_str().map(str::to_string))
            .unwrap_or_default();
        let ok = !pw.is_empty() && {
            let (p, pw) = (path.clone(), pw.clone());
            tokio::task::spawn_blocking(move || crate::cipher::open_file(&p, &pw).is_ok())
                .await
                .unwrap_or(false)
        };
        let json = if ok {
            r#"{"ok":true}"#
        } else {
            r#"{"ok":false,"error":"Incorrect password. Try again."}"#
        };
        let _ = write_http(&mut stream, "application/json", json.as_bytes()).await;
        if ok {
            let _ = tx.send(pw).await; // stops run_unlock; the app server takes over
        }
    } else {
        let _ = write_http(&mut stream, "text/html; charset=utf-8", UNLOCK_HTML.as_bytes()).await;
    }
}

async fn write_http(
    stream: &mut tokio::net::TcpStream,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        content_type,
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

const UNLOCK_HTML: &str = r#"<!doctype html><html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Locked — uapp</title>
<style>
  *{box-sizing:border-box} html,body{height:100%;margin:0}
  body{display:flex;align-items:center;justify-content:center;
    font:14px/1.5 system-ui,sans-serif;background:#23293a;color:#f2f4f8}
  .card{width:min(360px,90vw);background:#2b3145;border:1px solid #3a4260;
    border-radius:14px;padding:26px 26px 22px;
    box-shadow:0 16px 50px rgba(0,0,0,.4);text-align:center}
  .lock{font-size:30px;line-height:1;margin-bottom:8px}
  h1{font-size:17px;margin:2px 0 4px} p{margin:0 0 16px;color:#aeb6c8;font-size:13px}
  input{width:100%;padding:10px 12px;border-radius:9px;border:1px solid #46506e;
    background:#1d2230;color:#f2f4f8;font:inherit;margin-bottom:10px}
  input:focus{outline:none;border-color:#5b8cff}
  button{width:100%;padding:10px;border:none;border-radius:9px;background:#3f6de6;
    color:#fff;font:inherit;font-weight:600;cursor:pointer}
  button:hover{background:#4f7bf0} button:disabled{opacity:.6;cursor:default}
  .err{color:#ff9a9a;font-size:12px;min-height:16px;margin-top:8px}
</style></head><body>
<form class="card" id="f">
  <div class="lock">🔒</div>
  <h1>This app is encrypted</h1>
  <p>Enter the master password to open it.</p>
  <input id="pw" type="password" placeholder="Master password" autocomplete="current-password" autofocus>
  <button id="go" type="submit">Unlock</button>
  <div class="err" id="err"></div>
</form>
<script>
const f=document.getElementById('f'),pw=document.getElementById('pw'),
  err=document.getElementById('err'),go=document.getElementById('go');
f.addEventListener('submit',async(e)=>{
  e.preventDefault(); err.textContent=''; go.disabled=pw.disabled=true; go.textContent='Unlocking…';
  try{
    const r=await fetch('/unlock',{method:'POST',headers:{'content-type':'application/json'},
      body:JSON.stringify({password:pw.value})});
    const j=await r.json();
    if(j.ok){
      // Same socket becomes the app server; reload the same URL. The request
      // just queues until the app starts serving (a moment later).
      go.textContent='Opening…';
      setTimeout(()=>location.reload(),300);
    } else {
      err.textContent=j.error||'Incorrect password.';
      go.disabled=pw.disabled=false; go.textContent='Unlock'; pw.focus(); pw.select();
    }
  }catch(ex){
    err.textContent='Could not reach the app.';
    go.disabled=pw.disabled=false; go.textContent='Unlock';
  }
});
</script></body></html>"#;

/// Upload one file into the archive by POSTing its raw bytes (`?name=` is the
/// archive path). Uploads take this route instead of an RPC frame so a 100MB
/// file never has to exist as a base64 string in the page — the browser streams
/// the File object straight up, and the base64 the op log needs is built here.
async fn upload_file(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !authed(&app, &headers, &q) {
        return deny();
    }
    let Some(name) = q.get("name").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "upload needs ?name=").into_response();
    };
    if name.contains("..") || name.len() > 512 {
        return (StatusCode::BAD_REQUEST, "bad file name").into_response();
    }
    if body.len() > crate::rpc::MAX_UPLOAD_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("over the {}MB limit", crate::rpc::MAX_UPLOAD_BYTES / 1024 / 1024),
        )
            .into_response();
    }
    let name = crate::store::canon_name(name);
    // Encoding + the fsync'd op write are blocking work; keep them off the
    // async worker (a 100MB file is not a quick call).
    let bytes = body.len();
    let done = {
        let app = app.clone();
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &body);
            crate::rpc::dispatch(&app, "files.write", json!({"name": name, "b64": b64}))
        })
        .await
    };
    match done {
        Ok(Ok(_)) => axum::Json(json!({"ok": true, "name": name, "bytes": bytes})).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("upload failed: {e}")).into_response(),
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(shell))
        .route(
            "/upload",
            axum::routing::post(upload_file).layer(axum::extract::DefaultBodyLimit::max(
                crate::rpc::MAX_UPLOAD_BYTES + 1024 * 1024,
            )),
        )
        .route("/health", get(health))
        .route("/icon-256.png", get(|| async {
            (
                [(header::CONTENT_TYPE, "image/png".to_string()),
                 (header::CACHE_CONTROL, "max-age=86400".to_string())],
                include_bytes!("../assets/uapp-256.png") as &[u8],
            ).into_response()
        }))
        .route("/shell/*file", get(|axum::extract::Path(file): axum::extract::Path<String>| async move {
            match SHELL_MODULES.iter().find(|(name, _)| *name == file) {
                Some((_, body)) => static_asset(body, "text/javascript; charset=utf-8"),
                None => (axum::http::StatusCode::NOT_FOUND, "no such shell module").into_response(),
            }
        }))
        .route("/shell.css", get(|| async { static_asset(SHELL_CSS, "text/css; charset=utf-8") }))
        .route("/uapp.js", get(|| async { static_asset(UAPP_JS, "text/javascript; charset=utf-8") }))
        .route("/icons.js", get(|| async { static_asset(ICONS_JS, "text/javascript; charset=utf-8") }))
        .route("/scratch/", get(scratch_page))
        .route("/template.uapp", get(template_download))
        .route("/download.uapp", get(app_download))
        .route("/app/", get(app_root))
        .route("/app/*path", get(app_file))
        .route("/ws", get(ws_route))
        .fallback(get(root_file))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}

/// Baseline security headers on every response. Deliberately no CSP: app pages
/// run their own JS/styles by design, so a restrictive policy would break them
/// — origin isolation of untrusted apps is a separate, larger change.
async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, header::HeaderValue::from_static("no-referrer"));
    resp
}
