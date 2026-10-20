//! Copying an app's own schema between files: template exports, public
//! exports and publishes with FTS5 tables and generated columns, and a
//! template whose sqlite_master was edited to smuggle in extra SQL.

use serde_json::json;
use uapp::engine::Engine;
use uapp::{store, template};

fn root(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("uapp-schema-{}", std::process::id()));
    // Keep the registry and snapshots out of the real per-user data dir. One
    // dir for the whole binary: the tests run in parallel and share the env.
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

fn open(path: std::path::PathBuf) -> Engine {
    Engine::open(path, "dev".into(), "me".into()).unwrap()
}

/// An app with an FTS5 table (external content, kept in sync by triggers)
/// and a table with stored and virtual generated columns.
fn rich_app(path: std::path::PathBuf) -> Engine {
    let mut eng = open(path);
    eng.local_op(
        "batch",
        json!({"sql": "
            CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);
            CREATE VIRTUAL TABLE notes_fts USING fts5(body, content='notes', content_rowid='id');
            CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
              INSERT INTO notes_fts(rowid, body) VALUES (new.id, new.body);
            END;
            CREATE VIRTUAL TABLE docs USING fts5(title, body);
            CREATE TABLE prices(net INTEGER, tax INTEGER GENERATED ALWAYS AS (net / 10) VIRTUAL,
                                gross INTEGER GENERATED ALWAYS AS (net + net / 10) STORED);
            CREATE TABLE plain(v TEXT);
            INSERT INTO notes(id, body) VALUES (5, 'buy milk'), (9, 'call bob');
            INSERT INTO docs(title, body) VALUES ('a', 'hello world'), ('b', 'other');
            INSERT INTO prices(net) VALUES (100), (250);
            INSERT INTO plain(v) VALUES ('x'), ('y');
            DELETE FROM plain WHERE v = 'x';"}),
    )
    .unwrap();
    eng
}

fn q(c: &rusqlite::Connection, sql: &str) -> String {
    c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0))
        .map(|v| format!("{v:?}"))
        .unwrap_or_else(|e| format!("error: {e}"))
}

fn conn_of(bytes: &[u8], dir: &std::path::Path, name: &str) -> rusqlite::Connection {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    rusqlite::Connection::open(p).unwrap()
}

#[test]
fn exports_handle_fts5_and_generated_columns() {
    let dir = root("exports");
    let eng = rich_app(dir.join("rich.uapp"));

    // A template: the schema, no rows, no FTS shadow tables of its own.
    let tpl = store::export_template(&eng.db, "rich").unwrap();
    let t = conn_of(&tpl, &dir, "tpl.uapp");
    assert_eq!(q(&t, "SELECT count(*) FROM prices"), "Integer(0)");
    assert_eq!(q(&t, "SELECT count(*) FROM docs"), "Integer(0)");
    assert_eq!(
        q(&t, "PRAGMA application_id"),
        format!("Integer({})", store::APPLICATION_ID)
    );

    // A public copy: rows included, search and generated columns work.
    let public = store::export_public(&eng.db, "rich", false).unwrap();
    let p = conn_of(&public, &dir, "public.uapp");
    assert_eq!(
        q(&p, "SELECT title FROM docs WHERE docs MATCH 'hello'"),
        "Text(\"a\")"
    );
    assert_eq!(
        q(
            &p,
            "SELECT rowid FROM notes_fts WHERE notes_fts MATCH 'bob'"
        ),
        "Integer(9)"
    );
    assert_eq!(q(&p, "SELECT sum(gross) FROM prices"), "Integer(385)");
    assert_eq!(q(&p, "SELECT sum(tax) FROM prices"), "Integer(35)");
    assert_eq!(
        q(&p, "SELECT rowid FROM plain WHERE v = 'y'"),
        "Integer(2)",
        "an implicit rowid is kept"
    );
    // The trigger came along, after the rows.
    p.execute_batch("INSERT INTO notes(id, body) VALUES (12, 'water plants')")
        .unwrap();
    assert_eq!(
        q(
            &p,
            "SELECT rowid FROM notes_fts WHERE notes_fts MATCH 'plants'"
        ),
        "Integer(12)"
    );
}

#[test]
fn template_update_and_publish_skip_shadow_tables() {
    let dir = root("update");
    let src = rich_app(dir.join("src.uapp"));
    let tpl = store::export_template(&src.db, "rich").unwrap();
    let public = store::export_public(&src.db, "rich", false).unwrap();

    // The template's plan over an empty app: only the app's own objects.
    let mut target = open(dir.join("target.uapp"));
    let source = template::Source::from_bytes(&tpl, None).unwrap();
    let plan = template::plan(&target.db, source.conn()).unwrap();
    let created: Vec<String> = plan["schema"]["createTables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["table"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(created, ["notes", "notes_fts", "docs", "prices", "plain"]);
    drop(source);
    let b64 = base64_encode(&tpl);
    let (r, _) = target
        .local_op("template_update", json!({"b64": b64}))
        .unwrap();
    assert_eq!(r["warnings"], json!([]), "{r}");
    target
        .local_op(
            "batch",
            json!({"sql": "INSERT INTO docs(title, body) VALUES ('t', 'find me')"}),
        )
        .unwrap();
    assert_eq!(
        q(&target.db, "SELECT title FROM docs WHERE docs MATCH 'find'"),
        "Text(\"t\")"
    );

    // Publishing the public copy over the target replaces its tables.
    let (r, _) = target
        .local_op("publish", json!({"b64": base64_encode(&public)}))
        .unwrap();
    assert_eq!(r["schema"]["tables"], json!(5), "{r}");
    assert_eq!(
        q(
            &target.db,
            "SELECT title FROM docs WHERE docs MATCH 'hello'"
        ),
        "Text(\"a\")"
    );
    assert_eq!(
        q(
            &target.db,
            "SELECT count(*) FROM docs WHERE docs MATCH 'find'"
        ),
        "Integer(0)"
    );
    assert_eq!(
        q(&target.db, "SELECT sum(gross) FROM prices"),
        "Integer(385)"
    );
}

/// A template whose sqlite_master was edited (writable_schema) so a table's
/// CREATE carries more statements after it.
#[test]
fn tampered_template_schema_is_not_run() {
    let dir = root("tampered");
    let outside = dir.join("outside.db");
    let tpl = {
        let eng = open(dir.join("tpl-src.uapp"));
        eng.db.execute_batch("CREATE TABLE notes(t TEXT)").unwrap();
        store::export_template(&eng.db, "tpl").unwrap()
    };
    let path = dir.join("tampered.uapp");
    std::fs::write(&path, &tpl).unwrap();
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(&format!(
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_master
               SET sql = sql || '; COMMIT; ATTACH ''{}'' AS x; CREATE TABLE x.pwned(y); BEGIN'
               WHERE name = 'notes';
             PRAGMA writable_schema = OFF;",
            outside.display()
        ))
        .unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();

    // Applied as a template update: refused as a warning, nothing else ran.
    let mut target = open(dir.join("target.uapp"));
    let (r, _) = target
        .local_op("template_update", json!({"b64": base64_encode(&bytes)}))
        .unwrap();
    let warnings = r["warnings"].to_string();
    assert!(warnings.contains("tampered"), "{warnings}");
    assert_eq!(r["applied"]["tablesCreated"], json!(0));
    assert!(!outside.exists(), "the smuggled ATTACH never ran");

    // Published, or rebuilt by a library: the whole thing fails.
    assert!(
        target
            .local_op("publish", json!({"b64": base64_encode(&bytes)}))
            .is_err()
    );
    let src =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    store::harden_foreign(&src).unwrap();
    let e = format!("{:#}", store::export_public(&src, "x", true).unwrap_err());
    assert!(e.contains("tampered"), "{e}");
    assert!(!outside.exists());
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}
