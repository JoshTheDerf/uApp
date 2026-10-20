//! Integration test for `Engine::save_as` — the native window's "Save As"
//! re-home: write current state to a new file, switch the live engine onto it,
//! and keep working there. Verifies the destination is self-contained (no
//! sibling files), the engine follows, and post-save edits land in (and
//! survive at) the new file.

use serde_json::json;
use uapp::engine::Engine;

fn q_i64(eng: &Engine, sql: &str) -> i64 {
    eng.db.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

/// A fresh scratch dir. The registry and snapshots go to a temp data dir,
/// one for the whole binary (the tests share the env).
fn scratch(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("uapp-saveas-{}", std::process::id()));
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

fn app_with_data(path: &std::path::Path) -> Engine {
    let mut eng = Engine::open(path.to_path_buf(), "devA".into(), "Tester".into()).unwrap();
    eng.local_op(
        "batch",
        json!({"sql": "CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1)"}),
    )
    .unwrap();
    eng
}

#[test]
fn save_as_rehomes_and_persists() {
    let dir = scratch("rehome");
    let scratch = dir.join("Untitled.uapp");
    let saved = dir.join("Budget.uapp");

    // A scratch app with some data.
    let mut eng = Engine::open(scratch.clone(), "devA".into(), "Tester".into()).unwrap();
    eng.local_op(
        "sql",
        json!({"sql": "CREATE TABLE t(x INTEGER)", "params": []}),
    )
    .unwrap();
    eng.local_op(
        "sql",
        json!({"sql": "INSERT INTO t VALUES (1)", "params": []}),
    )
    .unwrap();
    assert_eq!(q_i64(&eng, "SELECT sum(x) FROM t"), 1, "data before save");

    // Save As → new file. The engine must re-home onto it.
    eng.save_as(saved.clone()).unwrap();
    assert_eq!(eng.path, saved, "engine path follows the saved file");
    assert!(saved.exists(), "the saved file was written");
    assert!(
        !saved.with_extension("uapp.sync").exists(),
        "no sidecar appears next to the saved file"
    );
    assert_eq!(
        q_i64(&eng, "SELECT sum(x) FROM t"),
        1,
        "data survived the re-home"
    );

    // A post-save edit must land in the NEW file.
    eng.local_op(
        "sql",
        json!({"sql": "INSERT INTO t VALUES (40)", "params": []}),
    )
    .unwrap();
    assert_eq!(
        q_i64(&eng, "SELECT sum(x) FROM t"),
        41,
        "edit after save applies"
    );
    drop(eng);

    // Reopen the saved file fresh: every write (pre- and post-save) is durable.
    let eng2 = Engine::open(saved.clone(), "devA".into(), "Tester".into()).unwrap();
    assert_eq!(
        q_i64(&eng2, "SELECT sum(x) FROM t"),
        41,
        "both edits durable at the new location after reopen"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_as_over_a_file_open_elsewhere_is_refused() {
    let dir = scratch("busy");
    let a = dir.join("A.uapp");
    let b = dir.join("B.uapp");
    let mut eng = app_with_data(&a);
    let other = Engine::open(b.clone(), "devB".into(), "Tester".into()).unwrap();
    let before = std::fs::read(&b).unwrap();
    let err = format!("{:#}", eng.save_as(b.clone()).unwrap_err());
    assert!(err.contains("already open"), "{err}");
    assert_eq!(std::fs::read(&b).unwrap(), before, "B untouched");
    drop(other);
    // The engine is still on A and still works.
    assert_eq!(eng.path, a);
    eng.local_op(
        "sql",
        json!({"sql": "INSERT INTO t VALUES (2)", "params": []}),
    )
    .unwrap();
    assert_eq!(q_i64(&eng, "SELECT sum(x) FROM t"), 3);
}

#[test]
fn a_failed_save_as_keeps_the_original_engine() {
    let dir = scratch("fails");
    let a = dir.join("A.uapp");
    let mut eng = app_with_data(&a);
    // A non-empty directory where the file should go: the rename fails.
    let target = dir.join("Taken.uapp");
    std::fs::create_dir_all(target.join("inside")).unwrap();
    assert!(eng.save_as(target.clone()).is_err());
    assert_eq!(eng.path, a);
    assert_eq!(
        q_i64(&eng, "SELECT sum(x) FROM t"),
        1,
        "still the real file"
    );
    // ... so the next snapshot holds the app, not an empty database.
    eng.snapshot().unwrap();
    let newest = std::fs::read_dir(uapp::registry::snapshot_dir(&a))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "uapp"))
        .max()
        .unwrap();
    let snap = rusqlite::Connection::open(newest).unwrap();
    let x: i64 = snap
        .query_row("SELECT sum(x) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(x, 1);
    // Leftover temp files are cleaned up.
    let stray: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(stray.is_empty());
    // Saving onto its own path is a no-op.
    eng.save_as(a.clone()).unwrap();
    assert_eq!(q_i64(&eng, "SELECT sum(x) FROM t"), 1);
}
