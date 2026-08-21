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

#[test]
fn save_as_rehomes_and_persists() {
    let dir = std::env::temp_dir().join(format!("uapp-saveas-{}", fastrand::u32(..)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = dir.join("Untitled.uapp");
    let saved = dir.join("Budget.uapp");

    // A scratch app with some data.
    let mut eng = Engine::open(scratch.clone(), "devA".into(), "Tester".into()).unwrap();
    eng.local_op(
        "sql",
        json!({"sql": "CREATE TABLE t(x INTEGER)", "params": []}),
    )
    .unwrap();
    eng.local_op("sql", json!({"sql": "INSERT INTO t VALUES (1)", "params": []}))
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
    assert_eq!(q_i64(&eng, "SELECT sum(x) FROM t"), 1, "data survived the re-home");

    // A post-save edit must land in the NEW file's log/checkpoint.
    eng.local_op("sql", json!({"sql": "INSERT INTO t VALUES (40)", "params": []}))
        .unwrap();
    assert_eq!(q_i64(&eng, "SELECT sum(x) FROM t"), 41, "edit after save applies");
    drop(eng);

    // Reopen the saved file fresh: every op (pre- and post-save) is durable.
    let eng2 = Engine::open(saved.clone(), "devA".into(), "Tester".into()).unwrap();
    assert_eq!(
        q_i64(&eng2, "SELECT sum(x) FROM t"),
        41,
        "both edits durable at the new location after reopen"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
