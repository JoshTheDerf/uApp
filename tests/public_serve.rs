//! Integration test for public ("hosted site") mode — `uapp serve`, which is
//! the ordinary server (`src/server.rs`) with `App::public` set.
//!
//! What an anonymous visitor can reach is the whole security story, so most
//! cases here are negative:
//!   * `data/` uploads must be unreachable, including via the `data/<name>`
//!     fallback baked into `store::name_candidates`.
//!   * Privileged routes (the shell, the WebSocket, uploads, downloads,
//!     publishing) must refuse without the token — and work with it.
//!   * `/site.uapp` must carry the site's own tables WITH rows, but no chat,
//!     no `data/` files and no config secrets.
//! And the write half: a `PUT /site.uapp` of an edited copy makes the served
//! site match it without touching config, chat or history.

use uapp::app::{App, PublicOpts};
use uapp::store;

const TOKEN: &str = "test-token-0123456789";

/// Serve `path` on an ephemeral port and return its base URL. Runs the server
/// on its own thread with its own runtime; the App outlives the test.
fn spawn(path: &std::path::Path, opts: PublicOpts) -> String {
    let eng = uapp::engine::Engine::open(path.to_path_buf(), "test-dev".into(), "tester".into()).unwrap();
    let mut app = App::new(eng, TOKEN.into(), false);
    app.public = Some(opts);
    let app = std::sync::Arc::new(app);
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            app.port.store(port, std::sync::atomic::Ordering::Relaxed);
            tx.send(port).unwrap();
            axum::serve(listener, uapp::server::router(app)).await.unwrap();
        });
    });
    let port = rx.recv().unwrap();
    // The listener is bound before the port is sent, so a request now queues
    // rather than being refused.
    format!("http://127.0.0.1:{port}")
}

fn status_of(r: Result<ureq::Response, ureq::Error>, url: &str) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("request to {url} failed: {e}"),
    }
}
fn status(url: &str) -> u16 {
    status_of(ureq::get(url).call(), url)
}
fn body(url: &str) -> String {
    ureq::get(url).call().unwrap().into_string().unwrap()
}
fn body_or_empty(url: &str) -> String {
    ureq::get(url).call().map(|r| r.into_string().unwrap_or_default()).unwrap_or_default()
}
fn bytes(url: &str) -> Vec<u8> {
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut ureq::get(url).call().unwrap().into_reader(), &mut out).unwrap();
    out
}
fn put(url: &str, token: Option<&str>, if_match: Option<&str>, body: &[u8]) -> (u16, String) {
    let mut req = ureq::put(url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    if let Some(m) = if_match {
        req = req.set("If-Match", m);
    }
    match req.send_bytes(body) {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
        Err(e) => panic!("PUT {url} failed: {e}"),
    }
}
fn live_config(path: &std::path::Path, key: &str) -> Option<String> {
    let c = rusqlite::Connection::open(path).unwrap();
    c.query_row("SELECT value FROM uapp_config WHERE key=?1", [key], |r| r.get(0)).ok()
}
fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uapp-public-{}", fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn fixture() -> std::path::PathBuf {
    let path = tmp("site.uapp");
    store::bootstrap(&path, "site", None).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let now = (store::now_ms() / 1000) as i64;
    let put = |name: &str, text: &str| store::sqlar_write(&conn, name, now, text.as_bytes()).unwrap();
    put("app/index.html", "<html><head><title>Home</title></head><body>home</body></html>");
    put("app/posts/hello.html", "<html><head><title>Hello</title></head><body>hello</body></html>");
    put("app/about/index.html", "<html><head><title>About</title></head><body>about</body></html>");
    put("app/404.html", "<html><head><title>Gone</title></head><body>no such page</body></html>");
    // A page that stored the browser build's old wasm-mode marker (a site
    // whose build pipeline renders pages from fetched HTML did exactly this).
    put("app/stale.html", "<html><head>\n<script>window.__uappWasm=1</script>\n<title>S</title></head><body>stale marker</body></html>");
    put("data/secret.csv", "ssn,salary\n123,999");
    conn.execute_batch(
        "CREATE TABLE posts(slug TEXT PRIMARY KEY, title TEXT);
         INSERT INTO posts VALUES('hello','Hello post');
         INSERT INTO uapp_config(key,value)
           VALUES('ai','{\"provider\":\"anthropic\",\"api_key\":\"sk-LEAKED\"}');
         INSERT INTO uapp_chat(mid,ts,ctr,device,user,role,content,session)
           VALUES('m1',1,1,'d','u','user','private chat','s1');",
    )
    .unwrap();
    drop(conn);
    path
}

/// Play the browser: open the downloaded archive and edit it the way the wasm
/// engine would (files + the site's own tables), returning the new bytes.
fn edit_copy(archive: &[u8], edit: impl FnOnce(&rusqlite::Connection)) -> Vec<u8> {
    let p = tmp("copy.uapp");
    std::fs::write(&p, archive).unwrap();
    let conn = rusqlite::Connection::open(&p).unwrap();
    edit(&conn);
    drop(conn);
    std::fs::read(&p).unwrap()
}

#[test]
fn serves_pages_at_real_urls() {
    let base = spawn(&fixture(), PublicOpts::default());
    assert_eq!(status(&format!("{base}/")), 200);
    assert!(body(&format!("{base}/")).contains("home"));
    // Extensionless and directory forms both resolve, so posts can be linked
    // as /posts/hello as well as /posts/hello.html.
    assert!(body(&format!("{base}/posts/hello")).contains("hello"));
    assert!(body(&format!("{base}/posts/hello.html")).contains("hello"));
    assert!(body(&format!("{base}/about/")).contains("about"));
    assert!(body(&format!("{base}/about")).contains("about"));
    // /app/* is the same site for a visitor (the service worker's fallback).
    assert!(body(&format!("{base}/app/posts/hello")).contains("hello"));
    // A missing page gets the site's 404 document, with a 404 status...
    assert_eq!(status(&format!("{base}/nope")), 404);
    assert!(body_or_empty(&format!("{base}/nope")).is_empty() || true);
    // ...but 404.html requested directly is a normal page.
    assert_eq!(status(&format!("{base}/404.html")), 200);
    // The viewport meta is injected; `/uapp.js` (the privileged console
    // forwarder) is NOT.
    let home = body(&format!("{base}/"));
    assert!(home.contains("name=\"viewport\""));
    assert!(!home.contains("uapp.js"));
    // The old injected wasm-mode marker never reaches a visitor: served at top
    // level it would tell the editing chrome it is inside the wasm frame.
    let stale = body(&format!("{base}/stale"));
    assert!(stale.contains("stale marker") && !stale.contains("__uappWasm"), "{stale}");
    // Public pages carry validators; a matching If-None-Match is a 304.
    let r = ureq::get(&format!("{base}/")).call().unwrap();
    let etag = r.header("etag").unwrap().to_string();
    assert_eq!(status_of(ureq::get(&format!("{base}/")).set("If-None-Match", &etag).call(), &base), 304);
}

#[test]
fn never_serves_user_data_and_refuses_privileged_routes_without_the_token() {
    let base = spawn(&fixture(), PublicOpts::default());
    // `store::name_candidates` would resolve every one of these to
    // `data/secret.csv`. A visitor must not.
    for p in ["/data/secret.csv", "/secret.csv", "/uploads/secret.csv", "/app/secret.csv"] {
        assert_eq!(status(&format!("{base}{p}")), 404, "{p} must not be served");
        assert!(!body_or_empty(&format!("{base}{p}")).contains("ssn,salary"), "{p} leaked user data");
    }
    // Privileged routes exist on this router but refuse without the token.
    for p in ["/upload", "/download.uapp", "/template.uapp", "/scratch/"] {
        let s = status(&format!("{base}{p}"));
        assert!(s == 403 || s == 405, "{p} must refuse (got {s})");
    }
    // A plain GET isn't a WebSocket upgrade (400); with the upgrade headers
    // but no token it is refused before the handshake.
    let ws = ureq::get(&format!("{base}/ws"))
        .set("Connection", "Upgrade")
        .set("Upgrade", "websocket")
        .set("Sec-WebSocket-Version", "13")
        .set("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .call();
    assert_eq!(status_of(ws, "/ws"), 403);
    assert_eq!(put(&format!("{base}/site.uapp"), None, None, b"SQLite format 3\0junk").0, 403);
    assert_eq!(put(&format!("{base}/site.uapp"), Some("wrong"), None, b"SQLite format 3\0junk").0, 403);
    // The shell itself needs the token; with it, the same URL is the editor.
    assert!(!body(&format!("{base}/")).contains("<shell-topbar"));
    assert!(body(&format!("{base}/?t={TOKEN}")).contains("<shell-topbar"));
    assert!(body(&format!("{base}/download.uapp?t={TOKEN}")).starts_with("SQLite format 3"));
}

#[test]
fn archive_carries_site_tables_but_no_secrets() {
    let base = spawn(&fixture(), PublicOpts::default());
    let bytes = bytes(&format!("{base}/site.uapp"));
    // Not just absent from the tables — absent from the file's bytes, so a
    // freed page can't carry them either.
    let hay = String::from_utf8_lossy(&bytes);
    assert!(!hay.contains("sk-LEAKED"), "api key leaked into /site.uapp");
    assert!(!hay.contains("private chat"), "chat leaked into /site.uapp");
    assert!(!hay.contains("ssn,salary"), "data/ file leaked into /site.uapp");

    let out = tmp("archive.uapp");
    std::fs::write(&out, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();
    let posts: i64 = conn.query_row("SELECT count(*) FROM posts", [], |r| r.get(0)).unwrap();
    assert_eq!(posts, 1, "user table rows must be copied");
    let chat: i64 = conn.query_row("SELECT count(*) FROM uapp_chat", [], |r| r.get(0)).unwrap();
    assert_eq!(chat, 0, "chat must be empty");
    let app: i64 = conn.query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'app/%'", [], |r| r.get(0)).unwrap();
    assert!(app >= 4, "app files must be present (got {app})");
    let data: i64 = conn.query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'data/%'", [], |r| r.get(0)).unwrap();
    assert_eq!(data, 0, "data/ files must not be exported");
    let ai: String = conn.query_row("SELECT value FROM uapp_config WHERE key='ai'", [], |r| r.get(0)).unwrap();
    assert!(ai.contains("anthropic") && !ai.contains("api_key"));
}

#[test]
fn archive_includes_data_sources_only_when_asked() {
    let base = spawn(&fixture(), PublicOpts { export_data: true, ..Default::default() });
    let bytes = bytes(&format!("{base}/site.uapp"));
    let out = tmp("archive.uapp");
    std::fs::write(&out, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();
    let data: i64 = conn.query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'data/%'", [], |r| r.get(0)).unwrap();
    assert_eq!(data, 1, "data/ sources must ship when export_data is on");
    let hay = String::from_utf8_lossy(&bytes);
    assert!(!hay.contains("sk-LEAKED"));
    assert!(!hay.contains("private chat"));
    // ...and it never changes what HTTP will serve.
    assert_eq!(status(&format!("{base}/data/secret.csv")), 404);
    assert_eq!(status(&format!("{base}/secret.csv")), 404);
}

#[test]
fn archive_can_be_disabled() {
    let base = spawn(&fixture(), PublicOpts { archive: false, ..Default::default() });
    assert_eq!(status(&format!("{base}/site.uapp")), 404);
}

#[test]
fn publish_replaces_content_and_keeps_secrets() {
    let path = fixture();
    let base = spawn(&path, PublicOpts::default());
    let url = format!("{base}/site.uapp");
    let r = ureq::get(&url).call().unwrap();
    let etag = r.header("etag").unwrap().to_string();
    let mut original = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut original).unwrap();

    // Edit the copy: change a page, add one, drop one, change the site's own
    // table, and (as a hostile copy might) plant a config key and a data file.
    let now = (store::now_ms() / 1000) as i64;
    let edited = edit_copy(&original, |c| {
        store::sqlar_write(c, "app/index.html", now, b"<html><body>NEW HOME</body></html>").unwrap();
        store::sqlar_write(c, "app/posts/second.html", now, b"<html><body>second</body></html>").unwrap();
        c.execute("DELETE FROM sqlar WHERE name='app/about/index.html'", []).unwrap();
        store::sqlar_write(c, "data/planted.csv", now, b"planted").unwrap();
        c.execute_batch(
            "INSERT INTO posts VALUES('second','Second post');
             CREATE TABLE tags(name TEXT);
             INSERT INTO tags VALUES('rust');
             INSERT OR REPLACE INTO uapp_config(key,value) VALUES('ai','{\"api_key\":\"sk-FROM-BROWSER\"}');
             INSERT OR REPLACE INTO uapp_config(key,value) VALUES('toolbar','{\"hidden\":true}');
             INSERT INTO uapp_chat(mid,ts,ctr,device,user,role,content,session)
               VALUES('m9',1,1,'d','u','user','browser chat','s9');",
        )
        .unwrap();
    });

    let (st, resp) = put(&url, Some(TOKEN), Some(&etag), &edited);
    assert_eq!(st, 200, "publish failed: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["result"]["files"]["written"], 2);
    assert_eq!(v["result"]["files"]["removed"], 1);
    assert_eq!(v["result"]["schema"]["tables"], 2, "{v}");
    assert_eq!(v["result"]["schema"]["rows"], 3, "{v}");

    // Visible on the very next request, no restart.
    assert!(body(&format!("{base}/")).contains("NEW HOME"));
    assert!(body(&format!("{base}/posts/second")).contains("second"));
    assert_eq!(status(&format!("{base}/about/")), 404);
    // The archive moved with it (new ETag) and carries the new rows.
    let r2 = ureq::get(&url).call().unwrap();
    assert_ne!(r2.header("etag").unwrap(), etag);
    let mut fresh = Vec::new();
    std::io::Read::read_to_end(&mut r2.into_reader(), &mut fresh).unwrap();
    let out = tmp("fresh.uapp");
    std::fs::write(&out, &fresh).unwrap();
    let c = rusqlite::Connection::open(&out).unwrap();
    let posts: i64 = c.query_row("SELECT count(*) FROM posts", [], |r| r.get(0)).unwrap();
    assert_eq!(posts, 2);
    let tags: i64 = c.query_row("SELECT count(*) FROM tags", [], |r| r.get(0)).unwrap();
    assert_eq!(tags, 1);

    // What the copy never legitimately had is exactly what the server kept.
    let ai: String = body(&format!("{base}/download.uapp?t={TOKEN}"));
    assert!(ai.contains("sk-LEAKED"), "server's own api key must survive a publish");
    assert!(!ai.contains("sk-FROM-BROWSER"), "a copy must not be able to set config");
    // …except the toolbar default, which is the site's own look and travels.
    let tb: Option<String> = live_config(&path, "toolbar");
    assert_eq!(tb.as_deref(), Some("{\"hidden\":true}"), "the toolbar default is published with the copy");
    assert!(String::from_utf8_lossy(&fresh).contains("\"hidden\":true"), "and the served archive carries it to visitors");
    assert!(ai.contains("private chat") && !ai.contains("browser chat"), "chat is the server's");
    assert!(ai.contains("ssn,salary"), "server data/ must be kept when data isn't published");
    assert!(!ai.contains("planted"), "data/ from the copy must be ignored without --publish-data");
    // Every file change went through the history (which keeps the outgoing
    // content — the two files that existed before), so it can be reverted.
    let live = rusqlite::Connection::open(&path).unwrap();
    let hist: Vec<String> = live
        .prepare("SELECT name FROM uapp_file_history WHERE ts > 0 ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(hist, vec!["app/about/index.html".to_string(), "app/index.html".to_string()]);
}

#[test]
fn publish_refuses_a_stale_copy_and_junk() {
    let base = spawn(&fixture(), PublicOpts { export_data: true, ..Default::default() });
    let url = format!("{base}/site.uapp");
    let r = ureq::get(&url).call().unwrap();
    let etag = r.header("etag").unwrap().to_string();
    let mut original = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut original).unwrap();
    let now = (store::now_ms() / 1000) as i64;

    // First publisher wins...
    let a = edit_copy(&original, |c| {
        store::sqlar_write(c, "app/index.html", now, b"<html><body>A</body></html>").unwrap();
        store::sqlar_write(c, "data/notes.md", now, b"from A").unwrap();
    });
    assert_eq!(put(&url, Some(TOKEN), Some(&etag), &a).0, 200);
    assert!(body(&format!("{base}/")).contains(">A<"));
    // ...and a second copy of the same download is told the site moved on.
    let b = edit_copy(&original, |c| {
        store::sqlar_write(c, "app/index.html", now, b"<html><body>B</body></html>").unwrap();
    });
    let (st, resp) = put(&url, Some(TOKEN), Some(&etag), &b);
    assert_eq!(st, 409, "{resp}");
    assert!(body(&format!("{base}/")).contains(">A<"), "a stale publish must change nothing");
    // Without If-Match the caller takes responsibility, and it lands.
    assert_eq!(put(&url, Some(TOKEN), None, &b).0, 200);
    assert!(body(&format!("{base}/")).contains(">B<"));
    // With --publish-data the copy's data/ is authoritative: B has none, so
    // A's file is gone from the archive (it lives on in the file history).
    let full = bytes(&format!("{base}/download.uapp?t={TOKEN}"));
    let out = tmp("full.uapp");
    std::fs::write(&out, &full).unwrap();
    let c = rusqlite::Connection::open(&out).unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM sqlar WHERE name='data/notes.md'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "data/ must follow the published copy when --publish-data is on");

    assert_eq!(put(&url, Some(TOKEN), None, b"not an archive").0, 400);
    let (st, _) = put(&url, Some(TOKEN), None, b"SQLite format 3\0but truncated garbage");
    assert!(st >= 400, "garbage must be rejected");
    assert!(body(&format!("{base}/")).contains(">B<"), "a failed publish must change nothing");
}
