//! Integration test for the archive-less host mode — `uapp serve --chrome <dir>`
//! with no file (`src/webhost.rs`): the browser build alone, index.html and
//! launcher included, a default-open meta injected, COI headers on documents,
//! and nothing served that is not in the bundle.

use std::sync::Arc;
use uapp::webhost::{router, WebHost};

fn fake_bundle() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("uapp-webhost-{}", fastrand::u32(..)));
    std::fs::create_dir_all(dir.join("shell")).unwrap();
    std::fs::create_dir_all(dir.join("examples")).unwrap();
    std::fs::write(dir.join("boot.js"), "// boot").unwrap();
    std::fs::write(dir.join("sw.js"), "// sw").unwrap();
    std::fs::write(dir.join("site-chrome.js"), "// chrome").unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><html><head><title>uapp</title></head><body></body></html>").unwrap();
    std::fs::write(dir.join("launcher.uapp"), b"SQLite format 3\0fake").unwrap();
    std::fs::write(dir.join("shell/main.js"), "// main").unwrap();
    std::fs::write(dir.join("examples/x.uapp"), b"nope").unwrap();
    std::fs::write(dir.join("uapp_wasm.d.ts"), "nope").unwrap();
    dir
}

fn spawn(dir: &std::path::Path, coi: bool, open: Option<&str>) -> String {
    let chrome = uapp::server::load_bundle(dir, true).unwrap();
    let host = Arc::new(WebHost { chrome, coi, open: open.map(String::from) });
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            axum::serve(listener, router(host)).await.unwrap();
        });
    });
    format!("http://127.0.0.1:{}", rx.recv().unwrap())
}

fn get(url: &str) -> (u16, Vec<(String, String)>, String) {
    let r = match ureq::get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("GET {url}: {e}"),
    };
    let status = r.status();
    let headers: Vec<(String, String)> = r
        .headers_names()
        .into_iter()
        .map(|n| (n.to_lowercase(), r.header(&n).unwrap_or("").to_string()))
        .collect();
    (status, headers, r.into_string().unwrap_or_default())
}
fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[test]
fn serves_the_bundle_with_index_and_launcher_and_nothing_else() {
    let dir = fake_bundle();
    let base = spawn(&dir, true, Some("/uapp/apps/launcher.uapp"));

    let (st, h, body) = get(&format!("{base}/"));
    assert_eq!(st, 200);
    assert!(header(&h, "content-type").unwrap().starts_with("text/html"));
    assert!(body.contains(r#"<meta name="uapp-open" content="/uapp/apps/launcher.uapp">"#), "{body}");
    assert!(body.find("uapp-open").unwrap() < body.find("<title>").unwrap(), "meta goes at the top of head");
    assert_eq!(header(&h, "cross-origin-opener-policy"), Some("same-origin"));
    assert_eq!(header(&h, "cross-origin-embedder-policy"), Some("credentialless"));
    assert_eq!(header(&h, "cache-control"), Some("public, max-age=0, must-revalidate"));
    let etag = header(&h, "etag").unwrap().to_string();

    // /index.html is the same document; a matching validator is a 304.
    let (st, _, body2) = get(&format!("{base}/index.html"));
    assert_eq!(st, 200);
    assert_eq!(body, body2);
    let r = ureq::get(&format!("{base}/")).set("If-None-Match", &etag).call();
    assert_eq!(r.as_ref().map(|r| r.status()).ok(), Some(304), "{r:?}");

    // The bundle, launcher included; non-HTML gets no COI headers.
    let (st, h, body) = get(&format!("{base}/launcher.uapp"));
    assert_eq!(st, 200);
    assert!(body.starts_with("SQLite format 3"));
    assert_eq!(header(&h, "cross-origin-opener-policy"), None);
    assert_eq!(get(&format!("{base}/boot.js")).0, 200);
    assert_eq!(get(&format!("{base}/shell/main.js")).0, 200);
    assert_eq!(get(&format!("{base}/health")).0, 200);

    // Build leftovers and the old static examples/ never ship; nothing outside
    // the bundle exists.
    assert_eq!(get(&format!("{base}/examples/x.uapp")).0, 404);
    assert_eq!(get(&format!("{base}/uapp_wasm.d.ts")).0, 404);
    assert_eq!(get(&format!("{base}/site.uapp")).0, 404);
    assert_eq!(get(&format!("{base}/app/index.html")).0, 404);
    assert_eq!(get(&format!("{base}/ws")).0, 404);
    // Writes have no home here: publishing goes to wherever the archive came from.
    let r = ureq::put(&format!("{base}/site.uapp")).send_bytes(b"x");
    assert!(matches!(r, Err(ureq::Error::Status(405, _))), "{r:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_default_or_coi_the_page_is_served_plain() {
    let dir = fake_bundle();
    let base = spawn(&dir, false, None);
    let (st, h, body) = get(&format!("{base}/"));
    assert_eq!(st, 200);
    assert!(!body.contains("uapp-open"), "{body}");
    assert_eq!(header(&h, "cross-origin-opener-policy"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_sites_editing_chrome_still_drops_index_and_launcher() {
    // The other loader, unchanged: a hosted site's own index.html must win.
    let dir = fake_bundle();
    let chrome = uapp::server::load_bundle(&dir, false).unwrap();
    assert!(!chrome.bundle.contains_key("/index.html"));
    assert!(!chrome.bundle.contains_key("/launcher.uapp"));
    assert!(chrome.bundle.contains_key("/boot.js"));
    assert!(chrome.bundle.contains_key("/chrome.js"));
    let _ = std::fs::remove_dir_all(&dir);
}
