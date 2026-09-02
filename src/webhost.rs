//! Archive-less host mode: `uapp serve --chrome <dist-web>` with NO .uapp.
//!
//! Serves the browser build alone — the shell's own `index.html` at `/`, its
//! bundle, and nothing else. There is no engine, no WebSocket, no
//! `/site.uapp` and no token: what the page opens is decided in the browser
//! (boot.js, `OPEN`). `?open=<url>` fetches an archive from anywhere on the
//! origin (or a CORS-enabled one), `?app=<id>` opens a document of the
//! visitor's own library, and a bare visit gets the page's default — the
//! `--open <url>` given here, injected into index.html as
//! `<meta name="uapp-open">`. Publishing goes back to wherever the archive came
//! from (a `uapp serve` site's `PUT /site.uapp`, a `uapp-library` PUT), so
//! this process holds no state and no secret at all.
//!
//! The same binary's full router ([`crate::server`]) serves ONE archive as a
//! website; this one is what a Caddy `file_server` on the build directory
//! would be, plus the COI headers the shell needs from its very first load
//! and the default-open meta.

use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

use crate::app::Chrome;

pub struct WebHost {
    /// The browser build, index.html and launcher.uapp included
    /// (`server::load_bundle(dir, true)`).
    pub chrome: Chrome,
    /// COOP/COEP on documents, so the shell is cross-origin isolated from its
    /// first load (SharedArrayBuffer for the wasm bridge). The service worker
    /// stamps the same headers once it runs; these cover the load before it.
    pub coi: bool,
    /// What a bare visit opens — a URL, resolved by the browser against the
    /// page (so a path works: `/uapp/apps/launcher.uapp`).
    pub open: Option<String>,
}

pub fn router(host: Arc<WebHost>) -> Router {
    Router::new()
        .route("/health", get(health))
        .fallback(serve)
        .with_state(host)
}

async fn health() -> Response {
    ([("x-uapp-version", env!("CARGO_PKG_VERSION"))], "ok").into_response()
}

async fn serve(State(host): State<Arc<WebHost>>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "GET only").into_response();
    }
    let mut path = crate::server::percent_decode(uri.path());
    if path == "/" || path.is_empty() {
        path = "/index.html".into();
    }
    let Some(bytes) = host.chrome.bundle.get(path.as_str()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let ctype = crate::server::content_type_for(&path);
    let is_html = ctype.starts_with("text/html");
    let body = if is_html && path == "/index.html" {
        inject_default_open(bytes.clone(), host.open.as_deref())
    } else {
        bytes.clone()
    };
    let etag = crate::server::etag_for(&body);
    if crate::server::if_none_match(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let mut resp = (
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (header::ETAG, etag),
            // Changes only with a deploy, but must never outlive one:
            // revalidate, and let the ETag make that cheap.
            (header::CACHE_CONTROL, "public, max-age=0, must-revalidate".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        body,
    )
        .into_response();
    if host.coi && is_html {
        let h = resp.headers_mut();
        h.insert("cross-origin-opener-policy", header::HeaderValue::from_static("same-origin"));
        h.insert("cross-origin-embedder-policy", header::HeaderValue::from_static("credentialless"));
    }
    resp
}

/// Put `<meta name="uapp-open" content="<url>">` at the top of `<head>` — the
/// default archive for a bare visit (boot.js reads it when the URL names
/// nothing). A page that already carries one keeps its own.
fn inject_default_open(bytes: Vec<u8>, open: Option<&str>) -> Vec<u8> {
    let Some(open) = open else { return bytes };
    let Ok(html) = String::from_utf8(bytes) else { return Vec::new() };
    if html.contains("name=\"uapp-open\"") || html.contains("name='uapp-open'") {
        return html.into_bytes();
    }
    let tag = format!(r#"<meta name="uapp-open" content="{}">"#, crate::server::html_escape(open));
    let lower = html.to_lowercase();
    if let Some(pos) = lower.find("<head") {
        if let Some(gt) = html[pos..].find('>') {
            let at = pos + gt + 1;
            return format!("{}\n{}{}", &html[..at], tag, &html[at..]).into_bytes();
        }
    }
    format!("{tag}\n{html}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_open_lands_in_head_escaped() {
        let html = b"<!doctype html><html><head><title>x</title></head><body></body></html>".to_vec();
        let out = String::from_utf8(inject_default_open(html, Some("/apps/a.uapp?x=\"1\""))).unwrap();
        assert!(out.contains(r#"<head>
<meta name="uapp-open" content="/apps/a.uapp?x=&quot;1&quot;"><title>"#), "{out}");
    }

    #[test]
    fn no_default_means_no_tag_and_an_existing_tag_wins() {
        let html = b"<head></head>".to_vec();
        assert_eq!(inject_default_open(html.clone(), None), html);
        let own = br#"<head><meta name="uapp-open" content="/mine.uapp"></head>"#.to_vec();
        assert_eq!(inject_default_open(own.clone(), Some("/other.uapp")), own);
    }
}
