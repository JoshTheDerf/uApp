//! The password page an encrypted app shows before it is unlocked
//! (`server::run_unlock`). It is reachable by anything that can reach the
//! port, so it must refuse other origins, oversized bodies and rapid guessing.

use std::io::{Read, Write};

fn post(port: u16, origin: Option<&str>, ctype: &str, body: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let origin = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "POST /unlock HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}Content-Type: {ctype}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[test]
fn the_unlock_page_only_takes_careful_guesses_from_itself() {
    let dir = std::env::temp_dir().join(format!("uapp-unlock-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("locked.uapp");
    // Opening a new file with a password creates it encrypted.
    drop(
        uapp::engine::Engine::open_with_passphrase(
            path.clone(),
            "dev".into(),
            "t".into(),
            Some("right-pw".into()),
        )
        .unwrap(),
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let got = rt.block_on(uapp::server::run_unlock(&listener, &p));
        let _ = tx.send(got.ok());
    });

    let me = format!("http://127.0.0.1:{port}");
    let json = "application/json";
    let guess = |pw: &str| format!(r#"{{"password":"{pw}"}}"#);

    // Another origin (a page on another port), no origin at all, a form post.
    let other = format!("http://127.0.0.1:{}", port.wrapping_add(1));
    assert_eq!(post(port, Some(&other), json, &guess("right-pw")).0, 403);
    assert_eq!(post(port, None, json, &guess("right-pw")).0, 403);
    assert_eq!(
        post(port, Some(&me), "text/plain", &guess("right-pw")).0,
        415
    );
    // A body no password needs.
    let big = guess(&"x".repeat(5000));
    assert_eq!(post(port, Some(&me), json, &big).0, 413);
    // DNS rebinding: a foreign Host gets nothing, not even the page.
    {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    }

    // A few wrong guesses are answered; then guessing has to wait.
    for _ in 0..3 {
        let (st, body) = post(port, Some(&me), json, &guess("wrong"));
        assert_eq!(st, 200);
        assert!(body.contains("Incorrect password"), "{body}");
    }
    let (st, body) = post(port, Some(&me), json, &guess("right-pw"));
    assert_eq!(st, 429, "{body}");
    assert!(body.contains("Try again in"), "{body}");
    assert!(rx.try_recv().is_err(), "a throttled guess must not unlock");

    // After the wait, the right password works.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (st, body) = post(port, Some(&me), json, &guess("right-pw"));
    assert_eq!(st, 200);
    assert!(body.contains("\"ok\":true"), "{body}");
    let got = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(got.as_deref(), Some("right-pw"));
    let _ = std::fs::remove_dir_all(&dir);
}
