//! Integration test for `Engine::reencrypt` — the in-app Settings → Encryption
//! flow: encrypt a plaintext app, re-key it, then decrypt it, all on a live
//! engine, verifying data + post-change edits survive and the wrong password is
//! rejected at rest.

use serde_json::json;
use uapp::engine::Engine;
use uapp::store;

fn sum(eng: &Engine) -> i64 {
    eng.db
        .query_row("SELECT coalesce(sum(x),0) FROM t", [], |r| r.get(0))
        .unwrap_or(-1)
}
fn add(eng: &mut Engine, x: i64) {
    eng.local_op(
        "sql",
        json!({"sql": format!("INSERT INTO t VALUES ({x})"), "params": []}),
    )
    .unwrap();
}
fn open(path: &std::path::Path, pw: Option<&str>) -> anyhow::Result<Engine> {
    Engine::open_with_passphrase(
        path.into(),
        "devA".into(),
        "Tester".into(),
        pw.map(String::from),
    )
}

/// A fresh scratch dir. The registry and snapshots go to a temp data dir,
/// one for the whole binary (the tests share the env).
fn scratch(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("uapp-reenc-{}", std::process::id()));
    static DATA: std::sync::Once = std::sync::Once::new();
    // SAFETY: test-only. Other tests may already be running on their own
    // threads, so this is not strictly race-free, but it runs once, before
    // any of them reaches the data dir (they all come through here first).
    DATA.call_once(|| unsafe { std::env::set_var("XDG_DATA_HOME", base.join("data")) });
    let dir = base.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn encrypt_rekey_decrypt_roundtrip() {
    let dir = scratch("roundtrip");
    let path = dir.join("App.uapp");

    // Plaintext app with data.
    let mut eng = open(&path, None).unwrap();
    eng.local_op(
        "sql",
        json!({"sql": "CREATE TABLE t(x INTEGER)", "params": []}),
    )
    .unwrap();
    add(&mut eng, 1);
    assert_eq!(sum(&eng), 1);
    assert!(store::looks_like_sqlite(&path), "starts plaintext");

    // 1) Encrypt.
    eng.reencrypt(Some("pw1".into())).unwrap();
    assert!(eng.crypt.is_some(), "engine now holds a key");
    assert!(
        !store::looks_like_sqlite(&path),
        "file is encrypted at rest"
    );
    assert_eq!(sum(&eng), 1, "data survived encryption");
    add(&mut eng, 10); // edit under encryption
    assert_eq!(sum(&eng), 11);
    drop(eng);

    // At rest: no password and wrong password are rejected; pw1 works.
    assert!(
        open(&path, None).is_err(),
        "encrypted file won't open without a password"
    );
    assert!(
        open(&path, Some("nope")).is_err(),
        "wrong password rejected"
    );
    let mut eng = open(&path, Some("pw1")).unwrap();
    assert_eq!(sum(&eng), 11, "reopened with pw1");

    // 2) Re-key pw1 -> pw2.
    eng.reencrypt(Some("pw2".into())).unwrap();
    add(&mut eng, 100);
    assert_eq!(sum(&eng), 111);
    drop(eng);
    assert!(
        open(&path, Some("pw1")).is_err(),
        "old password no longer works"
    );
    let mut eng = open(&path, Some("pw2")).unwrap();
    assert_eq!(sum(&eng), 111, "reopened with pw2");

    // 3) Decrypt.
    eng.reencrypt(None).unwrap();
    assert!(eng.crypt.is_none(), "engine no longer holds a key");
    assert!(
        store::looks_like_sqlite(&path),
        "file is plaintext at rest again"
    );
    assert_eq!(sum(&eng), 111, "data survived decryption");
    add(&mut eng, 1000);
    drop(eng);

    // Reopens plaintext with everything intact.
    let eng = open(&path, None).unwrap();
    assert_eq!(sum(&eng), 1111, "all edits durable after full round-trip");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every file under `dir` (recursively) whose bytes contain `needle`.
fn files_containing(dir: &std::path::Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_containing(&p, needle));
        } else if std::fs::read(&p)
            .map(|b| b.windows(needle.len()).any(|w| w == needle))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
    out
}

#[test]
fn encrypting_leaves_no_plaintext_copy_behind() {
    let dir = scratch("leftovers");
    let path = dir.join("Secret.uapp");
    let marker = "the-plaintext-marker-7f3a";
    let mut eng = open(&path, None).unwrap();
    eng.local_op(
        "batch",
        json!({"sql": format!("CREATE TABLE s(v TEXT); INSERT INTO s VALUES ('{marker}')")}),
    )
    .unwrap();
    eng.snapshot().unwrap();
    // A damaged copy kept beside the file by an earlier restore.
    std::fs::copy(&path, dir.join("Secret.uapp.corrupt-1700000000000")).unwrap();
    let snapdir = uapp::registry::snapshot_dir(&path);
    assert!(!files_containing(&snapdir, marker.as_bytes()).is_empty());

    eng.reencrypt(Some("pw".into())).unwrap();
    let data = std::env::var("XDG_DATA_HOME").unwrap();
    for place in [dir.as_path(), std::path::Path::new(&data)] {
        let left = files_containing(place, marker.as_bytes());
        assert!(left.is_empty(), "plaintext left behind: {left:?}");
    }
    // There is still a rescue point, encrypted like the file.
    let snaps: Vec<_> = std::fs::read_dir(&snapdir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "uapp"))
        .collect();
    assert!(!snaps.is_empty());
    assert!(snaps.iter().all(|p| !store::looks_like_sqlite(p)));
    let v: String = eng
        .db
        .query_row("SELECT v FROM s", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, marker);
}
