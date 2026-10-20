//! The live connection uses a rollback journal on disk: a commit cut short
//! (crash, process kill) leaves `<file>-journal`, and the next open rolls the
//! half-written transaction back instead of reading a torn file.

use serde_json::json;
use uapp::engine::Engine;
use uapp::store;

fn changed_rows(path: &std::path::Path) -> rusqlite::Result<i64> {
    rusqlite::Connection::open(path)?.query_row(
        "SELECT count(*) FROM t WHERE x = 'changed'",
        [],
        |r| r.get(0),
    )
}

#[test]
fn hot_journal_rolls_back_on_open() {
    let root = std::env::temp_dir().join(format!("uapp-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Keep the registry and snapshots out of the real per-user data dir.
    // SAFETY: runs first thing in the test, before it starts any threads.
    unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
    let path = root.join("app.uapp");
    let journal = store::journal_path(&path);
    assert!(journal.to_string_lossy().ends_with("app.uapp-journal"));

    let mut eng = Engine::open(path.clone(), "dev".into(), "me".into()).unwrap();
    eng.local_op(
        "batch",
        json!({"sql": "CREATE TABLE t(x TEXT);
            WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 3000)
            INSERT INTO t SELECT printf('%0300d', i) FROM n;"}),
    )
    .unwrap();
    assert!(!journal.exists(), "no journal between writes");

    // Freeze the files in the middle of a big transaction: a tiny page cache
    // makes SQLite write changed pages into the file before the commit, with
    // the originals saved in the journal.
    eng.db.execute_batch("PRAGMA cache_size = 2").unwrap();
    eng.db.execute_batch("BEGIN IMMEDIATE").unwrap();
    eng.db.execute_batch("UPDATE t SET x = 'changed'").unwrap();
    assert!(journal.exists(), "the journal is on disk mid-transaction");
    let crashed = root.join("crashed.uapp");
    std::fs::copy(&path, &crashed).unwrap();
    std::fs::copy(&journal, store::journal_path(&crashed)).unwrap();
    eng.db.execute_batch("ROLLBACK").unwrap();
    drop(eng);

    // Without its journal the frozen file is half-written: what
    // journal_mode=MEMORY used to leave behind after a crash.
    let torn = root.join("torn.uapp");
    std::fs::copy(&crashed, &torn).unwrap();
    assert!(
        !matches!(changed_rows(&torn), Ok(0)),
        "the copy without its journal holds part of the transaction"
    );

    // With it, opening rolls the transaction back.
    let eng = Engine::open(crashed.clone(), "dev".into(), "me".into()).unwrap();
    let (changed, total): (i64, i64) = eng
        .db
        .query_row(
            "SELECT count(*) FILTER (WHERE x = 'changed'), count(*) FROM t",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((changed, total), (0, 3000));
    assert!(!store::journal_path(&crashed).exists(), "journal consumed");
    drop(eng);
    let _ = std::fs::remove_dir_all(&root);
}
