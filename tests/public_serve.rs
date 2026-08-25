//! Integration test for public ("hosted site") mode — `src/public.rs`.
//!
//! The interesting cases here are all negative. A public origin has no token
//! and no loopback check, so anything the router reaches is world-readable:
//!   * `data/` uploads must be unreachable, including via the `data/<name>`
//!     fallback baked into `store::name_candidates` (which is why this module
//!     resolves names itself instead of calling `store::sqlar_read`).
//!   * The privileged routes must not exist at all.
//!   * `/site.uapp` must carry the site's own tables WITH rows, but no chat,
//!     no `data/` files and no config secrets.

use uapp::public::{self, PublicOpts};
use uapp::store;

/// Serve `db` on an ephemeral port and return its base URL plus a shutdown
/// handle. Runs the server on its own thread with its own runtime.
fn spawn(path: &std::path::Path, opts: PublicOpts) -> String {
    let db = public::open_readonly(path, None).unwrap();
    let site = public::site(db, opts);
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            axum::serve(listener, public::router(site)).await.unwrap();
        });
    });
    let port = rx.recv().unwrap();
    // The listener is bound before the port is sent, so a request now queues
    // rather than being refused.
    format!("http://127.0.0.1:{port}")
}

fn status(url: &str) -> u16 {
    match ureq::get(url).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("request to {url} failed: {e}"),
    }
}

fn body(url: &str) -> String {
    ureq::get(url).call().unwrap().into_string().unwrap()
}

fn fixture() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uapp-public-{}", fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("site.uapp");
    store::bootstrap(&path, "site", None).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let now = (store::now_ms() / 1000) as i64;
    let put = |name: &str, text: &str| {
        store::sqlar_write(&conn, name, now, text.as_bytes()).unwrap();
    };
    put("app/index.html", "<html><head><title>Home</title></head><body>home</body></html>");
    put("app/posts/hello.html", "<html><head><title>Hello</title></head><body>hello</body></html>");
    put("app/about/index.html", "<html><head><title>About</title></head><body>about</body></html>");
    put("app/404.html", "<html><head><title>Gone</title></head><body>no such page</body></html>");
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

#[test]
fn serves_pages_at_real_urls() {
    let path = fixture();
    let base = spawn(&path, PublicOpts::default());

    assert_eq!(status(&format!("{base}/")), 200);
    assert!(body(&format!("{base}/")).contains("home"));
    // Extensionless and directory forms both resolve, so posts can be linked
    // as /posts/hello as well as /posts/hello.html.
    assert!(body(&format!("{base}/posts/hello")).contains("hello"));
    assert!(body(&format!("{base}/posts/hello.html")).contains("hello"));
    assert!(body(&format!("{base}/about/")).contains("about"));
    assert!(body(&format!("{base}/about")).contains("about"));
    // A missing page gets the site's 404 document, with a 404 status.
    assert_eq!(status(&format!("{base}/nope")), 404);
    // ...but 404.html requested directly is a normal page.
    assert_eq!(status(&format!("{base}/404.html")), 200);
    // The viewport meta is injected; `/uapp.js` (privileged, and not served
    // here) is NOT.
    let home = body(&format!("{base}/"));
    assert!(home.contains("name=\"viewport\""));
    assert!(!home.contains("uapp.js"));
}

#[test]
fn never_serves_user_data_or_privileged_routes() {
    let path = fixture();
    let base = spawn(&path, PublicOpts::default());

    // `store::name_candidates` would resolve every one of these to
    // `data/secret.csv`. Public mode must not.
    for p in ["/data/secret.csv", "/secret.csv", "/uploads/secret.csv"] {
        assert_eq!(status(&format!("{base}{p}")), 404, "{p} must not be served");
        assert!(
            !body_or_empty(&format!("{base}{p}")).contains("ssn,salary"),
            "{p} leaked user data"
        );
    }
    // Nothing privileged is registered at all.
    for p in [
        "/ws",
        "/upload",
        "/download.uapp",
        "/template.uapp",
        "/uapp.js",
        "/shell/main.js",
        "/shell.css",
        "/scratch/",
        "/icons.js",
    ] {
        assert_eq!(status(&format!("{base}{p}")), 404, "{p} must not exist");
    }
}

#[test]
fn archive_carries_site_tables_but_no_secrets() {
    let path = fixture();
    let base = spawn(&path, PublicOpts::default());

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut ureq::get(&format!("{base}/site.uapp")).call().unwrap().into_reader(),
        &mut bytes,
    )
    .unwrap();

    // Not just absent from the tables — absent from the file's bytes, so a
    // freed page can't carry them either.
    let hay = String::from_utf8_lossy(&bytes);
    assert!(!hay.contains("sk-LEAKED"), "api key leaked into /site.uapp");
    assert!(!hay.contains("private chat"), "chat leaked into /site.uapp");
    assert!(!hay.contains("ssn,salary"), "data/ file leaked into /site.uapp");

    let out = std::env::temp_dir().join(format!("uapp-pubarch-{}.uapp", fastrand::u32(..)));
    std::fs::write(&out, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();

    // The site's own SQL machinery arrives WITH its rows — that is the whole
    // point of `export_public` over `export_template`.
    let posts: i64 = conn.query_row("SELECT count(*) FROM posts", [], |r| r.get(0)).unwrap();
    assert_eq!(posts, 1, "user table rows must be copied");
    let chat: i64 = conn.query_row("SELECT count(*) FROM uapp_chat", [], |r| r.get(0)).unwrap();
    assert_eq!(chat, 0, "chat must be empty");
    // App files yes, data/ files no.
    let app: i64 = conn
        .query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'app/%'", [], |r| r.get(0))
        .unwrap();
    assert!(app >= 4, "app files must be present (got {app})");
    let data: i64 = conn
        .query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'data/%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(data, 0, "data/ files must not be exported");
    // The provider survives, the key does not.
    let ai: String = conn
        .query_row("SELECT value FROM uapp_config WHERE key='ai'", [], |r| r.get(0))
        .unwrap();
    assert!(ai.contains("anthropic") && !ai.contains("api_key"));

    let _ = std::fs::remove_file(&out);
}

#[test]
fn archive_includes_data_sources_only_when_asked() {
    let path = fixture();
    let base = spawn(&path, PublicOpts { export_data: true, ..Default::default() });

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut ureq::get(&format!("{base}/site.uapp")).call().unwrap().into_reader(),
        &mut bytes,
    )
    .unwrap();
    let out = std::env::temp_dir().join(format!("uapp-pubdata-{}.uapp", fastrand::u32(..)));
    std::fs::write(&out, &bytes).unwrap();
    let conn = rusqlite::Connection::open(&out).unwrap();
    let data: i64 = conn
        .query_row("SELECT count(*) FROM sqlar WHERE name LIKE 'data/%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(data, 1, "data/ sources must ship when export_data is on");
    // Secrets are still stripped — this flag only widens the FILE set.
    let hay = String::from_utf8_lossy(&bytes);
    assert!(!hay.contains("sk-LEAKED"));
    assert!(!hay.contains("private chat"));
    let _ = std::fs::remove_file(&out);

    // ...and it never changes what HTTP will serve.
    assert_eq!(status(&format!("{base}/data/secret.csv")), 404);
    assert_eq!(status(&format!("{base}/secret.csv")), 404);
}

#[test]
fn archive_can_be_disabled() {
    let path = fixture();
    let base = spawn(&path, PublicOpts { archive: false, ..Default::default() });
    assert_eq!(status(&format!("{base}/site.uapp")), 404);
}

fn body_or_empty(url: &str) -> String {
    ureq::get(url).call().map(|r| r.into_string().unwrap_or_default()).unwrap_or_default()
}
