//! Integration test for `Engine::reencrypt` — the in-app Settings → Encryption
//! flow: encrypt a plaintext app, re-key it, then decrypt it, all on a live
//! engine, verifying data + post-change edits survive and the wrong password is
//! rejected at rest.

use serde_json::json;
use uapp::engine::Engine;
use uapp::store;

fn sum(eng: &Engine) -> i64 {
    eng.db.query_row("SELECT coalesce(sum(x),0) FROM t", [], |r| r.get(0)).unwrap_or(-1)
}
fn add(eng: &mut Engine, x: i64) {
    eng.local_op("sql", json!({"sql": format!("INSERT INTO t VALUES ({x})"), "params": []}))
        .unwrap();
}
fn open(path: &std::path::Path, pw: Option<&str>) -> anyhow::Result<Engine> {
    Engine::open_with_passphrase(path.into(), "devA".into(), "Tester".into(), pw.map(String::from))
}

#[test]
fn encrypt_rekey_decrypt_roundtrip() {
    let dir = std::env::temp_dir().join(format!("uapp-reenc-{}", fastrand::u32(..)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("App.uapp");

    // Plaintext app with data.
    let mut eng = open(&path, None).unwrap();
    eng.local_op("sql", json!({"sql": "CREATE TABLE t(x INTEGER)", "params": []})).unwrap();
    add(&mut eng, 1);
    assert_eq!(sum(&eng), 1);
    assert!(store::looks_like_sqlite(&path), "starts plaintext");

    // 1) Encrypt.
    eng.reencrypt(Some("pw1".into())).unwrap();
    assert!(eng.crypt.is_some(), "engine now holds a key");
    assert!(!store::looks_like_sqlite(&path), "file is encrypted at rest");
    assert_eq!(sum(&eng), 1, "data survived encryption");
    add(&mut eng, 10); // edit under encryption
    assert_eq!(sum(&eng), 11);
    drop(eng);

    // At rest: no password and wrong password are rejected; pw1 works.
    assert!(open(&path, None).is_err(), "encrypted file won't open without a password");
    assert!(open(&path, Some("nope")).is_err(), "wrong password rejected");
    let mut eng = open(&path, Some("pw1")).unwrap();
    assert_eq!(sum(&eng), 11, "reopened with pw1");

    // 2) Re-key pw1 -> pw2.
    eng.reencrypt(Some("pw2".into())).unwrap();
    add(&mut eng, 100);
    assert_eq!(sum(&eng), 111);
    drop(eng);
    assert!(open(&path, Some("pw1")).is_err(), "old password no longer works");
    let mut eng = open(&path, Some("pw2")).unwrap();
    assert_eq!(sum(&eng), 111, "reopened with pw2");

    // 3) Decrypt.
    eng.reencrypt(None).unwrap();
    assert!(eng.crypt.is_none(), "engine no longer holds a key");
    assert!(store::looks_like_sqlite(&path), "file is plaintext at rest again");
    assert_eq!(sum(&eng), 111, "data survived decryption");
    add(&mut eng, 1000);
    drop(eng);

    // Reopens plaintext with everything intact.
    let eng = open(&path, None).unwrap();
    assert_eq!(sum(&eng), 1111, "all edits durable after full round-trip");

    let _ = std::fs::remove_dir_all(&dir);
}
