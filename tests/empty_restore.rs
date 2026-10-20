// A .uapp we have served before that turns up as zero bytes (an interrupted
// copy or provider write-back) must come back from its newest snapshot, not
// open as a blank app.
use uapp::engine::Engine;

#[test]
fn zero_byte_known_file_restores_snapshot() {
    let root = std::env::temp_dir().join(format!("uapp-empty-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Keep the registry and snapshots out of the real per-user data dir.
    // SAFETY: runs first thing in the test, before it starts any threads.
    unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
    let path = root.join("notes.uapp");

    {
        let mut eng = Engine::open(path.clone(), "dev".into(), "me".into()).unwrap();
        eng.db
            .execute_batch("CREATE TABLE notes(t TEXT); INSERT INTO notes VALUES ('keep me');")
            .unwrap();
        eng.snapshot().unwrap();
    }

    std::fs::File::create(&path).unwrap(); // truncate to zero bytes
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

    let eng = Engine::open(path.clone(), "dev".into(), "me".into()).unwrap();
    let t: String = eng
        .db
        .query_row("SELECT t FROM notes", [], |r| r.get(0))
        .expect("the snapshot's table is back");
    assert_eq!(t, "keep me");
    // The empty file was kept aside rather than deleted.
    let aside = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
    assert!(aside, "the emptied file is kept next to the restored one");
    drop(eng);
    let _ = std::fs::remove_dir_all(&root);
}
