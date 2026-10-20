//! Opening files the engine must not touch: one from a newer uApp, and one
//! that fails its integrity check. Neither may be "rescued" from a snapshot
//! or migrated.

use uapp::engine::Engine;

fn setup(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("uapp-format-{}", std::process::id()));
    // Keep the registry and snapshots out of the real per-user data dir. One
    // dir for the whole binary: the tests run in parallel and share the env.
    static DATA: std::sync::Once = std::sync::Once::new();
    DATA.call_once(|| std::env::set_var("XDG_DATA_HOME", base.join("data")));
    let root = base.join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn open(path: &std::path::Path) -> anyhow::Result<Engine> {
    Engine::open(path.to_path_buf(), "dev".into(), "me".into())
}

#[test]
fn newer_format_is_refused_not_restored() {
    let root = setup("newer");
    let path = root.join("app.uapp");
    {
        let eng = open(&path).unwrap();
        eng.db
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
    }
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "user_version", uapp::store::FORMAT_VERSION + 1)
            .unwrap();
    }
    let err = format!("{:#}", open(&path).err().expect("refused"));
    assert!(err.contains("newer uApp"), "{err}");
    // The file is left exactly as the newer uApp wrote it.
    let c = rusqlite::Connection::open(&path).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, uapp::store::FORMAT_VERSION + 1);
    let aside = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
    assert!(!aside, "no snapshot restore for a refused format");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn integrity_is_checked_before_anything_is_written() {
    let root = setup("integrity");
    let path = root.join("damaged.uapp");
    {
        // A file with many pages and no snapshot history, so nothing can be
        // restored and the open has to fail.
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE big(x TEXT);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 2000)
             INSERT INTO big SELECT printf('%0200d', i) FROM n;",
        )
        .unwrap();
    }
    // Scribble over a page in the middle of the table.
    let mut bytes = std::fs::read(&path).unwrap();
    let page = 4096;
    let mid = (bytes.len() / page / 2) * page;
    for b in &mut bytes[mid..mid + page] {
        *b = 0xa5;
    }
    std::fs::write(&path, &bytes).unwrap();
    let err = format!("{:#}", open(&path).err().expect("refused"));
    assert!(err.contains("integrity"), "{err}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "nothing was written to the damaged file"
    );
    let _ = std::fs::remove_dir_all(&root);
}
