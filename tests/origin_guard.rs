//! The private (desktop) server only answers its own pages. The token cookie
//! alone can't ensure that: cookies aren't scoped to a port, so a page on any
//! other loopback port is "same-site" and gets our cookie sent along. These
//! check that such requests are refused while the shell's own (and
//! non-browser clients') still work.

use std::io::{Read, Write};
use uapp::app::App;

const TOKEN: &str = "test-token-0123456789";

fn spawn() -> (u16, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let eng = uapp::engine::Engine::open(
        dir.path().join("o.uapp"),
        "test-dev".into(),
        "tester".into(),
    )
    .unwrap();
    let app = std::sync::Arc::new(App::new(eng, TOKEN.into(), false));
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            app.port.store(port, std::sync::atomic::Ordering::Relaxed);
            tx.send(port).unwrap();
            axum::serve(listener, uapp::server::router(app))
                .await
                .unwrap();
        });
    });
    (rx.recv().unwrap(), dir)
}

/// A scratch directory removed on drop (no extra dev-dependency for it).
mod tempdir {
    pub struct Dir(std::path::PathBuf);
    impl Dir {
        pub fn new() -> Dir {
            let p = std::env::temp_dir().join(format!(
                "uapp-origin-{}-{}",
                std::process::id(),
                fastrand::u64(..)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// One raw HTTP/1.1 request; returns the status code and the head.
fn raw(port: u16, req: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head)
}

fn upload(port: u16, extra: &str) -> u16 {
    raw(
        port,
        &format!(
            "POST /upload?name=data/x.txt HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Cookie: uapp_t_{port}={TOKEN}\r\nContent-Type: text/plain\r\n{extra}\
             Content-Length: 5\r\nConnection: close\r\n\r\nhello"
        ),
    )
    .0
}

fn ws(port: u16, extra: &str) -> u16 {
    raw(
        port,
        &format!(
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: uapp_t_{port}={TOKEN}\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n{extra}\r\n"
        ),
    )
    .0
}

#[test]
fn another_loopback_port_is_refused() {
    let (port, _dir) = spawn();
    let other = port.wrapping_add(1);
    // What a page on another port sends: its own Origin, same-site.
    assert_eq!(
        upload(
            port,
            &format!("Origin: http://127.0.0.1:{other}\r\nSec-Fetch-Site: same-site\r\n")
        ),
        403
    );
    assert_eq!(
        upload(port, &format!("Origin: http://localhost:{port}\r\n")),
        403,
        "a different host name is a different origin"
    );
    assert_eq!(upload(port, "Origin: null\r\n"), 403);
    assert_eq!(
        ws(port, &format!("Origin: http://127.0.0.1:{other}\r\n")),
        403
    );
    // Reads too: a cross-port <script src> / <img> of an archive file.
    let (st, _) = raw(
        port,
        &format!(
            "GET /download.uapp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: uapp_t_{port}={TOKEN}\r\n\
             Sec-Fetch-Site: same-site\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(st, 403);
}

#[test]
fn our_own_pages_and_non_browser_clients_still_work() {
    let (port, _dir) = spawn();
    let own = format!("Origin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\n");
    assert_eq!(upload(port, &own), 200);
    assert_eq!(
        upload(port, ""),
        200,
        "no Origin = not a browser (curl, CLI)"
    );
    assert_eq!(ws(port, &own), 101);
    assert_eq!(ws(port, ""), 101);
    // A typed-in URL / the native window's first load.
    let (st, head) = raw(
        port,
        &format!(
            "GET /?t={TOKEN} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: none\r\n\
             Sec-Fetch-Dest: document\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(st, 200);
    let head = head.to_ascii_lowercase();
    assert!(head.contains("content-security-policy: default-src 'self'; script-src 'self'"));
    assert!(head.contains("frame-ancestors 'self'"));
}

#[test]
fn every_response_refuses_foreign_framing() {
    let (port, _dir) = spawn();
    for path in ["/app/", "/uapp.js", "/health", "/nope"] {
        let (_, head) = raw(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: uapp_t_{port}={TOKEN}\r\n\
                 Connection: close\r\n\r\n"
            ),
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("content-security-policy: frame-ancestors 'self'"),
            "{path}: {head}"
        );
    }
}
