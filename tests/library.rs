//! Integration test for `uapp-library` (`src/library.rs`): a directory of
//! .uapp files over HTTP. Reads are anonymous; PUT and DELETE need the token;
//! what gets STORED is the sanitized archive — never the uploader's config
//! (API keys), chat or history — and a publish over a moved-on copy is a 409.

use std::sync::Arc;
use uapp::library::{router, Library};

const TOKEN: &str = "lib-token-0123456789";

fn spawn(dir: &std::path::Path) -> String {
    let lib = Arc::new(Library { dir: dir.to_path_buf(), token: TOKEN.into(), max_bytes: 8 * 1024 * 1024 });
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            axum::serve(listener, router(lib)).await.unwrap();
        });
    });
    format!("http://127.0.0.1:{}", rx.recv().unwrap())
}

fn tmpdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uapp-library-{}", fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A realistic upload: an app with a page, a user table with rows, a chat
/// message, and an API key in its config.
fn sample_app(dir: &std::path::Path) -> Vec<u8> {
    let path = dir.join("src.uapp");
    uapp::store::bootstrap(&path, "Kanban", None).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT OR REPLACE INTO sqlar(name,mode,mtime,sz,data) VALUES('app/index.html',420,0,11,CAST('<h1>hi</h1>' AS BLOB));
         INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES('data/notes.csv',420,0,3,CAST('a,b' AS BLOB));
         CREATE TABLE cards(id INTEGER PRIMARY KEY, title TEXT);
         INSERT INTO cards(title) VALUES('one'),('two');
         INSERT INTO uapp_config(key,value) VALUES('ai','{\"provider\":\"anthropic\",\"api_key\":\"sk-SECRET\"}');
         INSERT INTO uapp_config(key,value) VALUES('toolbar','{\"hidden\":true}');
         INSERT INTO uapp_chat(mid,ts,ctr,device,user,role,content,session) VALUES('m1',1,1,'d','u','user','private words','s1');",
    )
    .unwrap();
    drop(conn);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    bytes
}

fn req(method: &str, url: &str, token: Option<&str>, if_match: Option<&str>, body: Option<&[u8]>) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut r = ureq::request(method, url);
    if let Some(t) = token {
        r = r.set("Authorization", &format!("Bearer {t}"));
    }
    if let Some(m) = if_match {
        r = r.set("If-Match", m);
    }
    let resp = match body {
        Some(b) => r.send_bytes(b),
        None => r.call(),
    };
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{method} {url}: {e}"),
    };
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers_names()
        .into_iter()
        .map(|n| (n.to_lowercase(), resp.header(&n).unwrap_or("").to_string()))
        .collect();
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).unwrap();
    (status, headers, out)
}
fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}
fn json(b: &[u8]) -> serde_json::Value {
    serde_json::from_slice(b).unwrap_or(serde_json::Value::Null)
}

#[test]
fn publish_fetch_conflict_delete() {
    let dir = tmpdir();
    let base = spawn(&dir);
    let upload = sample_app(&dir);
    let url = format!("{base}/kanban.uapp");

    // Empty shelf, anonymous listing.
    let (st, _, b) = req("GET", &format!("{base}/"), None, None, None);
    assert_eq!(st, 200);
    assert_eq!(json(&b)["apps"].as_array().unwrap().len(), 0);

    // Writes need the token; garbage is refused even with it.
    assert_eq!(req("PUT", &url, None, None, Some(&upload)).0, 403);
    assert_eq!(req("PUT", &url, Some("wrong"), None, Some(&upload)).0, 403);
    assert_eq!(req("PUT", &url, Some(TOKEN), None, Some(b"<html>not sqlite</html> and some padding to pass the length check .................")).0, 400);
    assert_eq!(req("PUT", &format!("{base}/a%20b.uapp"), Some(TOKEN), None, Some(&upload)).0, 400);
    assert_eq!(req("PUT", &format!("{base}/notes.txt"), Some(TOKEN), None, Some(&upload)).0, 400);
    assert_eq!(req("GET", &url, None, None, None).0, 404);

    // A real publish.
    let (st, h, b) = req("PUT", &url, Some(TOKEN), None, Some(&upload));
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&b));
    let j = json(&b);
    assert_eq!(j["ok"], true);
    let etag = header(&h, "etag").unwrap().to_string();
    assert_eq!(j["etag"], etag);
    // No temp files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert_eq!(leftovers, vec!["kanban.uapp".to_string()], "{leftovers:?}");

    // Anyone can fetch it — and what they get is SANITIZED.
    let (st, h, got) = req("GET", &url, None, None, None);
    assert_eq!(st, 200);
    assert_eq!(header(&h, "etag"), Some(etag.as_str()));
    assert_eq!(header(&h, "content-type"), Some("application/octet-stream"));
    assert!(got.starts_with(b"SQLite format 3\0"));
    let check = std::env::temp_dir().join(format!("uapp-library-check-{}.uapp", fastrand::u32(..)));
    std::fs::write(&check, &got).unwrap();
    let conn = rusqlite::Connection::open(&check).unwrap();
    let n = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    // Config travels minus secrets: the provider choice stays, the key goes.
    let ai: String = conn.query_row("SELECT value FROM uapp_config WHERE key='ai'", [], |r| r.get(0)).unwrap();
    assert!(ai.contains("anthropic") && !ai.contains("sk-SECRET") && !ai.contains("api_key"), "{ai}");
    assert!(!String::from_utf8_lossy(&got).contains("sk-SECRET"), "the API key must not be on the shelf");
    assert_eq!(n("SELECT count(*) FROM uapp_chat"), 0, "chat is private");
    assert_eq!(n("SELECT count(*) FROM cards"), 2, "user rows travel");
    assert_eq!(n("SELECT count(*) FROM sqlar WHERE name='app/index.html'"), 1);
    assert_eq!(n("SELECT count(*) FROM sqlar WHERE name='data/notes.csv'"), 1, "data/ travels (a library is a public shelf)");
    let name: String = conn.query_row("SELECT value FROM uapp_meta WHERE key='name'", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "Kanban");
    drop(conn);
    let _ = std::fs::remove_file(&check);

    // Validators: a matching one is a 304; the listing carries the same ETag.
    let r = ureq::get(&url).set("If-None-Match", &etag).call();
    assert_eq!(r.as_ref().map(|r| r.status()).ok(), Some(304), "{r:?}");
    let (_, _, b) = req("GET", &format!("{base}/index.json"), None, None, None);
    let apps = json(&b)["apps"].clone();
    assert_eq!(apps.as_array().unwrap().len(), 1);
    assert_eq!(apps[0]["name"], "kanban.uapp");
    assert_eq!(apps[0]["etag"], etag);
    assert_eq!(apps[0]["size"].as_u64().unwrap() as usize, got.len());

    // A publish from a copy that is not the current one is a conflict — and
    // the shelf is untouched by it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (st, h, b) = req("PUT", &url, Some(TOKEN), Some("\"stale-etag\""), Some(&upload));
    assert_eq!(st, 409, "{}", String::from_utf8_lossy(&b));
    assert_eq!(header(&h, "etag"), Some(etag.as_str()));
    assert_eq!(req("GET", &url, None, None, None).1.iter().find(|(k, _)| k == "etag").unwrap().1, etag);
    // From the current copy it goes through, and the ETag moves on.
    let (st, h, _) = req("PUT", &url, Some(TOKEN), Some(&etag), Some(&upload));
    assert_eq!(st, 200);
    assert_ne!(header(&h, "etag"), Some(etag.as_str()));

    // Delete: token only.
    assert_eq!(req("DELETE", &url, None, None, None).0, 403);
    assert_eq!(req("DELETE", &url, Some(TOKEN), None, None).0, 200);
    assert_eq!(req("GET", &url, None, None, None).0, 404);
    assert_eq!(req("DELETE", &url, Some(TOKEN), None, None).0, 404);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_query_token_works_too_and_hidden_files_are_invisible() {
    let dir = tmpdir();
    let base = spawn(&dir);
    let upload = sample_app(&dir);
    std::fs::write(dir.join(".secret.uapp"), &upload).unwrap();
    std::fs::write(dir.join("README.md"), "not an app").unwrap();
    let (st, _, _) = req("PUT", &format!("{base}/todo.uapp?t={TOKEN}"), None, None, Some(&upload));
    assert_eq!(st, 200);
    let (_, _, b) = req("GET", &format!("{base}/"), None, None, None);
    let names: Vec<String> = json(&b)["apps"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["todo.uapp".to_string()]);
    assert_eq!(req("GET", &format!("{base}/.secret.uapp"), None, None, None).0, 404);
    assert_eq!(req("GET", &format!("{base}/README.md"), None, None, None).0, 404);
    let _ = std::fs::remove_dir_all(&dir);
}
