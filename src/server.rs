//! HTTP + WebSocket server for one open .uapp — the one router, in two modes.
//!
//! Private (the desktop app, `uapp open`): binds 127.0.0.1 (and [::1]) with a
//! per-session token; every route requires it — through the [`Owner`] /
//! [`Visitor`] extractors, never a handler-local check — and a request with a
//! non-loopback Host is refused (DNS rebinding). The app frame gets an origin
//! of its own, `localhost:<port>`, told apart from the shell's `127.0.0.1` by
//! Host, with its own token and an app-role socket (see `app_access`,
//! `guard_and_headers` and docs/design/app-isolation.md).
//!
//! Public (`uapp serve`, `App::public` set): the same router faces the world
//! as a website. Unauthenticated requests get exactly three things — the
//! site's `app/` pages at real URLs (`public_page`), the sanitized archive at
//! `GET /site.uapp`, and the browser-build assets of the editing chrome. The
//! token (from `--token`/`UAPP_TOKEN`, sent as `?t=`, the cookie, or
//! `Authorization: Bearer`) unlocks everything else — the shell, the
//! WebSocket, and `PUT /site.uapp`, which is how an edited copy is published
//! back. Rules that hold regardless of the token:
//!   * unauthenticated file lookups resolve STRICTLY under `app/` — never the
//!     `data/<name>` fallback of `store::sqlar_read`, which would publish
//!     every user-data file in the archive;
//!   * `/site.uapp` is `store::export_public` output: no chat, no config
//!     secrets, `data/` only with `--publish-data`.

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;

use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::{App, Role};
pub use crate::app::{AppAction, ConsoleEntry, PendingApproval, PendingQuestion, RunHandle};

const SHELL_HTML: &str = include_str!("shell/index.html");
const SHELL_CSS: &str = include_str!("shell/shell.css");
// The shell's ES modules (one Web Component or shared module per file),
// served under /shell/<name>. The list is generated at build time from
// src/shell/js/**.js (see build.rs) — drop a file in and it is served, with
// no hand-kept inventory to forget.
include!(concat!(env!("OUT_DIR"), "/shell_modules.rs"));

const UAPP_JS: &str = include_str!("shell/uapp.js");
const UAPP_DTS: &str = include_str!("shell/uapp.d.ts");
const ICONS_JS: &str = include_str!("shell/icons.js");
const SCRATCH_HTML: &str = include_str!("shell/scratch.html");

fn cookie_name(app: &App) -> String {
    format!(
        "uapp_t_{}",
        app.port.load(std::sync::atomic::Ordering::Relaxed)
    )
}

/// The app frame's cookie. Browsers drop it when the frame is cross-site
/// with the shell (which it always is), so it mostly serves non-browser
/// clients and a page opened in a tab of its own; [`app_access`] has the
/// paths a framed page actually uses.
fn app_cookie_name(app: &App) -> String {
    format!(
        "uapp_a_{}",
        app.port.load(std::sync::atomic::Ordering::Relaxed)
    )
}

/// Is this request addressed to the app frame's origin (`localhost:<port>`)?
/// Decided by Host alone, so the shell (`127.0.0.1:<port>`) and the app
/// never share a route, a cookie or a role.
fn on_app_origin(app: &App, headers: &HeaderMap) -> bool {
    if app.app_origin().is_none() {
        return false;
    }
    let port = app.port.load(std::sync::atomic::Ordering::Relaxed);
    headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| host_and_port(h, 80))
        .is_some_and(|(h, p)| h == "localhost" && p == port)
}

/// Is one of the `a=` parameters in `query` the app token? (An app may use
/// `?a=` for its own purposes too, so every one of them is looked at.)
fn query_has_app_token(app: &App, query: Option<&str>) -> bool {
    query
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.strip_prefix("a="))
        .any(|t| crate::secrets::token_eq(t, &app.app_token))
}

/// Does the request carry the app token (`?a=` or the app cookie)?
fn has_app_token(app: &App, headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    use crate::secrets::token_eq;
    if query_has_app_token(app, uri.query()) {
        return true;
    }
    let name = app_cookie_name(app);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == name && token_eq(v, &app.app_token))
}

/// A `Referer` on the app origin whose URL carries the app token: a request
/// made by one of the app's own pages. They keep `?a=` in their address
/// (uapp.js sees to that) and are served with `Referrer-Policy: same-origin`,
/// so their scripts, images and fetches all send it, and no other site does.
fn referer_has_app_token(app: &App, headers: &HeaderMap) -> bool {
    let Some(ours) = app.app_origin() else {
        return false;
    };
    headers
        .get(header::REFERER)
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.strip_prefix(ours.as_str()))
        .filter(|rest| rest.starts_with('/'))
        .and_then(|rest| rest.split('#').next())
        .is_some_and(|rest| query_has_app_token(app, rest.split_once('?').map(|(_, q)| q)))
}

/// How much of the app an app-origin request may have.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppAccess {
    /// Shows the app token: the whole archive, `data/` included.
    Full,
    /// No token, but the request says it's the app's own (see
    /// [`app_access`]): only the app's code, the files under `app/`.
    Code,
}

/// What this app-origin request may have, if anything.
///
/// The shell loads the frame cross-site with `?a=<app token>`, every app page
/// keeps it in its URL, and the page's requests carry it in their `Referer`
/// (or on the URL, or in the app cookie where the browser keeps it). That's
/// [`AppAccess::Full`].
///
/// Some of the app's requests can't carry it: a worker's own fetches and
/// imports (its URL is the script's), `url()`s inside a stylesheet, nested
/// module imports, anything under `referrerpolicy="no-referrer"`. They are
/// same-origin, which `Sec-Fetch-Site` says (or, on an engine too old to send
/// it, a same-origin `Referer`); `none` is a URL typed into the address bar.
/// A browser page elsewhere can't fake either, but any local program can, so
/// they only get [`AppAccess::Code`]: the app's code, never its data.
fn app_access(app: &App, headers: &HeaderMap, uri: &axum::http::Uri) -> Option<AppAccess> {
    let ours = app.app_origin()?;
    if let Some(o) = headers.get(header::ORIGIN)
        && o.to_str().ok() != Some(ours.as_str())
    {
        return None;
    }
    if has_app_token(app, headers, uri) || referer_has_app_token(app, headers) {
        return Some(AppAccess::Full);
    }
    let own = match headers.get("sec-fetch-site").map(|v| v.to_str()) {
        Some(site) => matches!(site, Ok("same-origin" | "none")),
        None => headers
            .get(header::REFERER)
            .and_then(|r| r.to_str().ok())
            .and_then(|r| r.strip_prefix(ours.as_str()))
            .is_some_and(|rest| rest.starts_with('/')),
    };
    own.then_some(AppAccess::Code)
}

/// DNS-rebinding guard: the server binds 127.0.0.1 only, so a legitimate
/// request always arrives with a loopback Host. A rebinding attack (attacker
/// domain re-pointed at 127.0.0.1) carries the attacker's hostname here.
fn host_is_local(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return true; // HTTP/1.0 / some native webviews omit Host
    };
    is_loopback(hostname_of(host))
}

/// `host[:port]` -> `host`, with `[::1]:port` handled.
fn hostname_of(hostport: &str) -> &str {
    hostport
        .strip_prefix('[')
        .and_then(|h| h.split(']').next())
        .unwrap_or_else(|| hostport.split(':').next().unwrap_or(hostport))
}

/// Does this request carry the token (`?t=`, the cookie, or `Authorization:
/// Bearer`)? In private mode a non-loopback Host fails outright (DNS rebinding).
///
/// On a public site `?t=` only opens the shell (`GET /`, which then sets the
/// cookie): anywhere else a token in the URL would end up in proxy and CDN
/// logs for nothing, since the shell's own requests carry the cookie and a
/// publish sends `Authorization: Bearer`.
fn authed(app: &App, headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    use crate::secrets::token_eq;
    if app.public.is_none() && !host_is_local(headers) {
        return false;
    }
    if app.token.is_empty() {
        return false;
    }
    let url_token_ok = app.public.is_none() || uri.path() == "/";
    let q_token = uri
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("t="));
    if url_token_ok && q_token.is_some_and(|t| token_eq(t, &app.token)) {
        return true;
    }
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| token_eq(t.trim(), &app.token))
    {
        return true;
    }
    let name = cookie_name(app);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == name && token_eq(v, &app.token))
}

/// Did this request come from one of our own pages, or from no browser at all?
///
/// The token cookie can't tell: cookies aren't scoped to a port, so a page on
/// any other loopback port is "same-site" and the browser sends our
/// SameSite=Strict cookie along with its requests (a cross-port
/// `POST /upload` as text/plain needs no preflight and used to land). So on
/// top of the token, a browser request must say it is same-origin:
///   * `Sec-Fetch-Site`, when present, is `same-origin` or `none` (typed in,
///     a bookmark, the native window's first load);
///   * `Origin`, when present, is exactly our own scheme://host:port. Browsers
///     send it on every WebSocket handshake and every non-GET request, so its
///     absence there means a non-browser client (the CLI, curl, the tests),
///     which has to present the token itself.
fn from_own_origin(app: &App, headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site")
        && !matches!(site.to_str(), Ok("same-origin" | "none"))
    {
        return false;
    }
    match headers.get(header::ORIGIN) {
        Some(o) => o.to_str().is_ok_and(|o| origin_is_self(app, headers, o)),
        None => true,
    }
}

/// `origin` (scheme://host[:port]) names the host and port this request was
/// sent to. The Host header says what that was; in private mode it must also
/// be our loopback port (DNS rebinding is refused earlier, by Host).
fn origin_is_self(app: &App, headers: &HeaderMap, origin: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false; // "null" (sandboxed frames, file://) and junk
    };
    let default = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return false,
    };
    let Some((host, port)) = host_and_port(authority, default) else {
        return false;
    };
    let ours = app.port.load(std::sync::atomic::Ordering::Relaxed);
    if app.public.is_none() && (port != ours || !is_loopback(&host)) {
        return false;
    }
    match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(h) => host_and_port(h, default) == Some((host, port)),
        None => app.public.is_none(),
    }
}

/// `host[:port]` (as in a Host header or an origin) -> lowercased host and the
/// port, `default` when none is given. `[::1]:8080` style included.
fn host_and_port(authority: &str, default: u16) -> Option<(String, u16)> {
    if authority.is_empty() || authority.contains(['/', '@', ' ']) {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, rest) = v6.split_once(']')?;
            (h, rest.strip_prefix(':'))
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => default,
    };
    Some((host.to_ascii_lowercase(), port))
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

/// Who is asking — decided ONCE per request, by these two extractors, so no
/// handler carries its own token check or its own "but in public mode…".
///
/// [`Owner`] is for routes that exist only for the token holder (shell, ws,
/// downloads, upload, scratchpad, publish): without the token the request is
/// refused with 403, in either mode. [`Visitor`] is for the page routes,
/// where an anonymous request on a public site is a legitimate reader and gets
/// the site's page for that URL; in private mode it is again a 403.
pub struct Owner;

pub enum Visitor {
    /// Holds the token: the shell, the raw archive, everything.
    Owner,
    /// The app frame, on its own origin (see [`app_access`]).
    App(AppAccess),
    /// Anonymous reader of a public site.
    Reader,
}

impl axum::extract::FromRequestParts<Arc<App>> for Owner {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &Arc<App>,
    ) -> Result<Self, Response> {
        if on_app_origin(app, &parts.headers) {
            Err(cross_origin())
        } else if !authed(app, &parts.headers, &parts.uri) {
            Err(deny())
        } else if !from_own_origin(app, &parts.headers) {
            Err(cross_origin())
        } else {
            Ok(Owner)
        }
    }
}

impl axum::extract::FromRequestParts<Arc<App>> for Visitor {
    type Rejection = Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        app: &Arc<App>,
    ) -> Result<Self, Response> {
        if on_app_origin(app, &parts.headers) {
            return match app_access(app, &parts.headers, &parts.uri) {
                Some(a) => Ok(Visitor::App(a)),
                None => Err(deny()),
            };
        }
        // A token-holder's request from another origin is no owner's: on a
        // public site it still gets what any reader gets.
        if authed(app, &parts.headers, &parts.uri) && from_own_origin(app, &parts.headers) {
            Ok(Visitor::Owner)
        } else if app.public.is_some() {
            Ok(Visitor::Reader)
        } else {
            Err(deny())
        }
    }
}

fn deny() -> Response {
    (StatusCode::FORBIDDEN, "uapp: missing or bad token").into_response()
}

fn cross_origin() -> Response {
    (
        StatusCode::FORBIDDEN,
        "uapp: refused a request from another origin",
    )
        .into_response()
}

pub(crate) fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

async fn shell(State(app): State<Arc<App>>, who: Visitor, headers: HeaderMap) -> Response {
    match who {
        // A public site's front page is the site, not the shell.
        Visitor::Reader => return public_page(&app, "/", &headers),
        // On the app's own origin, `/` simply is the app.
        Visitor::App(a) => return serve_sqlar(&app, "index.html", &headers, a == AppAccess::Code),
        Visitor::Owner => {}
    }
    // Sharing the shell's origin, a root-absolute `href="/"` followed INSIDE
    // the app frame means the app's front page, not a second shell nested in
    // the first. WebViews and browsers label frame navigations with
    // Sec-Fetch-Dest: iframe.
    if app.app_origin().is_none()
        && headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("iframe")
    {
        return serve_sqlar(&app, "index.html", &headers, false);
    }
    // Where the shell loads the app frame from: its own origin, with the app
    // token for that first (cross-site) load, or next to the shell.
    let app_src = match app.app_origin() {
        Some(o) => format!("{o}/app/?a={}", app.app_token),
        None => "/app/".to_string(),
    };
    // Open with the toolbar this app saved for itself (see toolbar.rs). Set in
    // the HTML rather than by the shell after its first RPC: a class that
    // arrives a round trip late means the bar paints and then vanishes, which
    // reads as a glitch every single time the app is opened.
    let topbar_class = {
        let eng = app.eng();
        let saved = crate::store::config_get(&eng.db, "toolbar").ok().flatten();
        if crate::toolbar::settings(saved.as_ref())["hidden"] == json!(true) {
            " class=\"hidden\""
        } else {
            ""
        }
    };
    // The name comes from the shared .uapp file — escape it, or a hostile
    // file gets stored XSS in the (privileged) shell origin.
    let mut resp = Html(
        SHELL_HTML
            .replace("{{APP_NAME}}", &html_escape(&app.name))
            .replace("{{TOPBAR_CLASS}}", topbar_class)
            .replace("{{APP_SRC}}", &app_src),
    )
    .into_response();
    // Refresh the cookie so the iframe + ws can auth without the token in URL
    // (the shell then drops ?t= from the address bar, see core.js).
    // HttpOnly: app pages run arbitrary (AI-written) JS on this origin and
    // must not be able to read the token — on a public site it is the
    // publish secret. Secure there too: never over plain HTTP. The token was
    // checked to be cookie-safe at startup (`secrets::check_token`).
    if let Ok(v) = header::HeaderValue::from_str(&format!(
        "{}={}; Path=/; SameSite=Strict; HttpOnly{}",
        cookie_name(&app),
        app.token,
        if app.public.is_some() { "; Secure" } else { "" }
    )) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    // The shell is embedded in the binary and changes on every upgrade;
    // stale-cached shells would keep showing the old UI.
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if let Ok(csp) = header::HeaderValue::from_str(&shell_csp(&app, &headers)) {
        resp.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    resp
}

/// The shell document's CSP. The shell's own code is all served files (no
/// inline script, no eval), so script-src 'self' holds, and anything injected
/// into it (a hostile app name, chat markdown) can't run. Styles stay inline
/// because the Web Components carry theirs in their templates. frame-src is
/// broad on purpose: it governs where the APP frame may navigate, and an app
/// may take its own frame anywhere; the app's own origin is named first
/// anyway. The socket is named explicitly since older WebKit doesn't count
/// ws: as 'self'. Only the shell gets this; app pages run whatever their
/// authors wrote and get just frame-ancestors (see `security_headers`).
fn shell_csp(app: &App, headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            h.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
        });
    let sockets = host
        .map(|h| format!(" ws://{h} wss://{h}"))
        .unwrap_or_default();
    let app_frame = app
        .app_origin()
        .map(|o| format!(" {o}"))
        .unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; \
         connect-src 'self'{sockets}; frame-src 'self'{app_frame} blob: http: https:; worker-src 'self'; \
         object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'self'"
    )
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

/// One archive file for `name`. `code_only` (a request without the app
/// token, see [`app_access`]) serves only what resolves under `app/`: a name
/// that lands in `data/` (directly, through `/app/data/…`, or as a root-level
/// name's fallback) is refused.
fn serve_sqlar(app: &App, name: &str, headers: &HeaderMap, code_only: bool) -> Response {
    if name.contains("..") {
        return deny();
    }
    let mut needs_token = false;
    // Read + inflate under the lock, then release it: building the response
    // (and slicing a range out of a large video) must not block every other
    // request on the engine mutex.
    // Pretty URLs, as a static host would resolve them: "posts/x/" and
    // "posts/x" -> posts/x/index.html, "admin" -> admin.html. Generated sites
    // (and the public `uapp serve` router) link this way.
    let (data, name) = {
        let eng = app.eng();
        let read = |n: &str, needs_token: &mut bool| {
            if !code_only {
                return crate::store::sqlar_read(&eng.db, n);
            }
            match crate::store::resolve_name(&eng.db, n)? {
                Some(r) if r.starts_with(crate::store::APP_DIR) => {
                    crate::store::sqlar_read(&eng.db, &r)
                }
                Some(_) => {
                    *needs_token = true;
                    Ok(None)
                }
                None => Ok(None),
            }
        };
        let mut found = (read(name, &mut needs_token), name.to_string());
        if matches!(found.0, Ok(None)) {
            let bare = name.trim_end_matches('/');
            let last = bare.rsplit('/').next().unwrap_or(bare);
            if !last.contains('.') || name.ends_with('/') {
                let mut tries = vec![format!("{bare}/index.html")];
                if !name.ends_with('/') && !bare.is_empty() {
                    tries.insert(0, format!("{bare}.html"));
                }
                for t in tries {
                    match read(&t, &mut needs_token) {
                        Ok(Some(b)) => {
                            found = (Ok(Some(b)), t);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            found = (Err(e), t);
                            break;
                        }
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
                            (
                                header::CONTENT_RANGE,
                                format!("bytes {start}-{end}/{total}"),
                            ),
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
        Ok(None) if needs_token => {
            (StatusCode::FORBIDDEN, "uapp: data files need the app token").into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("no such file in app: {name}"),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn app_file(
    State(app): State<Arc<App>>,
    AxPath(path): AxPath<String>,
    who: Visitor,
    headers: HeaderMap,
) -> Response {
    // The editing chrome's service worker normally answers /app/* from the
    // visitor's own copy; when it isn't in control, the server's pages are
    // the right fallback.
    if let Visitor::Reader = who {
        return public_page(&app, &format!("/{path}"), &headers);
    }
    // The shell can still read the archive at /app/ on its own origin (file
    // previews, downloads), but a page from there never runs as the shell:
    // `guard_and_headers` sandboxes HTML and SVG on that side.
    let name = if path.is_empty() {
        "index.html".to_string()
    } else {
        path
    };
    serve_sqlar(&app, &name, &headers, code_only(&who))
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

fn code_only(who: &Visitor) -> bool {
    matches!(who, Visitor::App(AppAccess::Code))
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
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
    who: Visitor,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let path = percent_decode(uri.path());
    // The browser build's assets win over site files of the same name — the
    // editing chrome cannot boot if the site shadows its loader.
    if let Some(chrome) = &app.chrome
        && let Some(bytes) = chrome.bundle.get(path.as_str())
    {
        let etag = etag_for(bytes);
        if if_none_match(&headers, &etag) {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }
        return (
            [
                (header::CONTENT_TYPE, content_type_for(&path).to_string()),
                (header::ETAG, etag),
                // Changes only with the server image, but must never outlive
                // a deploy: revalidate, and let the ETag make that cheap.
                (
                    header::CACHE_CONTROL,
                    "public, max-age=0, must-revalidate".to_string(),
                ),
            ],
            bytes.clone(),
        )
            .into_response();
    }
    match who {
        Visitor::Reader => return public_page(&app, &path, &headers),
        // With the app on its own origin, root-absolute files are its business.
        Visitor::Owner if app.app_origin().is_some() => {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
        _ => {}
    }
    let name = path.trim_start_matches('/');
    if name.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    serve_sqlar(&app, name, &headers, code_only(&who))
}

async fn app_root(state: State<Arc<App>>, who: Visitor, headers: HeaderMap) -> Response {
    app_file(state, AxPath(String::new()), who, headers).await
}

/// Liveness probe. The version header lets `uapp open` tell when the server
/// it is about to reuse for this file is an older build than itself.
async fn health(State(app): State<Arc<App>>) -> Response {
    // The key names WHICH file this server has open: a stale registry entry
    // whose port was recycled by another uapp must not be mistaken for ours.
    let key = crate::registry::key(&app.eng().path);
    // Android's chat keep-alive service polls this to know when it can stop
    // holding the process out of the cached-apps freezer.
    let busy = if app.ai_busy_any() { "1" } else { "0" };
    (
        [
            ("x-uapp-version", env!("CARGO_PKG_VERSION").to_string()),
            ("x-uapp-key", key),
            ("x-uapp-ai-busy", busy.to_string()),
        ],
        "ok",
    )
        .into_response()
}

/// Download this app as a template: app-role files + empty tables, no user
/// data, no chat, no API key, fresh app_id. Served as an attachment so the
/// shell button just navigates here.
async fn template_download(State(app): State<Arc<App>>, _who: Owner) -> Response {
    let result = {
        let eng = app.eng();
        let name = app_display_name(&eng);
        crate::store::export_template(&eng.db, &name).map(|b| (b, name))
    };
    match result {
        Ok((bytes, name)) => attachment(bytes, &format!("{name} template")),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template export failed: {e}"),
        )
            .into_response(),
    }
}

/// Download the CURRENT app as a complete .uapp (all data included). Used to
/// save a scratch/unsaved app, but works for any app.
async fn app_download(State(app): State<Arc<App>>, _who: Owner) -> Response {
    let result = {
        let eng = app.eng();
        let name = app_display_name(&eng);
        crate::store::export_full(&eng.db, eng.crypt.as_ref().map(|k| k.passphrase.as_str()))
            .map(|b| (b, name))
    };
    match result {
        Ok((bytes, name)) => attachment(bytes, &name),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("download failed: {e}"),
        )
            .into_response(),
    }
}

fn app_display_name(eng: &crate::engine::Engine) -> String {
    crate::store::meta_get(&eng.db, "name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "app".into())
}

/// A `.uapp` download named after the app. The filename must be a valid
/// header value: non-ASCII characters are dropped (a name like "予算" used to
/// turn the whole download into a 500), and the empty result falls back to
/// "app". The full name travels in RFC 5987 `filename*` for browsers that
/// read it.
fn attachment(bytes: Vec<u8>, stem: &str) -> Response {
    let ascii: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .collect();
    let ascii = ascii.trim();
    let ascii = if ascii.is_empty() { "app" } else { ascii };
    let utf8: String = stem
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{ascii}.uapp\"; filename*=UTF-8''{utf8}.uapp"),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// The hidden scratchpad page the shell embeds: an empty document with only
/// uapp.js loaded, registered as the "scratchpad" eval context. Code the AI
/// runs there has every tool and file but no app code — and its globals
/// persist between run_js calls until the shell tab closes.
///
/// The shell requests this only when a run_js scratchpad call needs it (see
/// `invoke_eval` and main.js), so it is not fetched at all in a session that
/// never uses the tool.
///
/// It runs code the assistant wrote, so with isolation on it lives on the app
/// origin and gets the app's role (`guard_and_headers` refuses it on the
/// shell's).
async fn scratch_page(who: Visitor) -> Response {
    if let Visitor::Reader = who {
        return deny();
    }
    let mut resp = Html(SCRATCH_HTML).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    resp
}

/// One socket route for both sides. The shell's connections are the owner's;
/// on the app origin a connection needs the app token (`?a=` or its cookie)
/// and the app's own Origin, and gets the app role.
async fn ws_route(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> Response {
    let role = if on_app_origin(&app, &headers) {
        let origin_ok = match headers.get(header::ORIGIN) {
            Some(o) => o.to_str().ok() == app.app_origin().as_deref(),
            None => true, // not a browser: the token alone
        };
        if !origin_ok {
            return cross_origin();
        }
        if !has_app_token(&app, &headers, &uri) {
            return deny();
        }
        Role::App
    } else if !authed(&app, &headers, &uri) {
        return deny();
    } else if !from_own_origin(&app, &headers) {
        return cross_origin();
    } else {
        Role::Owner
    };
    // File uploads travel as base64 inside an RPC frame, so the socket has to
    // carry MAX_UPLOAD_BYTES * 4/3 plus JSON overhead (the defaults are 64/16
    // MiB, and an over-size frame kills the whole connection).
    let ws = ws
        .max_message_size(crate::rpc::MAX_UPLOAD_B64 + 4 * 1024 * 1024)
        .max_frame_size(crate::rpc::MAX_UPLOAD_B64 + 4 * 1024 * 1024);
    // Cross-origin WebSocket connects aren't blocked by the same-origin
    // policy; the checks above have already required our own Origin.
    ws.on_upgrade(move |socket| ws_conn(app, socket, role))
}

static CONN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// How much one connection may have running at once. Every RPC takes a
/// blocking-pool thread until it returns (some wait minutes on a prompt), and
/// its message stays in memory meanwhile, so a request costs 1 plus 1 per MiB
/// of message. Over budget, the request is refused straight away rather than
/// queued: queuing would stop this socket being read, and the replies that
/// in-flight calls are waiting for (an app action's result) arrive on it.
const CONN_BUDGET: usize = 256;

async fn ws_conn(app: Arc<App>, socket: WebSocket, role: Role) {
    struct ClientGuard(Arc<App>, u64);
    impl Drop for ClientGuard {
        fn drop(&mut self) {
            self.0.conn_close(self.1);
        }
    }
    let conn_id = CONN_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _guard = ClientGuard(app.clone(), conn_id);
    let (mut tx, mut rx) = socket.split();
    let mut events = app.events.subscribe();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let budget = Arc::new(tokio::sync::Semaphore::new(CONN_BUDGET));
    {
        let tx = out_tx.clone();
        app.conn_open(
            conn_id,
            std::sync::Arc::new(move |m: String| tx.blocking_send(m).is_ok()),
        );
    }

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
                    Ok(v) if role == Role::App && !App::app_may_see(&v) => continue,
                    Ok(v) => { if tx.send(Message::Text(v.to_string().into())).await.is_err() { break; } }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                resp = out_rx.recv() => match resp {
                    Some(s) => { if tx.send(Message::Text(s.into())).await.is_err() { break; } }
                    None => break,
                },
                _ = ping.tick() => {
                    if tx.send(Message::Ping(Default::default())).await.is_err() { break; }
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
        if let Some(reply) = app.conn_message(conn_id, &method, &params) {
            if !id.is_null() {
                let _ = out_tx
                    .send(json!({"id": id, "result": reply}).to_string())
                    .await;
            }
            continue;
        }
        let cost = (1 + (text.len() >> 20)).min(CONN_BUDGET) as u32;
        let Ok(permit) = budget.clone().try_acquire_many_owned(cost) else {
            if !id.is_null() {
                let msg = "too many requests in flight on this connection; wait for some to finish";
                let _ = out_tx
                    .send(json!({"id": id, "error": {"message": msg}}).to_string())
                    .await;
            }
            continue;
        };
        let app2 = app.clone();
        let out = out_tx.clone();
        // Each request handled on the blocking pool (SQLite is sync).
        tokio::spawn(async move {
            let _permit = permit;
            let result = tokio::task::spawn_blocking(move || {
                crate::rpc::dispatch_as(&app2, role, &method, params)
            })
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

/// Listen on `[::1]:<port>` next to `127.0.0.1:<port>`, for the app origin
/// (`localhost` resolves to either). Returns the listener, if any, and
/// whether the app frame can have its own origin at all:
///   * no IPv6 here: `localhost` is 127.0.0.1, which we already have;
///   * `[::1]:<port>` taken by someone else: `localhost` might reach THEM
///     with the app token, so the app stays on the shell's origin (and the
///     shell says it isn't isolated).
pub async fn bind_app_v6(port: u16) -> (Option<tokio::net::TcpListener>, bool) {
    match tokio::net::TcpListener::bind(("::1", port)).await {
        Ok(l) => (Some(l), true),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!(
                "[::1]:{port} is in use by another program; the app frame shares the editor's origin this session"
            );
            (None, false)
        }
        Err(e) => {
            use std::net::ToSocketAddrs;
            let v4 = ("localhost", port)
                .to_socket_addrs()
                .is_ok_and(|mut a| a.any(|a| a.ip() == std::net::Ipv4Addr::LOCALHOST));
            if !v4 {
                tracing::warn!(
                    "can't listen on [::1]:{port} ({e}) and localhost doesn't resolve to 127.0.0.1; the app frame shares the editor's origin this session"
                );
            }
            (None, v4)
        }
    }
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
    let gate = Arc::new(UnlockGate {
        port: listener.local_addr()?.port(),
        tries: tokio::sync::Mutex::new(Tries::default()),
    });
    loop {
        tokio::select! {
            pw = rx.recv() => return pw.ok_or_else(|| anyhow::anyhow!("unlock cancelled")),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                tokio::spawn(handle_unlock_conn(stream, path.to_path_buf(), tx.clone(), gate.clone()));
            }
        }
    }
}

/// What every unlock connection shares. The page is reachable by anything
/// that can reach the port, so password guesses are rate-limited: one at a
/// time (the lock is held while a guess is checked), and after a few misses
/// each further guess has to wait, twice as long every time.
struct UnlockGate {
    port: u16,
    tries: tokio::sync::Mutex<Tries>,
}

#[derive(Default)]
struct Tries {
    failures: u32,
    not_before: Option<tokio::time::Instant>,
}

/// Misses allowed before the waiting starts (typos happen).
const UNLOCK_FREE_TRIES: u32 = 3;
const UNLOCK_MAX_WAIT_SECS: u64 = 60;
/// A password is never this long; anything bigger isn't a real attempt.
const UNLOCK_MAX_BODY: usize = 4096;

/// Answer every request on this already-bound listener with one fixed response
/// for up to `secs` seconds, then return. Used when the browser/native window
/// was already pointed at this port (the unlock page fired) but the app can no
/// longer be served here — e.g. another instance won the file between unlock
/// and open. Without it the waiting tab's reload hits a closed socket
/// ("connection refused") with no explanation.
pub async fn run_notice(
    listener: &tokio::net::TcpListener,
    redirect_to: Option<String>,
    message: &str,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            accepted = listener.accept() => {
                let Ok((mut stream, _)) = accepted else { return };
                let redirect = redirect_to.clone();
                let body = message.to_string();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let head = match &redirect {
                        Some(url) => format!(
                            "HTTP/1.1 302 Found\r\nlocation: {url}\r\ncache-control: no-store\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                        ),
                        None => format!(
                            "HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                            body.len(), body
                        ),
                    };
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        }
    }
}

/// One unlock connection: GET anything → the password page; POST /unlock →
/// verify. On success, reply then hand the password to `run_unlock` via `tx`.
///
/// Hand-rolled, so the same rules as the real server are spelled out here: a
/// loopback Host (DNS rebinding), and for the POST an Origin of exactly this
/// page plus a JSON body (so another site can't submit guesses with a form or
/// a no-preflight fetch), at most 4 KB of it.
async fn handle_unlock_conn(
    mut stream: tokio::net::TcpStream,
    path: std::path::PathBuf,
    tx: tokio::sync::mpsc::Sender<String>,
    gate: Arc<UnlockGate>,
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
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let host = header("host");
    if let Some(h) = &host
        && !host_and_port(h, 80).is_some_and(|(h, _)| is_loopback(&h))
    {
        let _ = write_status(
            &mut stream,
            "403 Forbidden",
            "application/json",
            br#"{"ok":false}"#,
        )
        .await;
        return;
    }

    if method == "POST" && route == "/unlock" {
        let refuse = |status: &'static str, msg: &str| {
            (status, json!({"ok": false, "error": msg}).to_string())
        };
        let origin_ok = match (header("origin"), &host) {
            (Some(o), Some(h)) => {
                o == format!("http://{h}")
                    && host_and_port(h, 80).is_some_and(|(_, p)| p == gate.port)
            }
            _ => false,
        };
        let is_json = header("content-type").is_some_and(|c| {
            c.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
        let clen = header("content-length").and_then(|v| v.parse::<usize>().ok());
        let early = if !origin_ok {
            Some(refuse(
                "403 Forbidden",
                "This page can only be used from the app's own window.",
            ))
        } else if !is_json {
            Some(refuse("415 Unsupported Media Type", "Expected JSON."))
        } else if clen.unwrap_or(usize::MAX) > UNLOCK_MAX_BODY {
            Some(refuse(
                "413 Payload Too Large",
                "That request is too large.",
            ))
        } else {
            None
        };
        if let Some((status, body)) = early {
            let _ = write_status(&mut stream, status, "application/json", body.as_bytes()).await;
            return;
        }
        let clen = clen.unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < clen {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        body.truncate(clen);
        let pw = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v["password"].as_str().map(str::to_string))
            .unwrap_or_default();

        // One guess at a time, and none before the back-off has passed.
        let mut tries = gate.tries.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(t) = tries.not_before.filter(|t| *t > now) {
            let secs = (t - now).as_secs() + 1;
            let msg = format!("Too many wrong passwords. Try again in {secs} s.");
            let (status, body) = refuse("429 Too Many Requests", &msg);
            drop(tries);
            let _ = write_status(&mut stream, status, "application/json", body.as_bytes()).await;
            return;
        }
        let ok = !pw.is_empty() && {
            let (p, pw) = (path.clone(), pw.clone());
            tokio::task::spawn_blocking(move || crate::cipher::open_file(&p, &pw).is_ok())
                .await
                .unwrap_or(false)
        };
        if ok {
            *tries = Tries::default();
        } else {
            tries.failures += 1;
            if tries.failures >= UNLOCK_FREE_TRIES {
                let wait = 1u64 << (tries.failures - UNLOCK_FREE_TRIES).min(6);
                tries.not_before =
                    Some(now + std::time::Duration::from_secs(wait.min(UNLOCK_MAX_WAIT_SECS)));
            }
        }
        drop(tries);
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
        let _ = write_http(
            &mut stream,
            "text/html; charset=utf-8",
            UNLOCK_HTML.as_bytes(),
        )
        .await;
    }
}

async fn write_http(
    stream: &mut tokio::net::TcpStream,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_status(stream, "200 OK", content_type, body).await
}

async fn write_status(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\n\
         x-content-type-options: nosniff\r\ncontent-security-policy: frame-ancestors 'none'\r\n\
         connection: close\r\n\r\n",
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
    _who: Owner,
    Query(q): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(name) = q.get("name").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "upload needs ?name=").into_response();
    };
    if name.contains("..") || name.len() > 512 {
        return (StatusCode::BAD_REQUEST, "bad file name").into_response();
    }
    if body.len() > crate::rpc::MAX_UPLOAD_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "over the {}MB limit",
                crate::rpc::MAX_UPLOAD_BYTES / 1024 / 1024
            ),
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
            crate::rpc::dispatch_untrusted(&app, "files.write", json!({"name": name, "b64": b64}))
        })
        .await
    };
    match done {
        Ok(Ok(_)) => axum::Json(json!({"ok": true, "name": name, "bytes": bytes})).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("upload failed: {e}"),
        )
            .into_response(),
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
        .route(
            "/icon-256.png",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "image/png".to_string()),
                        (header::CACHE_CONTROL, "max-age=86400".to_string()),
                    ],
                    include_bytes!("../assets/uapp-256.png") as &[u8],
                )
                    .into_response()
            }),
        )
        .route(
            "/shell/{*file}",
            get(
                |axum::extract::Path(file): axum::extract::Path<String>| async move {
                    match SHELL_MODULES.iter().find(|(name, _)| *name == file) {
                        Some((_, body)) => static_asset(body, "text/javascript; charset=utf-8"),
                        None => (axum::http::StatusCode::NOT_FOUND, "no such shell module")
                            .into_response(),
                    }
                },
            ),
        )
        .route(
            "/shell.css",
            get(|| async { static_asset(SHELL_CSS, "text/css; charset=utf-8") }),
        )
        .route(
            "/uapp.js",
            get(|| async { static_asset(UAPP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/uapp.d.ts",
            get(|| async { static_asset(UAPP_DTS, "text/plain; charset=utf-8") }),
        )
        .route(
            "/icons.js",
            get(|| async { static_asset(ICONS_JS, "text/javascript; charset=utf-8") }),
        )
        .route("/scratch/", get(scratch_page))
        .route("/template.uapp", get(template_download))
        .route("/download.uapp", get(app_download))
        .route("/app/", get(app_root))
        .route("/app/{*path}", get(app_file))
        .route("/ws", get(ws_route))
        .route(
            "/site.uapp",
            get(site_archive)
                .put(site_publish)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::rpc::MAX_UPLOAD_BYTES + 1024 * 1024,
                )),
        )
        .fallback(get(root_file))
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            guard_and_headers,
        ))
        .with_state(app)
}

/// Routes that only exist on the shell's origin. On the app origin they 404
/// (an app's own file of the same name was shadowed by them before, too).
fn shell_only(path: &str) -> bool {
    matches!(
        path,
        "/upload"
            | "/shell.css"
            | "/icons.js"
            | "/icon-256.png"
            | "/template.uapp"
            | "/download.uapp"
            | "/site.uapp"
    ) || path.starts_with("/shell/")
}

/// Keep the two origins' routes apart, then add the security headers.
async fn guard_and_headers(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let app_side = on_app_origin(&app, req.headers());
    let isolated = app.app_origin().is_some();
    let path = req.uri().path().to_string();
    if (app_side && shell_only(&path)) || (isolated && !app_side && path == "/scratch/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    // An app page navigating to another of its pages without `?a=` (a plain
    // link, a form): send it back with the token, so that page's requests
    // carry it too. The Referer says which page it came from.
    if app_side
        && req.method() == axum::http::Method::GET
        && !query_has_app_token(&app, req.uri().query())
        && req
            .headers()
            .get("sec-fetch-mode")
            .and_then(|v| v.to_str().ok())
            == Some("navigate")
        && referer_has_app_token(&app, req.headers())
    {
        let to = match req.uri().query().filter(|q| !q.is_empty()) {
            Some(q) => format!("{path}?{q}&a={}", app.app_token),
            None => format!("{path}?a={}", app.app_token),
        };
        let resp = (
            StatusCode::FOUND,
            [
                (header::LOCATION, to),
                (header::CACHE_CONTROL, "no-store".into()),
            ],
        )
            .into_response();
        return security_headers(&app, resp);
    }
    // The shell's first load of the frame carries ?a=; hand the cookie over
    // for clients that keep it (see `app_cookie_name`).
    let set_app_cookie = app_side
        && req.uri().query().unwrap_or("").split('&').any(|kv| {
            kv.strip_prefix("a=")
                .is_some_and(|t| crate::secrets::token_eq(t, &app.app_token))
        });
    let mut resp = next.run(req).await;
    if app_side {
        // Only our shell (and the app's own pages, for frames of its own)
        // may frame the app. Same-origin referrers carry the token from the
        // page's URL to its requests (see `app_access`), and nowhere else.
        if !resp.headers().contains_key(header::CONTENT_SECURITY_POLICY)
            && let Ok(v) = header::HeaderValue::from_str(&format!(
                "frame-ancestors 'self' {}",
                app.shell_origin()
            ))
        {
            resp.headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, v);
        }
        resp.headers_mut().insert(
            header::REFERRER_POLICY,
            header::HeaderValue::from_static("same-origin"),
        );
        if set_app_cookie
            && let Ok(v) = header::HeaderValue::from_str(&format!(
                "{}={}; Path=/; SameSite=Strict; HttpOnly",
                app_cookie_name(&app),
                app.app_token
            ))
        {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    } else if isolated && path.starts_with("/app/") {
        // The archive as the shell reads it: an HTML or SVG file opened from
        // here runs sandboxed, never as the shell.
        let active = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html") || v.starts_with("image/svg+xml"));
        if active {
            resp.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                header::HeaderValue::from_static("frame-ancestors 'self'; sandbox"),
            );
        }
    }
    security_headers(&app, resp)
}

/// Baseline security headers on every response. App pages get no script
/// policy: they run their own JS/styles by design, so a restrictive one would
/// break them. Their isolation is the origin they're served from (see
/// docs/design/app-isolation.md). They do get frame-ancestors.
///
/// A public site that opted in (`--coi`) gets COOP/COEP on its documents so
/// the editing chrome is cross-origin isolated from birth (SharedArrayBuffer
/// for the wasm bridge) — credentialless rather than require-corp, so plain
/// cross-origin `<script>`/`<img>` in the pages keep loading.
fn security_headers(app: &App, mut resp: Response) -> Response {
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/html"))
        .unwrap_or(false);
    let h = resp.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    // Nothing here may be framed by another origin: the shell (clickjacking
    // its Allow buttons) least of all. The shell sets a full policy of its own.
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static("frame-ancestors 'self'"),
        );
    }
    match app.public {
        None => {
            if !h.contains_key(header::REFERRER_POLICY) {
                h.insert(
                    header::REFERRER_POLICY,
                    header::HeaderValue::from_static("no-referrer"),
                );
            }
        }
        Some(opts) => {
            h.insert(
                header::REFERRER_POLICY,
                header::HeaderValue::from_static("strict-origin-when-cross-origin"),
            );
            if opts.coi && is_html {
                h.insert(
                    "cross-origin-opener-policy",
                    header::HeaderValue::from_static("same-origin"),
                );
                h.insert(
                    "cross-origin-embedder-policy",
                    header::HeaderValue::from_static("credentialless"),
                );
            }
        }
    }
    resp
}

// ---- public site mode -------------------------------------------------------

/// Load the browser build (`dist-web`) a public site serves as its editing
/// chrome. Without it a site still serves perfectly well — it just has no
/// editor.
pub fn load_chrome(dir: &std::path::Path) -> anyhow::Result<crate::app::Chrome> {
    load_bundle(dir, false)
}

/// Read the browser build into memory. `standalone` keeps the pieces that make
/// the bundle a page of its own — `index.html` (the shell's entry document)
/// and `launcher.uapp` — for the archive-less host mode
/// ([`crate::webhost`]); a site's editing chrome drops them, since the site's
/// own index.html must win at `/`. Build leftovers never ship either way.
pub fn load_bundle(dir: &std::path::Path, standalone: bool) -> anyhow::Result<crate::app::Chrome> {
    use anyhow::Context as _;
    use std::hash::{Hash, Hasher};
    let skip = |rel: &str| -> bool {
        (!standalone && (rel == "index.html" || rel == "launcher.uapp"))
            || rel.starts_with("examples/")
            || rel.ends_with(".d.ts")
            || rel.ends_with(".map")
    };
    let mut bundle = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in
            std::fs::read_dir(&d).with_context(|| format!("reading web bundle {}", d.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if skip(&rel) {
                continue;
            }
            bundle.insert(format!("/{rel}"), std::fs::read(&path)?);
        }
    }
    anyhow::ensure!(
        bundle.contains_key("/boot.js") && bundle.contains_key("/sw.js"),
        "{} does not look like a uapp web build (no boot.js / sw.js) — run scripts/build-web.sh",
        dir.display()
    );
    // site-chrome.js is the injected entry point; serve it under the shorter
    // name the injected tag uses.
    let Some(chrome) = bundle.get("/site-chrome.js").cloned() else {
        anyhow::bail!(
            "{} has no site-chrome.js — run scripts/build-web.sh",
            dir.display()
        );
    };
    bundle.insert("/chrome.js".into(), chrome);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut names: Vec<&String> = bundle.keys().collect();
    names.sort();
    for n in names {
        n.hash(&mut h);
        bundle[n].hash(&mut h);
    }
    Ok(crate::app::Chrome {
        bundle,
        version: format!("{:x}", h.finish()),
    })
}

/// A strong-form validator over the served bytes. Only a cache validator, so
/// a non-cryptographic hash is fine.
pub(crate) fn etag_for(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:x}\"", h.finish())
}

pub(crate) fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
}

/// Read one archive entry by its EXACT name, still compressed: `(sz, data)`
/// as stored, inflated by the caller outside the engine lock.
/// `store::sqlar_read` is the wrong tool for an anonymous request — see the
/// module header.
fn read_exact_raw(db: &rusqlite::Connection, name: &str) -> Option<(Option<i64>, Vec<u8>)> {
    let mut stmt = db
        .prepare("SELECT sz, data FROM sqlar WHERE name=?1")
        .ok()?;
    let mut rows = stmt.query(rusqlite::params![name]).ok()?;
    let row = rows.next().ok()??;
    let sz: Option<i64> = row.get(0).ok()?;
    let data: Option<Vec<u8>> = row.get(1).ok()?; // NULL = directory entry
    Some((sz, data?))
}

/// Map a request path to an `app/` name the way a static host would: `/` and
/// `/posts/` get `index.html`, and an extensionless path tries `<p>.html`
/// then `<p>/index.html`, so posts can be linked as `/posts/hello` as well as
/// `/posts/hello.html`.
fn resolve_public(db: &rusqlite::Connection, path: &str) -> Option<(String, Option<i64>, Vec<u8>)> {
    let p = path.trim_start_matches('/');
    if p.contains("..") || p.contains('\\') {
        return None;
    }
    let mut tries: Vec<String> = Vec::new();
    if p.is_empty() || p.ends_with('/') {
        tries.push(format!("app/{p}index.html"));
    } else {
        tries.push(format!("app/{p}"));
        if !p.rsplit('/').next().unwrap_or("").contains('.') {
            tries.push(format!("app/{p}.html"));
            tries.push(format!("app/{p}/index.html"));
        }
    }
    tries
        .into_iter()
        .find_map(|name| read_exact_raw(db, &name).map(|(sz, b)| (name, sz, b)))
}

/// Add `<meta name="viewport">` when the document lacks one, and — when the
/// editing chrome is available — the one script tag that offers it.
///
/// Deliberately NOT `inject_viewport`: that adds `/uapp.js`, whose console
/// forwarding wants the privileged WebSocket. Nothing else is added, so a
/// cold load stays the plain document a crawler wants.
fn inject_chrome(bytes: Vec<u8>, chrome: Option<&str>) -> Vec<u8> {
    const META: &str = r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => return e.into_bytes(), // non-utf8: serve untouched
    };
    // A marker the browser build used to inject into frame HTML; a site that
    // renders pages from fetched HTML persisted it, and at top level it told
    // the chrome it was inside the wasm frame. Harmless now, still removed.
    let text = strip_wasm_marker(&text);
    let lower = text.to_ascii_lowercase();
    let mut inject = String::new();
    if !(lower.contains("name=\"viewport\"") || lower.contains("name='viewport'")) {
        inject.push('\n');
        inject.push_str(META);
    }
    if let Some(v) = chrome
        && !loads_chrome_entry(&lower)
    {
        inject.push('\n');
        inject.push_str(&format!(
            r#"<script src="/chrome.js?v={v}" defer></script>"#
        ));
    }
    if inject.is_empty() {
        return text.into_bytes();
    }
    if let Some(pos) = lower.find("<head")
        && let Some(gt) = text[pos..].find('>')
    {
        let at = pos + gt + 1;
        return format!("{}{}{}", &text[..at], inject, &text[at..]).into_bytes();
    }
    format!("{inject}\n{text}").into_bytes()
}

/// Does the document already load the injected `/chrome.js` entry point?
///
/// Has to be an exact path match, not a substring: a site may ship a file of
/// its own whose name merely *ends* in `chrome.js` (thederf.com's pages load
/// `/js/chrome.js`, its window chrome), and a substring test treats that as
/// the tag already being present — so nothing is injected and the editor
/// never starts on any page of the site.
fn loads_chrome_entry(lower: &str) -> bool {
    const NAME: &str = "/chrome.js";
    let b = lower.as_bytes();
    lower.match_indices(NAME).any(|(i, _)| {
        // The path must start right after an attribute's quote (or `=`, for an
        // unquoted src) and end at the quote, a `?query` or whitespace.
        let opens = i > 0 && matches!(b[i - 1], b'"' | b'\'' | b'=' | b' ');
        let ends = matches!(
            b.get(i + NAME.len()),
            None | Some(b'"' | b'\'' | b'?' | b' ' | b'>')
        );
        opens && ends
    })
}

fn strip_wasm_marker(text: &str) -> String {
    const MARKERS: [&str; 2] = [
        "<script>window.__uappWasm=1</script>",
        "<script>window.__uappWasm = 1;</script>",
    ];
    let mut out = text.to_string();
    for m in MARKERS {
        while let Some(pos) = out.find(m) {
            let start = out[..pos].trim_end_matches(['\n', ' ', '\t']).len();
            out.replace_range(start..pos + m.len(), "");
        }
    }
    out
}

/// An anonymous visitor's view of the site: one `app/` page (or the site's
/// `404.html`), with cache validators so a CDN or browser can revalidate, and
/// byte ranges so media can seek.
fn public_page(app: &App, path: &str, headers: &HeaderMap) -> Response {
    let opts = app.public.unwrap_or_default();
    let Some(page) = public_lookup(app, path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let is_404 = page.name == "app/404.html" && path.trim_start_matches('/') != "404.html";
    let ctype = content_type_for(&page.name);
    let is_html = ctype.starts_with("text/html");
    let cache = if is_html {
        // Always revalidate: a reload must get the server's authoritative copy.
        "public, max-age=0, must-revalidate".to_string()
    } else {
        // Site files are served under their archive names (nothing is
        // content-hashed), so after a publish a cached asset would run
        // against new HTML for its whole max-age. Default: always
        // revalidate — the ETag makes that a cheap 304. Operators whose
        // build fingerprints assets can raise it with --asset-max-age.
        if opts.asset_max_age == 0 {
            "public, max-age=0, must-revalidate".to_string()
        } else {
            format!("public, max-age={}", opts.asset_max_age)
        }
    };
    let etag = page.etag.clone();
    if !is_404 && if_none_match(headers, &etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache)],
        )
            .into_response();
    }
    let total = page.body.len() as u64;
    // The cached page itself becomes the body: no copy per request.
    let body = axum::body::Bytes::from_owner(SharedBody(page));
    let range = if is_404 {
        None
    } else {
        parse_range(headers, total)
    };
    match range {
        Some(Ok((start, end))) => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, ctype.to_string()),
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                ),
            ],
            body.slice(start as usize..=end as usize),
        )
            .into_response(),
        Some(Err(())) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            "range not satisfiable",
        )
            .into_response(),
        None => (
            if is_404 {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            },
            [
                (header::CONTENT_TYPE, ctype.to_string()),
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            body,
        )
            .into_response(),
    }
}

/// A shared, immutable response body (cached page or archive) handed to the
/// HTTP layer without copying it.
struct SharedBody<T>(Arc<T>);

impl<T: AsRef<[u8]>> AsRef<[u8]> for SharedBody<T> {
    fn as_ref(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

/// Pages bigger than this are served but not kept (a video can be huge), and
/// the whole cache is dropped when it would grow past the total.
const PAGE_CACHE_ENTRY_MAX: usize = 8 * 1024 * 1024;
const PAGE_CACHE_TOTAL_MAX: usize = 64 * 1024 * 1024;
/// The site's 404 page is cached once under this key, not once per missing
/// path (which anyone could use to fill the cache).
const NOT_FOUND_KEY: &str = ":404"; // request paths all start with '/'

/// The page for `path`, from the cache when nothing was written since it was
/// built. On a miss the engine lock is held only to read the stored
/// (compressed) rows; inflating and injecting happen after it is released.
fn public_lookup(app: &App, path: &str) -> Option<Arc<crate::app::CachedPage>> {
    let writes = app.eng().writes;
    {
        let c = crate::app::lock(&app.page_cache);
        if c.writes == writes
            && let Some(p) = c.pages.get(path)
        {
            return Some(p.clone());
        }
    }
    let (found, not_found, writes) = {
        let eng = app.eng();
        let found = resolve_public(&eng.db, path);
        let cached_404 = {
            let c = crate::app::lock(&app.page_cache);
            (c.writes == eng.writes)
                .then(|| c.pages.get(NOT_FOUND_KEY).cloned())
                .flatten()
        };
        let not_found = match (&found, cached_404) {
            (Some(_), _) => None,
            (None, Some(p)) => return Some(p),
            (None, None) => read_exact_raw(&eng.db, "app/404.html"),
        };
        (found, not_found, eng.writes)
    };
    let (key, name, sz, data) = match (found, not_found) {
        (Some((name, sz, data)), _) => (path.to_string(), name, sz, data),
        (None, Some((sz, data))) => (NOT_FOUND_KEY.to_string(), "app/404.html".into(), sz, data),
        (None, None) => return None,
    };
    let bytes = crate::store::sqlar_inflate(sz, data, &name).ok()?;
    let body = if content_type_for(&name).starts_with("text/html") {
        inject_chrome(bytes, app.chrome.as_ref().map(|c| c.version.as_str()))
    } else {
        bytes
    };
    let page = Arc::new(crate::app::CachedPage {
        etag: etag_for(&body),
        name,
        body,
    });
    let len = page.body.len();
    if len <= PAGE_CACHE_ENTRY_MAX {
        let mut c = crate::app::lock(&app.page_cache);
        if c.writes != writes || c.bytes + len > PAGE_CACHE_TOTAL_MAX {
            *c = crate::app::PageCache {
                writes,
                ..Default::default()
            };
        }
        if let Some(old) = c.pages.insert(key, page.clone()) {
            c.bytes -= old.body.len();
        }
        c.bytes += len;
    }
    Some(page)
}

/// The current `/site.uapp`, rebuilt only when something was written since.
fn current_archive(app: &App) -> anyhow::Result<Arc<crate::app::PublicArchive>> {
    let opts = app.public.unwrap_or_default();
    let eng = app.eng();
    let mut cache = crate::app::lock(&app.archive_cache);
    if let Some((stamp, arc)) = cache.as_ref()
        && *stamp == eng.writes
    {
        return Ok(arc.clone());
    }
    let name = crate::store::meta_get(&eng.db, "name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "site".into());
    let bytes = crate::store::export_public(&eng.db, &name, opts.export_data)?;
    let arc = Arc::new(crate::app::PublicArchive {
        etag: etag_for(&bytes),
        bytes,
    });
    *cache = Some((eng.writes, arc.clone()));
    Ok(arc)
}

/// `GET /site.uapp`: the copy the in-browser engine boots from. Built by
/// `store::export_public`, which keeps app files and user-table ROWS (the
/// site's own SQL machinery) but drops chat, file history and config secrets.
async fn site_archive(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !app.public.map(|o| o.archive).unwrap_or(false) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let arc = match tokio::task::spawn_blocking({
        let app = app.clone();
        move || current_archive(&app)
    })
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    if if_none_match(&headers, &arc.etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, arc.etag.clone())]).into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::ETAG, arc.etag.clone()),
            // Revalidate rather than cache blind: the client re-fetches on
            // every full load and must see the current copy; a 304 keeps
            // that cheap.
            (
                header::CACHE_CONTROL,
                "public, max-age=0, must-revalidate".to_string(),
            ),
        ],
        axum::body::Bytes::from_owner(SharedBody(arc)),
    )
        .into_response()
}

/// `PUT /site.uapp` (token required): publish an edited copy. The body is a
/// whole .uapp as the browser engine exports it; `template::publish` makes
/// the served site match it while keeping what the copy never had (config,
/// chat, history). `If-Match` with the ETag of the copy's download refuses
/// (409) to overwrite a site that moved on in the meantime — the browser
/// then shows "server copy is newer" instead of silently losing that.
async fn site_publish(
    State(app): State<Arc<App>>,
    _who: Owner,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(opts) = app.public else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if body.len() > crate::rpc::MAX_UPLOAD_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "archive too large").into_response();
    }
    if !body.starts_with(b"SQLite format 3\0") {
        return (StatusCode::BAD_REQUEST, "not a .uapp archive").into_response();
    }
    let if_match = headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let done = tokio::task::spawn_blocking(move || -> Result<Value, Response> {
        let err = |st: StatusCode, e: String| (st, e).into_response();
        if let Some(want) = if_match {
            let cur = current_archive(&app).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            if want.split(',').all(|t| t.trim() != cur.etag && t.trim() != "*") {
                return Err((
                    StatusCode::CONFLICT,
                    [(header::ETAG, cur.etag.clone())],
                    axum::Json(json!({"error": "the site changed since this copy was downloaded", "etag": cur.etag})),
                )
                    .into_response());
            }
        }
        // Rescue point before a whole-site change, like a template update.
        if let Err(e) = app.eng().snapshot() {
            tracing::warn!("snapshot before publish failed: {e:#}");
        }
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &body);
        let result = crate::rpc::local_op(&app, "publish", json!({"b64": b64, "data": opts.export_data}))
            .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("publish failed: {e:#}")))?;
        let arc = current_archive(&app).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        Ok(json!({"ok": true, "result": result, "etag": arc.etag}))
    })
    .await;
    match done {
        Ok(Ok(v)) => {
            let etag = v["etag"].as_str().unwrap_or("").to_string();
            ([(header::ETAG, etag)], axum::Json(v)).into_response()
        }
        Ok(Err(resp)) => resp,
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("publish failed: {e}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injected(html: &str) -> String {
        String::from_utf8(inject_chrome(html.as_bytes().to_vec(), Some("abc123"))).unwrap()
    }

    #[test]
    fn a_site_file_named_chrome_js_does_not_suppress_the_entry_point() {
        // thederf.com's generated pages load their own `/js/chrome.js` (the
        // window chrome). A substring test on "/chrome.js" matched it and the
        // tag stopped being injected site-wide: the editor never mounted.
        let page = r#"<html><head><title>t</title></head><body>
          <script>var srcs = ["/uapp.js", "/js/chrome.js?v=1", "/js/build.js?v=1"];</script>
        </body></html>"#;
        let out = injected(page);
        assert!(
            out.contains(r#"<script src="/chrome.js?v=abc123" defer></script>"#),
            "{out}"
        );
        // ...and the site's own reference is left alone.
        assert!(out.contains(r#""/js/chrome.js?v=1""#), "{out}");
    }

    #[test]
    fn the_entry_point_is_never_injected_twice() {
        for tag in [
            r#"<script src="/chrome.js?v=old" defer></script>"#,
            r#"<script src="/chrome.js" defer></script>"#,
            r#"<script src='/chrome.js?v=old'></script>"#,
            r#"<script src=/chrome.js defer></script>"#,
        ] {
            let out = injected(&format!("<html><head>{tag}</head><body></body></html>"));
            assert_eq!(
                out.matches("/chrome.js").count(),
                1,
                "duplicated for {tag}: {out}"
            );
        }
    }

    #[test]
    fn injection_lands_inside_head_and_adds_the_viewport_once() {
        let out = injected("<html><head><title>t</title></head><body>x</body></html>");
        let head = out.find("</head>").unwrap();
        assert!(out.find("/chrome.js").unwrap() < head, "{out}");
        assert_eq!(out.matches("name=\"viewport\"").count(), 1, "{out}");
        // A page with its own viewport keeps just the one.
        let own = injected(
            r#"<html><head><meta name="viewport" content="width=device-width"></head><body></body></html>"#,
        );
        assert_eq!(own.matches("name=\"viewport\"").count(), 1, "{own}");
    }
}
