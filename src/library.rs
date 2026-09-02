//! `uapp-library`: a directory of `.uapp` files over HTTP — the place a shell
//! fetches archives from (`?open=<url>`, a launcher's samples, a desktop's
//! launchers) and publishes them back to.
//!
//! Deliberately NOT part of `uapp-server`: that binary serves one archive as a
//! website, or the bare browser build, and knows nothing about collections.
//! This one knows nothing about apps beyond "is it a .uapp, and is it safe to
//! hand out".
//!
//!   GET  /              JSON listing: `{"apps":[{name,size,etag,modified}]}`
//!   GET  /<name>.uapp   the file, with an ETag (If-None-Match honoured)
//!   PUT  /<name>.uapp   store an archive (token). If-Match against the current
//!                       ETag makes a publish over a newer copy a 409.
//!   DELETE /<name>.uapp remove it (token)
//!
//! Every stored archive goes through [`store::export_public`] first: app
//! files, user tables WITH rows, `data/` — but no chat, no history and no
//! `uapp_config`, which is where API keys live. So an uploaded archive can
//! never leak a key to the next visitor who opens it, whatever the uploader
//! sent. Writes are temp-file-plus-rename; a reader never sees a half file.
//!
//! The token (`--token` / `UAPP_TOKEN`) arrives as `Authorization: Bearer` or
//! `?t=`. Reads are anonymous: a library is a public shelf.

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Library {
    pub dir: PathBuf,
    pub token: String,
    /// Largest upload accepted, in bytes.
    pub max_bytes: usize,
}

pub fn router(lib: Arc<Library>) -> Router {
    let max = lib.max_bytes;
    Router::new()
        .route("/", get(list))
        .route("/index.json", get(list))
        .route("/health", get(health))
        .route(
            "/:name",
            get(fetch).put(put).delete(delete).layer(axum::extract::DefaultBodyLimit::max(max)),
        )
        .with_state(lib)
}

async fn health() -> Response {
    ([("x-uapp-version", env!("CARGO_PKG_VERSION"))], "ok").into_response()
}

/// The token holder. Same shape as the server's `Owner`: decided once, by the
/// extractor, so no handler checks tokens itself.
pub struct Owner;

#[axum::async_trait]
impl axum::extract::FromRequestParts<Arc<Library>> for Owner {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut axum::http::request::Parts, lib: &Arc<Library>) -> Result<Self, Response> {
        if lib.token.is_empty() {
            return Err(deny());
        }
        let q_token = parts.uri.query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("t="));
        if q_token == Some(lib.token.as_str()) {
            return Ok(Owner);
        }
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.trim() == lib.token)
            .unwrap_or(false);
        if bearer { Ok(Owner) } else { Err(deny()) }
    }
}

fn deny() -> Response {
    (StatusCode::FORBIDDEN, "uapp-library: missing or bad token").into_response()
}

fn bad(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": msg.into() }))).into_response()
}

/// A file name this library will store or serve: `<stem>.uapp`, where the stem
/// is letters, digits, `.`, `_`, `-`, does not start with a dot, and is short.
/// Nothing else — no separators, no traversal, no hidden files.
pub fn valid_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".uapp") else { return false };
    !stem.is_empty()
        && stem.len() <= 120
        && !stem.starts_with('.')
        && stem.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Cheap, strong-enough validator: the file's length and mtime. Our own
/// writes are atomic renames, which always move the mtime on.
fn etag_of(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", meta.len(), mtime)
}

fn modified_ms(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

async fn list(State(lib): State<Arc<Library>>) -> Response {
    let mut apps = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&lib.dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !valid_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            apps.push(json!({
                "name": name,
                "size": meta.len(),
                "etag": etag_of(&meta),
                "modified": modified_ms(&meta) as u64,
            }));
        }
    }
    apps.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(json!({ "apps": apps })),
    )
        .into_response()
}

async fn fetch(State(lib): State<Arc<Library>>, AxPath(name): AxPath<String>, headers: HeaderMap) -> Response {
    if !valid_name(&name) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = lib.dir.join(&name);
    let Ok(meta) = std::fs::metadata(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let etag = etag_of(&meta);
    if crate::server::if_none_match(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::ETAG, etag),
            // A shell compares ETags to know whether the shelf moved on; never
            // let a cache answer that for us.
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::CONTENT_DISPOSITION, format!("inline; filename=\"{name}\"")),
        ],
        bytes,
    )
        .into_response()
}

async fn put(
    State(lib): State<Arc<Library>>,
    AxPath(name): AxPath<String>,
    _who: Owner,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_name(&name) {
        return bad("name must be <letters, digits, . _ ->.uapp");
    }
    if body.len() < 100 || !body.starts_with(b"SQLite format 3\0") {
        return bad("not a plain .uapp file (an encrypted one cannot be published to a library)");
    }
    let target = lib.dir.join(&name);
    // If-Match: the ETag of the copy the publisher started from. A shelf that
    // moved on since (someone else published) is a conflict, not an overwrite.
    if let Some(want) = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok()) {
        let now = std::fs::metadata(&target).map(|m| etag_of(&m)).unwrap_or_default();
        if !want.split(',').any(|t| t.trim() == now || t.trim() == "*") {
            return (
                StatusCode::CONFLICT,
                [(header::ETAG, now.clone())],
                axum::Json(json!({ "error": "the library's copy changed since yours was fetched", "etag": now })),
            )
                .into_response();
        }
    }
    let lib2 = lib.clone();
    let name2 = name.clone();
    let stored = tokio::task::spawn_blocking(move || sanitize_and_store(&lib2, &name2, &body)).await;
    match stored {
        Ok(Ok(meta)) => {
            let etag = etag_of(&meta);
            (
                StatusCode::OK,
                [(header::ETAG, etag.clone())],
                axum::Json(json!({ "ok": true, "name": name, "size": meta.len(), "etag": etag })),
            )
                .into_response()
        }
        Ok(Err(e)) => bad(format!("{e:#}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

/// Validate the upload as a .uapp, rebuild it through `export_public` (the
/// same sanitizer a hosted site's `/site.uapp` goes through) and rename the
/// result into place.
fn sanitize_and_store(lib: &Library, name: &str, body: &[u8]) -> anyhow::Result<std::fs::Metadata> {
    use anyhow::Context as _;
    let rand: String = (0..8).map(|_| fastrand::alphanumeric()).collect();
    let upload = lib.dir.join(format!(".{name}.upload-{rand}"));
    let staged = lib.dir.join(format!(".{name}.staged-{rand}"));
    let result = (|| -> anyhow::Result<()> {
        std::fs::write(&upload, body).with_context(|| format!("writing {}", upload.display()))?;
        let conn = rusqlite::Connection::open_with_flags(&upload, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("opening the upload as SQLite")?;
        let has_sqlar: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sqlar'", [], |r| r.get(0))
            .context("reading the upload's schema")?;
        anyhow::ensure!(has_sqlar == 1, "not a .uapp: no sqlar table");
        let app_name = crate::store::meta_get(&conn, "name")
            .ok()
            .flatten()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| name.trim_end_matches(".uapp").to_string());
        let clean = crate::store::export_public(&conn, &app_name, true).context("sanitizing the archive")?;
        drop(conn);
        std::fs::write(&staged, &clean).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, lib.dir.join(name)).context("moving the archive into place")?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&upload);
    let _ = std::fs::remove_file(&staged);
    result?;
    Ok(std::fs::metadata(lib.dir.join(name))?)
}

async fn delete(State(lib): State<Arc<Library>>, AxPath(name): AxPath<String>, _who: Owner) -> Response {
    if !valid_name(&name) {
        return bad("bad name");
    }
    match std::fs::remove_file(lib.dir.join(&name)) {
        Ok(()) => axum::Json(json!({ "ok": true, "name": name })).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn names_are_plain_uapp_files_only() {
        for good in ["a.uapp", "kanban-board.uapp", "My_App.2.uapp", "launcher.uapp"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["a", ".uapp", ".hidden.uapp", "../x.uapp", "a/b.uapp", "a b.uapp", "x.UAPP", "a.uapp.upload-1"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
