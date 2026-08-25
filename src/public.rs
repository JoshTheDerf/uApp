//! Public read-only HTTP server for one .uapp — the "hosted site" mode.
//!
//! This is a DELIBERATELY separate router from `server.rs`. That module's
//! routes (`/ws`, `/upload`, `/download.uapp`, `/template.uapp`, `/shell/*`,
//! `/scratch/`) are all privileged: the WebSocket alone carries `sql.exec`.
//! Rather than thread a `public: bool` through every handler and hope none is
//! ever forgotten, nothing privileged is registered here at all. Omission is
//! the security model.
//!
//! Two other rules matter as much as the route list:
//!   * The database is opened SQLITE_OPEN_READ_ONLY, so a write is impossible
//!     even by way of a bug.
//!   * File lookups resolve STRICTLY under `app/`. `store::sqlar_read` falls
//!     back to `data/<name>` (see `store::name_candidates`), which on a public
//!     origin would publish every user-data file in the archive.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rusqlite::Connection;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

/// Knobs a host may want per site. Defaults are the conservative ones.
#[derive(Clone, Copy)]
pub struct PublicOpts {
    /// Send COOP/COEP so the page is cross-origin isolated from birth (no
    /// service-worker stamp, no boot reload). Costs cross-origin `<iframe>`
    /// embeds — YouTube, CodePen and friends stop rendering unless they opt
    /// in — so it is off unless the site actually wants the SharedArrayBuffer
    /// bridge in the editing chrome.
    pub coi: bool,
    /// Expose `/site.uapp` (the sanitized archive the in-browser engine boots
    /// from). Off means "plain static site, no editing chrome".
    pub archive: bool,
    /// Include `data/` in that archive — the content sources a site's build
    /// pipeline reads, so visitors can re-run the build in their browser.
    /// Anything included is PUBLIC: the archive is downloaded by every visitor.
    /// Serving `data/` over HTTP stays blocked either way.
    pub export_data: bool,
    /// max-age for non-HTML assets. HTML is always revalidated.
    pub asset_max_age: u32,
}

impl Default for PublicOpts {
    fn default() -> Self {
        Self { coi: false, archive: true, export_data: false, asset_max_age: 3600 }
    }
}

pub struct PublicSite {
    db: Mutex<Connection>,
    opts: PublicOpts,
    /// The browser-build assets (wasm engine + shell) that the editing chrome
    /// needs, keyed by the root path they are served at. Empty = no chrome.
    ///
    /// These are served at the ROOT rather than under a prefix for two
    /// reasons: a service worker's scope is its own directory, and `/sw.js`
    /// is the only place it can be registered from to control `/app/*`; and
    /// boot.js resolves everything it loads relative to its own URL. A site
    /// file with one of these names is shadowed — the same trade the native
    /// server makes for its shell routes.
    bundle: std::collections::HashMap<String, Vec<u8>>,
    /// Short content hash of the bundle, appended as `?v=` to the injected
    /// script tag. Cloudflare (and other CDNs) rewrite Cache-Control on .js
    /// to hours of browser caching, so without this a deploy leaves visitors
    /// on the previous chrome until it expires.
    bundle_version: String,
    /// The archive is built from a read-only database that cannot change while
    /// we run, so it is built at most once.
    /// (bytes, etag, last content modification as unix seconds — max sqlar mtime)
    archive: Mutex<Option<Arc<(Vec<u8>, String, i64)>>>,
}

/// Open a .uapp for public serving: read-only, no registry lock, no migration.
///
/// Deliberately NOT `Engine::open` — that takes an exclusive lock for the
/// process lifetime (so you could not edit the file locally while it is
/// served) and runs migrations, which are writes.
pub fn open_readonly(path: &Path, passphrase: Option<&str>) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match passphrase {
        Some(pw) => crate::cipher::open_file_readonly(path, pw)?,
        None => {
            anyhow::ensure!(
                !crate::store::is_encrypted_file(path),
                "{} is encrypted — pass --password (or set UAPP_PASSWORD) to serve it",
                path.display()
            );
            Connection::open_with_flags(path, flags)?
        }
    };
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

pub fn site(db: Connection, opts: PublicOpts) -> Arc<PublicSite> {
    Arc::new(PublicSite {
        db: Mutex::new(db),
        opts,
        bundle: std::collections::HashMap::new(),
        bundle_version: String::new(),
        archive: Mutex::new(None),
    })
}

/// Files the browser build ships that a hosted site must NOT serve: the demo's
/// own launcher and landing page (index.html would shadow the site's), plus
/// build leftovers.
fn bundle_skip(rel: &str) -> bool {
    rel == "index.html"
        || rel == "launcher.uapp"
        || rel.starts_with("examples/")
        || rel.ends_with(".d.ts")
        || rel.ends_with(".map")
}

/// Load the browser build (`dist-web`) so the editing chrome can boot. Without
/// it a site still serves perfectly well — it just has no editor.
pub fn site_with_chrome(db: Connection, opts: PublicOpts, dir: &Path) -> Result<Arc<PublicSite>> {
    let mut bundle = std::collections::HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .with_context(|| format!("reading web bundle {}", d.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if bundle_skip(&rel) {
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
    if let Some(v) = bundle.get("/site-chrome.js").cloned() {
        bundle.insert("/chrome.js".into(), v);
    }
    anyhow::ensure!(
        bundle.contains_key("/chrome.js"),
        "{} has no site-chrome.js — run scripts/build-web.sh",
        dir.display()
    );
    let mut h = DefaultHasher::new();
    let mut names: Vec<&String> = bundle.keys().collect();
    names.sort();
    for n in names {
        n.hash(&mut h);
        bundle[n].hash(&mut h);
    }
    let bundle_version = format!("{:x}", h.finish());
    Ok(Arc::new(PublicSite { db: Mutex::new(db), opts, bundle, bundle_version, archive: Mutex::new(None) }))
}

pub fn router(state: Arc<PublicSite>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/site.uapp", get(site_archive))
        .fallback(get(public_file))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// ---- file serving -----------------------------------------------------------

/// Read one archive entry by its EXACT name. `store::sqlar_read` is the wrong
/// tool on a public origin — see the module header.
fn read_exact(db: &Connection, name: &str) -> Option<Vec<u8>> {
    let mut stmt = db.prepare("SELECT sz, data FROM sqlar WHERE name=?1").ok()?;
    let mut rows = stmt.query(rusqlite::params![name]).ok()?;
    let row = rows.next().ok()??;
    let sz: Option<i64> = row.get(0).ok()?;
    // NULL data = directory entry, not a readable file.
    let data: Option<Vec<u8>> = row.get(1).ok()?;
    crate::store::sqlar_inflate(sz, data?, name).ok()
}

/// Map a request path to an archive name, applying the usual static-host
/// conveniences: `/` and `/posts/` get `index.html`, and an extensionless
/// path tries `<p>.html` then `<p>/index.html` so posts can be linked as
/// `/posts/hello` as well as `/posts/hello.html`.
fn resolve(db: &Connection, path: &str) -> Option<(String, Vec<u8>)> {
    let p = path.trim_start_matches('/');
    // `..` can't escape `app/` given we always prefix, but a stored name
    // containing it would be confusing at best. Backslash likewise.
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
    for name in tries {
        if let Some(bytes) = read_exact(db, &name) {
            return Some((name, bytes));
        }
    }
    None
}

/// A weak-ish but strong-form validator over the served bytes. Only a cache
/// validator, so a non-cryptographic hash is fine.
fn etag_for(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("\"{:x}\"", h.finish())
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim() == etag))
        .unwrap_or(false)
}

/// Add `<meta name="viewport">` when the document lacks one, and — when the
/// editing chrome is available — the one script tag that offers it.
///
/// Deliberately NOT the privileged server's `inject_viewport`: that injects
/// `/uapp.js`, which here would 404 and would point a page's console at a
/// server that is not listening. Nothing else is added, so a cold load stays
/// the plain document a crawler wants.
fn inject_chrome(bytes: Vec<u8>, chrome: Option<&str>) -> Vec<u8> {
    const META: &str = r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => return e.into_bytes(), // non-utf8: serve untouched
    };
    let lower = text.to_ascii_lowercase();
    let mut inject = String::new();
    if !(lower.contains("name=\"viewport\"") || lower.contains("name='viewport'")) {
        inject.push('\n');
        inject.push_str(META);
    }
    if let Some(v) = chrome {
        if !lower.contains("/chrome.js") {
            inject.push('\n');
            inject.push_str(&format!(r#"<script src="/chrome.js?v={v}" defer></script>"#));
        }
    }
    if inject.is_empty() {
        return text.into_bytes();
    }
    if let Some(pos) = lower.find("<head") {
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

async fn public_file(
    State(site): State<Arc<PublicSite>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = percent_decode(uri.path());
    // Browser-build assets win over archive files of the same name — the
    // editing chrome cannot boot if the site shadows its loader.
    if let Some(bytes) = site.bundle.get(path.as_str()) {
        let ctype = crate::server::content_type_for(&path);
        let etag = etag_for(bytes);
        if if_none_match(&headers, &etag) {
            let resp = (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
            return with_headers(&site, resp, false);
        }
        let resp = (
            [
                (header::CONTENT_TYPE, ctype.to_string()),
                (header::ETAG, etag),
                // The bundle changes only when the server image does, but it
                // must never outlive a deploy: revalidate, and let the ETag
                // make that cheap.
                (header::CACHE_CONTROL, "public, max-age=0, must-revalidate".to_string()),
            ],
            bytes.clone(),
        )
            .into_response();
        return with_headers(&site, resp, false);
    }
    let found = {
        let db = site.db.lock().unwrap();
        resolve(&db, &path).or_else(|| {
            read_exact(&db, "app/404.html").map(|b| ("app/404.html".to_string(), b))
        })
    };
    let Some((name, bytes)) = found else {
        return with_headers(&site, (StatusCode::NOT_FOUND, "not found").into_response(), false);
    };
    let is_404 = path.trim_start_matches('/') != name.trim_start_matches("app/")
        && name == "app/404.html";
    let ctype = crate::server::content_type_for(&name);
    let bytes = if ctype.starts_with("text/html") {
        inject_chrome(bytes, (!site.bundle.is_empty()).then_some(site.bundle_version.as_str()))
    } else {
        bytes
    };

    let etag = etag_for(&bytes);
    let is_html = ctype.starts_with("text/html");
    if !is_404 && if_none_match(&headers, &etag) {
        let resp = (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache_control(&site, is_html))],
        )
            .into_response();
        return with_headers(&site, resp, is_html);
    }
    let status = if is_404 { StatusCode::NOT_FOUND } else { StatusCode::OK };
    let resp = (
        status,
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache_control(&site, is_html)),
        ],
        bytes,
    )
        .into_response();
    with_headers(&site, resp, is_html)
}

fn cache_control(site: &PublicSite, is_html: bool) -> String {
    if is_html {
        // Always revalidate: a reload must get the server's authoritative copy.
        "public, max-age=0, must-revalidate".into()
    } else {
        format!("public, max-age={}", site.opts.asset_max_age)
    }
}

/// Baseline headers, plus cross-origin isolation when the site opted in.
fn with_headers(site: &PublicSite, mut resp: Response, is_document: bool) -> Response {
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    if site.opts.coi && is_document {
        h.insert("cross-origin-opener-policy", header::HeaderValue::from_static("same-origin"));
        // credentialless (not require-corp) so plain cross-origin <script>/<img>
        // in the site's pages keep loading.
        h.insert("cross-origin-embedder-policy", header::HeaderValue::from_static("credentialless"));
    }
    resp
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

// ---- /site.uapp -------------------------------------------------------------

/// The copy the in-browser engine boots from. Built by `store::export_public`,
/// which keeps app files and user-table ROWS (the site's own SQL machinery)
/// but drops chat, file history and config secrets.
/// Unix-seconds stamp of the archive's newest file; a HEAD is enough to read it.
const MODIFIED_HEADER: header::HeaderName = header::HeaderName::from_static("x-uapp-modified");

async fn site_archive(State(site): State<Arc<PublicSite>>, headers: HeaderMap) -> Response {
    if !site.opts.archive {
        return with_headers(&site, (StatusCode::NOT_FOUND, "not found").into_response(), false);
    }
    let cached = {
        let mut slot = site.archive.lock().unwrap();
        if slot.is_none() {
            let db = site.db.lock().unwrap();
            let name = crate::store::meta_get(&db, "name")
                .ok()
                .flatten()
                .unwrap_or_else(|| "site".into());
            match crate::store::export_public(&db, &name, site.opts.export_data) {
                Ok(bytes) => {
                    let etag = etag_for(&bytes);
                    let modified: i64 = db
                        .query_row("SELECT coalesce(max(mtime), 0) FROM sqlar", [], |r| r.get(0))
                        .unwrap_or(0);
                    *slot = Some(Arc::new((bytes, etag, modified)));
                }
                Err(e) => {
                    return with_headers(
                        &site,
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
                        false,
                    )
                }
            }
        }
        slot.clone().unwrap()
    };
    let (bytes, etag, modified) = (&cached.0, &cached.1, cached.2);
    if if_none_match(&headers, etag) {
        let resp = (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag.clone()), (MODIFIED_HEADER, modified.to_string())],
        )
            .into_response();
        return with_headers(&site, resp, false);
    }
    let resp = (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::ETAG, etag.clone()),
            // Revalidate rather than cache blind: the client re-fetches on every
            // full load and must see the server's current copy, but a 304 keeps
            // that cheap.
            (header::CACHE_CONTROL, "public, max-age=0, must-revalidate".to_string()),
            // When the site's content last changed (unix seconds). The editing
            // chrome compares this against the copy it downloaded to tell the
            // visitor the server has moved on (site-chrome.js sync pill).
            (MODIFIED_HEADER, modified.to_string()),
        ],
        bytes.clone(),
    )
        .into_response();
    with_headers(&site, resp, false)
}
