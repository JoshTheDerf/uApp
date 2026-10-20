//! Rolling snapshots are kept per .uapp path: a copy of an app never gets
//! the other copy's snapshot restored over it, the app_id inside a file
//! can't steer where they are written, and opening an unchanged file doesn't
//! take a new one.

use serde_json::json;
use uapp::engine::Engine;
use uapp::registry;

fn root(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("uapp-snapshots-{}", std::process::id()));
    // Keep the registry and snapshots out of the real per-user data dir. One
    // dir for the whole binary: the tests run in parallel and share the env.
    static DATA: std::sync::Once = std::sync::Once::new();
    DATA.call_once(|| std::env::set_var("XDG_DATA_HOME", base.join("data")));
    let dir = base.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open(path: &std::path::Path) -> anyhow::Result<Engine> {
    Engine::open(path.to_path_buf(), "dev".into(), "me".into())
}

fn note(eng: &mut Engine, text: &str) {
    eng.local_op(
        "batch",
        json!({"sql": format!("CREATE TABLE IF NOT EXISTS notes(t TEXT); DELETE FROM notes; INSERT INTO notes VALUES ('{text}')")}),
    )
    .unwrap();
    eng.snapshot_if_dirty();
}

fn read_note(eng: &Engine) -> String {
    eng.db
        .query_row("SELECT t FROM notes", [], |r| r.get(0))
        .unwrap()
}

/// Overwrite the SQLite header: the file no longer opens.
fn damage(path: &std::path::Path) {
    let mut b = std::fs::read(path).unwrap();
    b[..100].fill(0x5a);
    std::fs::write(path, b).unwrap();
}

fn snapshots(path: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(registry::snapshot_dir(path))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.ends_with(".uapp"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn a_copy_never_restores_the_other_copys_snapshot() {
    let dir = root("copies");
    let a = dir.join("a.uapp");
    let b = dir.join("b.uapp");
    note(&mut open(&a).unwrap(), "from A");
    std::fs::copy(&a, &b).unwrap(); // Save As keeps the app_id; so does a copy
    note(&mut open(&b).unwrap(), "from B");

    damage(&a);
    let eng = open(&a).unwrap();
    assert_eq!(read_note(&eng), "from A");
    drop(eng);
    let eng = open(&b).unwrap();
    assert_eq!(read_note(&eng), "from B");
}

#[test]
fn a_snapshot_dir_for_another_path_is_not_used() {
    let dir = root("source");
    let a = dir.join("a.uapp");
    note(&mut open(&a).unwrap(), "from A");
    std::fs::write(
        registry::snapshot_dir(&a).join("source"),
        "/somewhere/else.uapp",
    )
    .unwrap();
    damage(&a);
    assert!(open(&a).is_err(), "no restore from a dir for another file");
}

#[test]
fn snapshots_from_older_builds_still_restore() {
    let dir = root("legacy");
    let a = dir.join("a.uapp");
    let app_id = {
        let mut eng = open(&a).unwrap();
        note(&mut eng, "from A");
        eng.app_id.clone()
    };
    // What an older build left: snapshots under <app_id>/snapshots only.
    let legacy = registry::local_root().join(&app_id).join("snapshots");
    std::fs::create_dir_all(&legacy).unwrap();
    let newest = snapshots(&a).pop().unwrap();
    std::fs::copy(
        registry::snapshot_dir(&a).join(&newest),
        legacy.join(&newest),
    )
    .unwrap();
    std::fs::remove_dir_all(registry::snapshot_dir(&a)).unwrap();

    damage(&a);
    assert_eq!(read_note(&open(&a).unwrap()), "from A");
}

#[test]
fn a_crafted_app_id_is_ignored() {
    let dir = root("appid");
    let a = dir.join("a.uapp");
    drop(open(&a).unwrap());
    {
        let c = rusqlite::Connection::open(&a).unwrap();
        c.execute(
            "UPDATE uapp_meta SET value = '../../escape' WHERE key = 'app_id'",
            [],
        )
        .unwrap();
    }
    let eng = open(&a).unwrap();
    assert_eq!(eng.app_id, "unknown");
    drop(eng);
    let outside = registry::local_root().join("../../escape");
    assert!(!outside.exists(), "nothing created outside the data dir");
    assert!(registry::valid_app_id("Ab3_x-9"));
    assert!(!registry::valid_app_id("../x") && !registry::valid_app_id(""));
}

#[test]
fn unchanged_opens_take_no_new_snapshot() {
    let dir = root("unchanged");
    let a = dir.join("a.uapp");
    note(&mut open(&a).unwrap(), "one");
    let before = snapshots(&a);
    for _ in 0..5 {
        drop(open(&a).unwrap());
    }
    assert_eq!(snapshots(&a), before, "five unchanged opens");

    // A leftover temp file from a crash goes with the next snapshot.
    let stale = registry::snapshot_dir(&a).join("snapshot-123.tmp");
    std::fs::write(&stale, b"half").unwrap();
    // Changed outside uApp (another tool), then opened: a new snapshot.
    {
        let c = rusqlite::Connection::open(&a).unwrap();
        c.execute("UPDATE notes SET t = 'two'", []).unwrap();
    }
    let eng = open(&a).unwrap();
    assert_eq!(read_note(&eng), "two");
    assert_eq!(snapshots(&a).len(), before.len() + 1);
    assert!(!stale.exists(), "stale temp file removed");
}
