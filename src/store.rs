//! The .uapp file format (SQLite, sqlar layout): schema, migrations, the
//! write-operation applier, queries, the sqlar file store, and full/template
//! exports. The engine holds one live connection on the file; everything here
//! takes `&Connection`.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::Connection;
use serde_json::{json, Value};
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

/// Milliseconds since the unix epoch. (SystemTime panics on
/// wasm32-unknown-unknown, so the browser build asks JS for Date.now().)
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> u64 {
    crate::wasm::now_ms()
}

/// One write operation: what happened, by whom, when. Executed exactly once by
/// `apply_op` inside the engine's transaction; `dev`/`user`/`ts` feed the
/// attribution columns (chat, file history).
pub struct Op {
    pub dev: String,
    pub ts: u64,
    pub user: String,
    pub kind: String,
    pub payload: Value,
}

// ---- the file format ---------------------------------------------------------
//
// A .uapp is a SQLite database (optionally SQLCipher-encrypted) holding:
//
//   sqlar              the app's files (app/ code, data/ user content), in the
//                      standard sqlar layout (zlib-compressed when smaller)
//   uapp_meta          app_id, name, created, format_version, template info
//   uapp_config        settings (AI provider, toolbar, MCP servers, ...)
//   uapp_chat          assistant chat messages, per session
//   uapp_sessions      chat sessions and agent conversations
//   uapp_file_history  earlier versions of files, for revert
//   anything else      the app's own tables
//
// The SQLite header says which format the file is in:
//
//   application_id = APPLICATION_ID ("uApp" in ASCII), so other tools can
//                    tell a .uapp from any other SQLite file;
//   user_version   = FORMAT_VERSION, the layout above.
//
// `format_version` in uapp_meta repeats the number for readers that only look
// there. A file with a higher number than FORMAT_VERSION was made by a newer
// uApp and is refused rather than half-understood. Files from before the
// header was stamped (application_id 0) are read as format 0 and upgraded on
// open. A layout change bumps FORMAT_VERSION and adds a step to MIGRATIONS.
//
// Format history:
//   0  unstamped files from before format versioning (any earlier layout)
//   1  app/ + data/ archive roots, uapp_chat.session, uapp_file_history.sz,
//      no uapp_applied table

pub const FORMAT_VERSION: i64 = 1;
/// The SQLite header's application_id for a .uapp: "uApp" in ASCII.
pub const APPLICATION_ID: i32 = 0x7541_7070;

pub const DEFAULT_INDEX_HTML: &str = include_str!("shell/default_app.html");
/// Seeded into new apps so agents working on the .uapp from OUTSIDE the
/// built-in assistant (e.g. Claude Code editing the archive) know the layout,
/// the uapp.js API, and the determinism rules.
pub const DEFAULT_AGENTS_MD: &str = include_str!("shell/AGENTS.md");

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sqlar(
  name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB
);
CREATE TABLE IF NOT EXISTS uapp_meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS uapp_config(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS uapp_chat(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  mid TEXT UNIQUE, ts INTEGER, ctr INTEGER, device TEXT, user TEXT,
  role TEXT, content TEXT, session TEXT
);
CREATE TABLE IF NOT EXISTS uapp_sessions(
  id TEXT PRIMARY KEY, title TEXT, created INTEGER,
  kind TEXT DEFAULT 'chat',      -- 'chat' | 'agent'
  parent TEXT,                   -- parent session id for agents
  archived INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS uapp_file_history(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT, ts INTEGER, device TEXT, user TEXT, action TEXT, data BLOB,
  sz INT
);
"#;

/// Upgrade steps, in order: `MIGRATIONS[n]` takes a file from format n to
/// n + 1. They run inside the bootstrap transaction, so a file is upgraded
/// completely or not at all.
const MIGRATIONS: &[fn(&Connection) -> Result<()>] = &[to_v1];

/// 0 → 1. Format 0 covers every file from before the header was stamped, so
/// each change here checks before it acts.
fn to_v1(conn: &Connection) -> Result<()> {
    // sz on uapp_file_history (added when history snapshots became
    // sqlar-compressed). Old rows keep sz NULL = uncompressed.
    if !has_column(conn, "uapp_file_history", "sz")? {
        conn.execute_batch("ALTER TABLE uapp_file_history ADD COLUMN sz INT")?;
    }
    // session on uapp_chat (multi-chat). NULL == 'main' everywhere.
    if !has_column(conn, "uapp_chat", "session")? {
        conn.execute_batch("ALTER TABLE uapp_chat ADD COLUMN session TEXT")?;
    }
    // The old sync engine's per-device watermark table, gone with it.
    conn.execute_batch("DROP TABLE IF EXISTS uapp_applied")?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// The format a file is in, read from its header (0 for an unstamped file),
/// or an error when it isn't one this build can open: another program's
/// SQLite file, or a .uapp from a newer uApp.
pub fn check_format(conn: &Connection) -> Result<i64> {
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let has_meta: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='uapp_meta'",
        [],
        |r| r.get(0),
    )?;
    let meta_version = if has_meta > 0 {
        meta_get(conn, "format_version")?
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let version = match app_id {
        APPLICATION_ID => user_version.max(meta_version),
        // Unstamped: user_version, if set at all, was set by the app itself
        // back when app SQL could, so only uapp_meta speaks for the format.
        0 => meta_version,
        other => bail!(
            "this is a SQLite database belonging to another program (application id \
             {other:#x}), not a .uapp"
        ),
    };
    if version > FORMAT_VERSION {
        bail!(
            "this file was made by a newer uApp (file format {version}; this version reads up \
             to {FORMAT_VERSION}). Update uApp to open it."
        );
    }
    Ok(if app_id == APPLICATION_ID {
        user_version
    } else {
        0
    })
}

/// Open a .uapp file read-write (creating it when absent, encrypted when a
/// master password is given), check it, and bring it up to date: the one way
/// the live connection is made. Rollback-journal mode with full syncs (see
/// `engine` for the durability contract). Integrity is checked before
/// anything is written, so a damaged file is never migrated.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_file(path: &Path, app_name: &str, key: Option<&str>) -> Result<Connection> {
    // The WHOLE body is retried on a transient sharing/lock violation: on
    // Windows, creating/writing the file trips real-time AV (Defender), which
    // briefly locks it — and SQLite's busy_timeout does NOT cover an OS-level
    // lock (it's not SQLite's own locking protocol), so a mid-write statement
    // can fail with ERROR_LOCK_VIOLATION. Bootstrap is one transaction, so
    // re-running the whole thing is safe.
    with_open_retry(|| {
        let conn = match key {
            Some(p) => crate::cipher::open_file(path, p)?,
            None => Connection::open(path)?,
        };
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if verdict != "ok" {
            bail!("{} failed its integrity check: {verdict}", path.display());
        }
        bootstrap_conn(&conn, app_name)?;
        Ok(conn)
    })
    .with_context(|| format!("opening {}", path.display()))
}

/// Create/complete the schema in a possibly-empty .uapp file. Handles the
/// "file manager created a 0-byte file from a template" case. With a master
/// password the file is (or becomes) SQLCipher-encrypted.
#[cfg(not(target_arch = "wasm32"))]
pub fn bootstrap(path: &Path, app_name: &str, key: Option<&str>) -> Result<()> {
    open_file(path, app_name, key).map(drop)
}

/// The platform-independent part of opening a .uapp, on an already-open (and
/// integrity-checked) connection: refuse a file this build can't read, then
/// in one transaction create what's missing, run the migrations the file
/// needs, seed a brand-new app and stamp the header. Idempotent.
pub fn bootstrap_conn(conn: &Connection, app_name: &str) -> Result<()> {
    let from = check_format(conn)?;
    // Deferred, so opening an up-to-date file takes no write lock and writes
    // nothing.
    conn.execute_batch("BEGIN")?;
    match bootstrap_tx(conn, app_name, from) {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    // Durable before anyone opens it.
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn bootstrap_tx(conn: &Connection, app_name: &str, from: i64) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    for (n, step) in MIGRATIONS.iter().enumerate().skip(from as usize) {
        step(conn).with_context(|| format!("upgrading the file to format {}", n + 1))?;
    }
    // Not a versioned step: names written into the archive by something
    // other than uApp (a person or tool editing the file directly) are
    // folded back into app/ and data/ on every open. Cheap when there is
    // nothing to do.
    migrate_paths(conn).context("moving archive paths into app/ and data/")?;
    let has_meta: i64 = conn.query_row(
        "SELECT count(*) FROM uapp_meta WHERE key='app_id'",
        [],
        |r| r.get(0),
    )?;
    if has_meta == 0 {
        let app_id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        conn.execute(
            "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES
             ('app_id',?1),('name',?2),('created',?3)",
            rusqlite::params![app_id, app_name, now_ms().to_string()],
        )?;
    }
    // migrate_paths() above has already moved any pre-split `index.html` here.
    let has_index: i64 = conn.query_row(
        "SELECT count(*) FROM sqlar WHERE name='app/index.html'",
        [],
        |r| r.get(0),
    )?;
    if has_index == 0 {
        sqlar_write(
            conn,
            "app/index.html",
            (now_ms() / 1000) as i64,
            DEFAULT_INDEX_HTML.as_bytes(),
        )?;
        // A brand-new app also gets the agent handbook (only then — an
        // app that deleted or replaced it keeps its own state).
        sqlar_write(
            conn,
            "app/AGENTS.md",
            (now_ms() / 1000) as i64,
            DEFAULT_AGENTS_MD.as_bytes(),
        )?;
    }
    stamp_format(conn)
}

/// Write the current format into the header and uapp_meta, touching only
/// what differs (so an up-to-date file isn't written to on every open).
fn stamp_format(conn: &Connection) -> Result<()> {
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if app_id != APPLICATION_ID {
        if app_id == 0 && user_version != 0 {
            // The app used user_version for itself. It belongs to uApp from
            // here on, so keep the app's number where it can still be found.
            conn.execute(
                "INSERT OR IGNORE INTO uapp_meta(key,value) VALUES('app_user_version',?1)",
                rusqlite::params![user_version.to_string()],
            )?;
        }
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    }
    if user_version != FORMAT_VERSION || app_id != APPLICATION_ID {
        conn.pragma_update(None, "user_version", FORMAT_VERSION)?;
    }
    if meta_get(conn, "format_version")?.as_deref() != Some(&FORMAT_VERSION.to_string()) {
        conn.execute(
            "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES('format_version',?1)",
            rusqlite::params![FORMAT_VERSION.to_string()],
        )?;
    }
    Ok(())
}

/// The rollback journal SQLite keeps next to a database while a write is
/// being committed (`<file>-journal`). After a crash it is "hot": the next
/// open uses it to undo the half-written transaction. So it belongs to that
/// exact file. Whatever moves a .uapp aside must move the journal with it,
/// and whatever puts different bytes at a path must first remove a journal
/// left there, or SQLite would "roll back" the new file with the old one's
/// pages.
#[cfg(not(target_arch = "wasm32"))]
pub fn journal_path(path: &Path) -> std::path::PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push("-journal");
    p.into()
}

/// The archive has exactly two top-level directories, and a file's location
/// IS its role — there is no separate tagging.
pub const APP_DIR: &str = "app/";
pub const DATA_DIR: &str = "data/";

/// Fold any path into its canonical home. Everything the app itself is made of
/// lives under `app/`; everything the user put in lives under `data/`. Names
/// from before the split (and root-absolute refs written by apps or models,
/// `index.html`, `/js/app.js`, `uploads/sales.csv`) are mapped in, so this is
/// the one place that decides where bytes land.
pub fn canon_name(name: &str) -> String {
    let n = name.trim().trim_start_matches('/');
    if n.starts_with(APP_DIR) || n.starts_with(DATA_DIR) {
        return n.to_string();
    }
    match n.strip_prefix("uploads/") {
        Some(rest) => format!("{DATA_DIR}{rest}"),
        None => format!("{APP_DIR}{n}"),
    }
}

/// Drop a leading `app/`, `data/` or (legacy) `uploads/` — the path relative
/// to whichever root it sits in.
pub fn strip_root(name: &str) -> &str {
    let n = name.trim().trim_start_matches('/');
    for root in [APP_DIR, DATA_DIR, "uploads/"] {
        if let Some(rest) = n.strip_prefix(root) {
            return rest;
        }
    }
    n
}

/// Which role a file plays for template export: files under `app/` ship in
/// the template, files under `data/` (user content) do not.
pub fn file_role(name: &str) -> &'static str {
    if name.starts_with(DATA_DIR) {
        "data"
    } else {
        "app"
    }
}

/// Names a reference could mean, most-specific first: the literal name (always
/// right for a name that came out of `files.list`), then its canonical home,
/// then the same relative path under either root. Lets a legacy or
/// root-absolute reference (`/js/app.js`, `uploads/sales.csv`) still find its
/// file after the split.
pub fn name_candidates(name: &str) -> Vec<String> {
    let n = name.trim().trim_start_matches('/').to_string();
    let mut out = vec![n.clone()];
    let bare = strip_root(&n);
    for cand in [
        canon_name(&n),
        format!("{APP_DIR}{bare}"),
        format!("{DATA_DIR}{bare}"),
    ] {
        if !out.contains(&cand) {
            out.push(cand);
        }
    }
    out
}

/// The archive name a reference resolves to, if the file exists.
pub fn resolve_name(conn: &Connection, name: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT 1 FROM sqlar WHERE name=?1")?;
    for cand in name_candidates(name) {
        if stmt.exists(rusqlite::params![cand])? {
            return Ok(Some(cand));
        }
    }
    Ok(None)
}

/// Move archive names from before the app/data split (or written by a tool
/// that doesn't know the layout) to their canonical home, honouring (and then
/// dropping) the old `file_roles` overrides so a seed dataset someone had
/// marked "app" stays app code. Idempotent; runs on every open.
fn migrate_paths(conn: &Connection) -> Result<()> {
    let roles = config_get(conn, "file_roles")?;
    let stray: i64 = conn.query_row(
        "SELECT count(*) FROM sqlar WHERE name NOT LIKE 'app/%' AND name NOT LIKE 'data/%'",
        [],
        |r| r.get(0),
    )?;
    let hstray: i64 = conn.query_row(
        "SELECT count(*) FROM uapp_file_history
         WHERE name NOT LIKE 'app/%' AND name NOT LIKE 'data/%'",
        [],
        |r| r.get(0),
    )?;
    if stray == 0 && hstray == 0 && roles.is_none() {
        return Ok(());
    }
    let roles = roles.unwrap_or(Value::Null);
    // Explicit role first, else the name's own convention.
    let target = |name: &str| -> String {
        match roles[name].as_str() {
            Some("app") => format!("{APP_DIR}{}", strip_root(name)),
            Some("data") => format!("{DATA_DIR}{}", strip_root(name)),
            _ => canon_name(name),
        }
    };
    // Sorted so a collision (both `index.html` and `app/index.html` present)
    // resolves the same way everywhere.
    let names: Vec<String> = {
        let mut stmt = conn.prepare("SELECT name FROM sqlar ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let hnames: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT name FROM uapp_file_history ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in names
        .iter()
        .chain(hnames.iter().filter(|h| !names.contains(h)))
    {
        let new = target(old);
        if &new == old {
            continue;
        }
        // OR REPLACE: if the destination somehow exists, the canonical row wins.
        conn.execute(
            "UPDATE OR REPLACE sqlar SET name=?2 WHERE name=?1",
            rusqlite::params![old, new],
        )?;
        conn.execute(
            "UPDATE uapp_file_history SET name=?2 WHERE name=?1",
            rusqlite::params![old, new],
        )?;
    }
    // Roles are the directory now; the override map has no meaning left.
    if !roles.is_null() {
        conn.execute("DELETE FROM uapp_config WHERE key='file_roles'", [])?;
    }
    Ok(())
}

// ---- copying an app's own schema and rows -------------------------------------

/// One schema object belonging to the app itself: never uApp's tables, never
/// SQLite internals, never the shadow tables a virtual table keeps its data in
/// (those come back on their own when the virtual table is created).
pub(crate) struct SchemaObject {
    pub typ: String,
    pub name: String,
    pub sql: String,
}

/// The app's own tables, views, indexes and triggers, in creation order.
pub(crate) fn user_objects(conn: &Connection) -> Result<Vec<SchemaObject>> {
    let shadows = shadow_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE sql IS NOT NULL
           AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'uapp_%' AND name != 'sqlar'
           AND (tbl_name IS NULL OR (tbl_name NOT LIKE 'uapp_%' AND tbl_name != 'sqlar'))
         ORDER BY rowid",
    )?;
    let mut out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(1)?;
        let tbl: Option<String> = r.get(2)?;
        if shadows.contains(&name) || tbl.is_some_and(|t| shadows.contains(&t)) {
            continue;
        }
        out.push(SchemaObject {
            typ: r.get(0)?,
            name,
            sql: r.get(3)?,
        });
    }
    Ok(out)
}

/// The shadow tables of this database's virtual tables (FTS3/4/5, R*Tree).
/// SQLite reports them when it has the module; the module's naming scheme
/// covers the rest.
fn shadow_tables(conn: &Connection) -> Result<std::collections::BTreeSet<String>> {
    let mut out = std::collections::BTreeSet::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='shadow'")
    {
        let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for n in names {
            out.insert(n?);
        }
    }
    let mut stmt = conn.prepare(
        "SELECT name, lower(sql) FROM sqlite_master
         WHERE type='table' AND lower(sql) LIKE 'create virtual table%'",
    )?;
    let vtabs = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for v in vtabs {
        let (name, sql) = v?;
        let module = sql
            .split_once(" using ")
            .map(|(_, m)| m.trim_start())
            .unwrap_or("");
        let suffixes: &[&str] = if module.starts_with("fts5") {
            &["_data", "_idx", "_content", "_docsize", "_config"]
        } else if module.starts_with("fts3") || module.starts_with("fts4") {
            &["_content", "_segments", "_segdir", "_docsize", "_stat"]
        } else if module.starts_with("rtree") || module.starts_with("geopoly") {
            &["_node", "_rowid", "_parent"]
        } else {
            &[]
        };
        for s in suffixes {
            out.insert(format!("{name}{s}"));
        }
    }
    Ok(out)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Run one object's CREATE statement (or a template's ALTER TABLE ... ADD
/// COLUMN, `typ` "alter") on `conn`. The text comes from another file's
/// sqlite_master, which anyone can edit, so it must be exactly one statement
/// of the expected kind, and it runs under the same sandbox as app SQL (no
/// ATTACH, no transaction control, nothing that touches uApp's tables).
pub(crate) fn exec_schema_sql(conn: &Connection, typ: &str, name: &str, sql: &str) -> Result<()> {
    let kw = leading_keywords(sql, 2);
    let kw: Vec<&str> = kw.iter().map(String::as_str).collect();
    let expected = matches!(
        (typ, kw.as_slice()),
        ("table", ["create", "table" | "virtual"])
            | ("index", ["create", "index" | "unique"])
            | ("view", ["create", "view"])
            | ("trigger", ["create", "trigger"])
            | ("alter", ["alter", "table"])
    );
    if !expected {
        bail!(
            "the schema SQL for {typ} {name} isn't the CREATE statement it should be, so the \
             file may have been tampered with: {}",
            sql.chars().take(80).collect::<String>()
        );
    }
    let guard = Guard::new(conn, Access::Write)?;
    guard
        .run(|| {
            // Whatever follows the first statement is refused unread: even
            // preparing it could act (SQLCipher runs its key PRAGMAs then).
            let len = first_statement_len(conn, sql)?;
            if !only_trivia(&sql[len..]) {
                bail!(
                    "the schema SQL for {typ} {name} has more statements after the CREATE, so \
                     the file may have been tampered with"
                );
            }
            conn.prepare(&sql[..len])?.raw_execute()?;
            Ok(())
        })
        .with_context(|| format!("creating {typ} {name}"))
}

/// Byte length of the first statement in `sql`, found by SQLite's own parser
/// (prepared and thrown away, so the authorizer of the caller's guard sees
/// it). The rest of the text is never looked at by SQLite.
fn first_statement_len(conn: &Connection, sql: &str) -> Result<usize> {
    use rusqlite::ffi;
    let c = std::ffi::CString::new(sql).map_err(|_| anyhow!("schema SQL contains a NUL byte"))?;
    let mut stmt = std::ptr::null_mut();
    let mut tail = std::ptr::null();
    // SAFETY: `c` outlives the call, `tail` points into it (or is null), and
    // the statement is finalized before the handle is used again.
    let (rc, len) = unsafe {
        let rc = ffi::sqlite3_prepare_v2(conn.handle(), c.as_ptr(), -1, &mut stmt, &mut tail);
        ffi::sqlite3_finalize(stmt);
        let len = if tail.is_null() {
            sql.len()
        } else {
            tail.offset_from(c.as_ptr()) as usize
        };
        (rc, len)
    };
    if rc != ffi::SQLITE_OK {
        // Prepare the same text through rusqlite for a proper error.
        conn.prepare(&sql[..len.min(sql.len())])?;
        bail!("could not read the schema SQL (sqlite error {rc})");
    }
    Ok(len.min(sql.len()))
}

/// Nothing but whitespace, semicolons and comments.
fn only_trivia(mut rest: &str) -> bool {
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.find('\n').map(|i| &r[i + 1..]).unwrap_or("");
        } else if let Some(r) = rest.strip_prefix("/*") {
            match r.find("*/") {
                Some(i) => rest = &r[i + 2..],
                None => return false,
            }
        } else {
            return rest.is_empty();
        }
    }
}

/// Copy every row of one of the app's tables from `src` into the same table
/// in `dst`. Columns are named from `table_xinfo`, skipping generated
/// columns (SQLite computes them) and a virtual table's hidden ones. A rowid
/// that is the row's only identity (no INTEGER PRIMARY KEY, or an FTS
/// table's docid) is carried over too, so references to it stay valid.
pub(crate) fn copy_rows(src: &Connection, dst: &Connection, table: &str) -> Result<usize> {
    let mut cols: Vec<(String, String, i64)> = Vec::new();
    {
        let mut stmt = src
            .prepare("SELECT name, upper(type), pk FROM pragma_table_xinfo(?1) WHERE hidden = 0")?;
        let rows = stmt.query_map([table], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        for c in rows {
            cols.push(c?);
        }
    }
    if cols.is_empty() {
        return Ok(0);
    }
    let (kind, without_rowid, sql): (String, bool, String) = src.query_row(
        "SELECT l.type, l.wr, lower(m.sql) FROM pragma_table_list AS l
         JOIN sqlite_master AS m ON m.name = l.name
         WHERE l.schema = 'main' AND l.name = ?1",
        [table],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let pk_cols: Vec<&(String, String, i64)> = cols.iter().filter(|c| c.2 > 0).collect();
    let rowid_alias = pk_cols.len() == 1 && pk_cols[0].1 == "INTEGER";
    let name_taken = cols.iter().any(|c| {
        matches!(
            c.0.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
        )
    });
    let keep_rowid = !name_taken
        && match kind.as_str() {
            "table" => !without_rowid && !rowid_alias,
            "virtual" => sql.contains(" using fts"),
            _ => false,
        };
    let mut list: Vec<String> = cols.iter().map(|c| quote_ident(&c.0)).collect();
    if keep_rowid {
        list.insert(0, "rowid".into());
    }
    let list = list.join(", ");
    let holders = vec!["?"; cols.len() + keep_rowid as usize].join(", ");
    let quoted = quote_ident(table);
    let mut sel = src.prepare(&format!("SELECT {list} FROM {quoted}"))?;
    let ncols = sel.column_count();
    let mut ins = dst.prepare(&format!("INSERT INTO {quoted} ({list}) VALUES ({holders})"))?;
    let mut rows = sel.query([])?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        let vals: Vec<SqlValue> = (0..ncols)
            .map(|i| row.get::<_, SqlValue>(i))
            .collect::<rusqlite::Result<_>>()?;
        ins.execute(rusqlite::params_from_iter(vals.iter()))?;
        n += 1;
    }
    Ok(n)
}

/// Lock down a connection on a .uapp that came from somewhere else (a dropped
/// template, a publish, a library upload): everything `harden` does, plus
/// `trusted_schema=OFF`, so the file's views and triggers can't call SQL
/// functions with side effects.
pub fn harden_foreign(conn: &Connection) -> Result<()> {
    harden(conn)?;
    conn.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    )?;
    Ok(())
}

/// Fill a fresh destination connection with an export of `mem`: a new app_id +
/// meta, the sqlar files for the wanted roles, the user schema (with rows only
/// when `include_rows`), and config minus secrets. The one body behind
/// `export_template` (app files, tables emptied), `export_public` (+ rows, and
/// `data/` when asked) and their wasm twin — they differ only in these two
/// flags and in how the destination is opened.
fn populate_export(
    out: &Connection,
    mem: &Connection,
    app_name: &str,
    include_data: bool,
    include_rows: bool,
) -> Result<()> {
    out.execute_batch(SCHEMA)?;
    let app_id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
    out.execute(
        "INSERT INTO uapp_meta(key,value) VALUES ('app_id',?1),('name',?2),('created',?3)",
        rusqlite::params![app_id, app_name, now_ms().to_string()],
    )?;
    stamp_format(out)?;
    // Files: stored form copied verbatim (compression preserved). `data/` only
    // when the caller opted in; app files always.
    {
        let mut sel = mem.prepare("SELECT name, mode, mtime, sz, data FROM sqlar")?;
        let mut ins =
            out.prepare("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?1,?2,?3,?4,?5)")?;
        let mut rows = sel.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let role = file_role(&name);
            if role != "app" && !(include_data && role == "data") {
                continue;
            }
            ins.execute(rusqlite::params![
                name,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?
            ])?;
        }
    }
    // Tables first, then (when wanted) their rows, and only THEN indexes,
    // views and triggers: a trigger created first would fire on the copied
    // rows.
    let objects = user_objects(mem)?;
    for o in objects.iter().filter(|o| o.typ == "table") {
        exec_schema_sql(out, &o.typ, &o.name, &o.sql)?;
        if include_rows {
            copy_rows(mem, out, &o.name)?;
        }
    }
    for o in objects.iter().filter(|o| o.typ != "table") {
        exec_schema_sql(out, &o.typ, &o.name, &o.sql)?;
    }
    // Config: everything except secrets — the `ai` entry keeps the
    // provider/model choice, minus api_key.
    let mut sel = mem.prepare("SELECT key, value FROM uapp_config")?;
    let mut rows = sel.query([])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let raw: String = row.get(1)?;
        let value = if key == "ai" {
            let mut v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.remove("api_key");
            }
            if v.is_null() {
                continue;
            }
            v.to_string()
        } else {
            raw
        };
        out.execute(
            "INSERT INTO uapp_config(key,value) VALUES(?1,?2)",
            rusqlite::params![key, value],
        )?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
/// Build a template .uapp from the live app: `app/` files + empty user tables +
/// config minus the API key, a fresh app_id — no data/, rows, chat or history.
pub fn export_template(mem: &Connection, app_name: &str) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!(
        "uapp-template-{}.uapp",
        (0..10)
            .map(|_| fastrand::alphanumeric())
            .collect::<String>()
    ));
    let build = (|| -> Result<()> {
        let out = Connection::open(&tmp)?;
        out.pragma_update(None, "journal_mode", "DELETE")?;
        populate_export(&out, mem, app_name, false, false)?;
        out.pragma_update(None, "synchronous", "FULL")?;
        Ok(())
    })();
    let bytes = build.and_then(|_| Ok(std::fs::read(&tmp)?));
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
/// Export the copy a hosted site hands to a visitor's browser.
///
/// Sits between `export_template` (app files, tables emptied) and a raw copy of
/// the file. A hosted site's own SQL machinery — post index, tags, whatever the
/// build pipeline reads — is useless without its rows, so user tables are copied
/// WITH their data. Chat, sessions, file history and config secrets never leave.
///
/// `include_data` also ships `data/` — the markdown/asset SOURCES a site's build
/// pipeline reads, so a visitor can re-run that pipeline in their browser.
/// Everything in the result is world-readable by definition: this archive is
/// downloaded by anyone who visits. Off unless the caller opts in.
pub fn export_public(mem: &Connection, app_name: &str, include_data: bool) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!(
        "uapp-public-{}.uapp",
        (0..10)
            .map(|_| fastrand::alphanumeric())
            .collect::<String>()
    ));
    let build = (|| -> Result<()> {
        let out = Connection::open(&tmp)?;
        out.pragma_update(None, "journal_mode", "DELETE")?;
        populate_export(&out, mem, app_name, include_data, true)?;
        out.pragma_update(None, "synchronous", "FULL")?;
        Ok(())
    })();
    let bytes = build.and_then(|_| Ok(std::fs::read(&tmp)?));
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
/// Windows temporarily locks a freshly-touched file out from under us:
/// OneDrive "Files On-Demand" placeholders lock the range while hydrating,
/// and AV / Search Indexer scan new downloads. Both surface as
/// ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33) and clear within
/// a moment, so retry a file-open a handful of times before giving up.
pub fn with_open_retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    // Patient enough to ride out a OneDrive upload of a just-written file
    // (~10s worst case), which can hold the lock longer than a scan.
    let mut delay = std::time::Duration::from_millis(40);
    const ATTEMPTS: usize = 12;
    for attempt in 0..ATTEMPTS {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = format!("{e:#}");
                let transient = msg.contains("os error 32")
                    || msg.contains("os error 33")
                    || msg.contains("locked a portion")
                    || msg.contains("being used by another process")
                    || msg.contains("database is locked");
                if !transient || attempt == ATTEMPTS - 1 {
                    return Err(e);
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(1000));
            }
        }
    }
    unreachable!()
}

#[cfg(not(target_arch = "wasm32"))]
/// Serialize the CURRENT state to a complete, self-contained .uapp (all
/// tables, files, chat, config — everything, unlike a template). Used to
/// download the app so the user can keep a copy. When the app is encrypted,
/// the export is encrypted too (a plaintext download would silently defeat
/// the file's at-rest protection).
pub fn export_full(db: &Connection, key: Option<&str>) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!(
        "uapp-export-{}.uapp",
        (0..12)
            .map(|_| fastrand::alphanumeric())
            .collect::<String>()
    ));
    let build = (|| -> Result<()> {
        let mut dst = match key {
            Some(p) => crate::cipher::open_file(&tmp, p)?,
            None => Connection::open(&tmp)?,
        };
        dst.pragma_update(None, "journal_mode", "DELETE")?;
        dst.pragma_update(None, "synchronous", "FULL")?;
        let backup = rusqlite::backup::Backup::new(db, &mut dst)?;
        backup.run_to_completion(512, std::time::Duration::from_millis(0), None)?;
        Ok(())
    })();
    let bytes = build.and_then(|_| Ok(std::fs::read(&tmp)?));
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
/// Is this file a plausible SQLite db? (0-byte and garbage both => false)
/// Reads only the 16-byte header — never the whole file — so it can't cross
/// SQLite's high-offset lock region or stall on a large archive, and it's
/// wrapped in the sharing-violation retry for freshly-downloaded files.
pub fn looks_like_sqlite(path: &Path) -> bool {
    use std::io::Read as _;
    with_open_retry(|| {
        let mut f = std::fs::File::open(path)?;
        let mut head = [0u8; 16];
        let n = f.read(&mut head)?;
        Ok(n >= 16 && &head[..15] == b"SQLite format 3")
    })
    .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
/// Is this file plausibly a WHOLE .uapp, as opposed to an empty or cut-off
/// copy? Used before an incoming file is allowed to replace a working copy
/// (Android "open with", where a provider or an interrupted copy can hand us
/// zero bytes). A plain file must hold at least the pages its header claims;
/// an encrypted one has no readable header, so it only has to be a non-empty
/// run of whole pages.
pub fn check_whole_file(path: &Path) -> Result<()> {
    use std::io::Read as _;
    let len = std::fs::metadata(path)?.len();
    if len == 0 {
        bail!("{} is empty", path.display());
    }
    let mut head = [0u8; 100];
    let n = std::fs::File::open(path)?.read(&mut head)?;
    check_whole_bytes(&head[..n], len).map_err(|e| anyhow!("{}: {e}", path.display()))
}

/// [`check_whole_file`] over the first (up to) 100 bytes and the total length.
pub fn check_whole_bytes(head: &[u8], len: u64) -> Result<()> {
    if len == 0 {
        bail!("the file is empty");
    }
    if head.len() >= 100 && &head[..15] == b"SQLite format 3" {
        let be32 = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let page = match u16::from_be_bytes([head[16], head[17]]) {
            1 => 65536u64,
            p => p as u64,
        };
        if !(512..=65536).contains(&page) || !page.is_power_of_two() {
            bail!("the file has a damaged header");
        }
        // The in-header page count is only valid when the change counter
        // matches version-valid-for (SQLite file format, section 1.3.7).
        let pages = be32(28) as u64;
        if pages > 0 && be32(24) == be32(92) && len < pages * page {
            bail!("the file is cut off ({len} of {} bytes)", pages * page);
        }
        if len < page {
            bail!("the file is cut off ({len} bytes)");
        }
        return Ok(());
    }
    if len < 1024 || len % 512 != 0 {
        bail!("the file is not a whole .uapp ({len} bytes)");
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
/// A non-empty file that isn't plain SQLite: an encrypted .uapp (SQLCipher) that
/// needs a master password before it can be opened. Used to decide whether to
/// show the unlock prompt.
pub fn is_encrypted_file(path: &Path) -> bool {
    !path.is_dir()
        && std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        && !looks_like_sqlite(path)
}

pub fn json_to_sql(v: &Value) -> Result<SqlValue> {
    Ok(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else {
                SqlValue::Real(n.as_f64().ok_or_else(|| anyhow!("bad number"))?)
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Object(o) => {
            if let Some(Value::String(b)) = o.get("b64") {
                SqlValue::Blob(base64::engine::general_purpose::STANDARD.decode(b)?)
            } else {
                SqlValue::Text(v.to_string())
            }
        }
        Value::Array(_) => SqlValue::Text(v.to_string()),
    })
}

pub fn sql_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!({"b64": base64::engine::general_purpose::STANDARD.encode(b)}),
    }
}

fn bind_params(stmt: &mut rusqlite::Statement, params: &[Value]) -> Result<()> {
    let want = stmt.parameter_count();
    if want != params.len() {
        bail!(
            "statement has {want} placeholder{} but {} param{} were given: {}",
            if want == 1 { "" } else { "s" },
            params.len(),
            if params.len() == 1 { "" } else { "s" },
            stmt.expanded_sql()
                .unwrap_or_default()
                .chars()
                .take(120)
                .collect::<String>()
        );
    }
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, json_to_sql(p)?)?;
    }
    Ok(())
}

/// The first `n` SQL keywords, lowercased, skipping whitespace and -- / /* */
/// comments. Stops early at anything that isn't a plain word.
fn leading_keywords(sql: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = sql;
    while out.len() < n {
        loop {
            rest = rest.trim_start();
            if rest.starts_with("--") {
                rest = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or("");
            } else if rest.starts_with("/*") {
                rest = rest.find("*/").map(|i| &rest[i + 2..]).unwrap_or("");
            } else {
                break;
            }
        }
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        if word.is_empty() {
            break;
        }
        rest = &rest[word.len()..];
        out.push(word.to_ascii_lowercase());
    }
    out
}

/// First SQL keyword, skipping whitespace and comments.
fn first_keyword(sql: &str) -> String {
    leading_keywords(sql, 1).pop().unwrap_or_default()
}

/// Word-boundary scan of a whole (possibly multi-statement) SQL text for
/// ATTACH/DETACH: coarse (a string literal can false-positive), but a false
/// positive only means the assistant asks for permission it didn't need. The
/// authorizer below is what actually enforces the rule.
pub fn mentions_attach(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    let b = lower.as_bytes();
    for kw in ["attach", "detach"] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(kw) {
            let i = from + pos;
            let j = i + kw.len();
            let boundary = |c: u8| !(c.is_ascii_alphanumeric() || c == b'_');
            if (i == 0 || boundary(b[i - 1])) && (j >= b.len() || boundary(b[j])) {
                return true;
            }
            from = j;
        }
    }
    false
}

// ---- the SQL sandbox ----------------------------------------------------------
//
// SQL text from outside uApp (app pages through uapp.js, the shell's SQL
// panel, the assistant's sql tools) arrives through `query` and the "sql",
// "batch" and "import" ops. SQLite's own read-only flag isn't enough for it:
// most PRAGMAs count as read-only (`PRAGMA rekey` would lock the user out of
// an encrypted file), and a batch could COMMIT the engine's transaction and
// then run anything (`VACUUM INTO '/any/path'`). So while that SQL is
// prepared and run, an authorizer checks every action SQLite is about to
// take, and refuses:
//
//   * transaction control (each exec/batch already is one transaction);
//   * ATTACH/DETACH, unless the user approved it in the assistant's prompt;
//   * PRAGMAs outside a short read-only list;
//   * writes to uApp's own tables (`uapp_*`, `sqlar`) and to SQLite's
//     internals, and writes to any attached database.
//
// Internal code (file writes, chat, settings, migrations) talks to SQLite
// directly and is never checked: the authorizer is only installed for the
// length of one guarded call. `harden` adds the always-on part for the live
// connection and for files opened from elsewhere.

/// PRAGMAs that only report on the schema; any argument is fine.
const PRAGMA_INSPECT: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
    "database_list",
    "collation_list",
    "function_list",
    "module_list",
    "pragma_list",
    "compile_options",
];

/// PRAGMAs that may be read but not set.
const PRAGMA_READ_ONLY: &[&str] = &[
    "user_version",
    "application_id",
    "schema_version",
    "data_version",
    "page_count",
    "page_size",
    "freelist_count",
    "encoding",
    "foreign_keys",
    "journal_mode",
    "synchronous",
    "auto_vacuum",
];

/// SQLite's own tables that ordinary DDL updates (CREATE/DROP/ALTER write the
/// schema table, AUTOINCREMENT keeps sqlite_sequence, ANALYZE the stat
/// tables). Direct writes to the schema table are refused by SQLite itself.
const SQLITE_BOOKKEEPING: &[&str] = &[
    "sqlite_master",
    "sqlite_schema",
    "sqlite_temp_master",
    "sqlite_temp_schema",
    "sqlite_sequence",
    "sqlite_stat1",
    "sqlite_stat2",
    "sqlite_stat3",
    "sqlite_stat4",
];

/// What a guarded call may do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    /// `sql.query`: read anything, change nothing.
    Read,
    /// `sql.query` after the user approved ATTACH/DETACH in the assistant's
    /// permission prompt.
    ReadAttach,
    /// `sql.exec`, `sql.batch`, CSV import, and schema copied in from another
    /// file (a template, a publish, a library upload).
    Write,
}

enum Verdict {
    Allow,
    /// Skip the action without an error (only used for `PRAGMA foreign_keys
    /// = ...`, which was always a silent no-op inside a write transaction).
    Ignore,
    Deny(String),
}

/// A uApp table or a SQLite internal that app SQL may read but never change.
fn reserved_name(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    if n == "sqlar" {
        Some(
            "sqlar holds the app's files and can't be changed with SQL (use files.write \
             and files.delete)"
                .into(),
        )
    } else if n.starts_with("uapp_") {
        Some(format!(
            "{name} is one of uApp's own tables (chat, file history, settings) and can't be \
             changed with SQL"
        ))
    } else if n.starts_with("sqlite_")
        && !SQLITE_BOOKKEEPING.contains(&n.as_str())
        // The indexes SQLite makes for PRIMARY KEY / UNIQUE constraints.
        && !n.starts_with("sqlite_autoindex_")
    {
        Some(format!("{name} is internal to SQLite and can't be changed"))
    } else {
        None
    }
}

fn judge(access: Access, ctx: &rusqlite::hooks::AuthContext<'_>) -> Verdict {
    use rusqlite::hooks::AuthAction as A;
    let deny = |why: String| Verdict::Deny(why);
    // The tables (and other objects) a write action touches.
    let targets: &[&str] =
        match &ctx.action {
            A::Transaction { .. } | A::Savepoint { .. } => return deny(
                "BEGIN, COMMIT, ROLLBACK and SAVEPOINT can't be used here: every exec or batch \
                 already runs as one transaction, committed when it succeeds and rolled back \
                 when any statement fails"
                    .into(),
            ),
            A::Attach { .. } | A::Detach { .. } => {
                return match access {
                    Access::ReadAttach => Verdict::Allow,
                    Access::Read => deny(
                        "ATTACH/DETACH opens another database file on this machine and needs \
                     explicit user approval; ask via the assistant, which prompts for permission"
                            .into(),
                    ),
                    Access::Write => deny(
                        "ATTACH/DETACH can't run in app writes; read the other database with an \
                     approved read-only query and inline the values instead"
                            .into(),
                    ),
                };
            }
            A::Pragma {
                pragma_name,
                pragma_value,
            } => {
                let name = pragma_name.to_ascii_lowercase();
                return if PRAGMA_INSPECT.contains(&name.as_str()) {
                    Verdict::Allow
                } else if PRAGMA_READ_ONLY.contains(&name.as_str()) {
                    match pragma_value {
                        None => Verdict::Allow,
                        Some(_) if name == "foreign_keys" => Verdict::Ignore,
                        Some(_) => deny(format!(
                            "PRAGMA {name} can be read here but not set (uApp manages it)"
                        )),
                    }
                } else {
                    deny(format!(
                        "PRAGMA {name} isn't allowed from app or assistant SQL; only read-only \
                     PRAGMAs such as table_info, index_list and foreign_key_list are"
                    ))
                };
            }
            A::Function { function_name } => {
                let f = function_name.to_ascii_lowercase();
                return if f.starts_with("sqlcipher") || f == "load_extension" {
                    deny(format!(
                        "the SQL function {function_name}() isn't allowed here"
                    ))
                } else {
                    Verdict::Allow
                };
            }
            A::Insert { table_name }
            | A::Delete { table_name }
            | A::Update { table_name, .. }
            | A::CreateTable { table_name }
            | A::CreateTempTable { table_name }
            | A::DropTable { table_name }
            | A::DropTempTable { table_name }
            | A::AlterTable { table_name, .. } => &[table_name],
            A::CreateIndex {
                index_name,
                table_name,
            }
            | A::CreateTempIndex {
                index_name,
                table_name,
            }
            | A::DropIndex {
                index_name,
                table_name,
            }
            | A::DropTempIndex {
                index_name,
                table_name,
            } => &[index_name, table_name],
            A::CreateTrigger {
                trigger_name,
                table_name,
            }
            | A::CreateTempTrigger {
                trigger_name,
                table_name,
            }
            | A::DropTrigger {
                trigger_name,
                table_name,
            }
            | A::DropTempTrigger {
                trigger_name,
                table_name,
            } => &[trigger_name, table_name],
            A::CreateView { view_name }
            | A::CreateTempView { view_name }
            | A::DropView { view_name }
            | A::DropTempView { view_name } => &[view_name],
            A::CreateVtable { table_name, .. } | A::DropVtable { table_name, .. } => &[table_name],
            // Reads, SELECT, recursive CTEs, ANALYZE, REINDEX.
            _ => return Verdict::Allow,
        };
    if access != Access::Write {
        return deny("query() only accepts read-only SQL; use exec for writes".into());
    }
    // ALTER TABLE reports the database as its first argument instead.
    let db = match &ctx.action {
        A::AlterTable { database_name, .. } => Some(*database_name),
        _ => ctx.database_name,
    };
    if let Some(db) = db.filter(|d| *d != "main" && *d != "temp") {
        return deny(format!(
            "writing to the attached database {db} isn't allowed; only this app's own tables \
             can be changed"
        ));
    }
    match targets.iter().find_map(|t| reserved_name(t)) {
        Some(why) => deny(why),
        None => Verdict::Allow,
    }
}

/// The always-on part of the sandbox, for the live connection and for any
/// .uapp opened from somewhere else: no ATTACH (a guarded call that the user
/// approved lifts this for its own length), and SQLite's DEFENSIVE mode, which
/// turns off the features that let plain SQL corrupt the file
/// (`writable_schema`, writes to `sqlite_dbpage` and FTS shadow tables, ...).
pub fn harden(conn: &Connection) -> Result<()> {
    conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    Ok(())
}

/// The authorizer installed for one guarded call. Dropping it uninstalls the
/// authorizer and puts the attach limit back.
struct Guard<'c> {
    conn: &'c Connection,
    refused: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    attach_limit: Option<i32>,
}

impl<'c> Guard<'c> {
    fn new(conn: &'c Connection, access: Access) -> Result<Guard<'c>> {
        use rusqlite::hooks::Authorization;
        let refused = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let why = refused.clone();
        conn.authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            match judge(access, &ctx) {
                Verdict::Allow => Authorization::Allow,
                Verdict::Ignore => Authorization::Ignore,
                Verdict::Deny(reason) => {
                    // Keep the first reason: it names what the statement
                    // itself tried, before anything it would have led to.
                    if let Ok(mut slot) = why.lock() {
                        slot.get_or_insert(reason);
                    }
                    Authorization::Deny
                }
            }
        }))?;
        let mut guard = Guard {
            conn,
            refused,
            attach_limit: None,
        };
        if access == Access::ReadAttach {
            guard.attach_limit =
                Some(conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 10)?);
        }
        Ok(guard)
    }

    /// A failure inside the guarded call, reworded to say what was refused and
    /// why when the authorizer is the reason ("not authorized" says neither).
    fn explain(&self, e: impl Into<anyhow::Error>) -> anyhow::Error {
        match self.refused.lock().ok().and_then(|mut r| r.take()) {
            Some(why) => anyhow!(why),
            None => e.into(),
        }
    }

    /// Run `f` under the guard, rewording any refusal.
    fn run<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        f().map_err(|e| self.explain(e))
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        );
        if let Some(prev) = self.attach_limit {
            let _ = self
                .conn
                .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, prev);
        }
    }
}

/// Run a read-only query from app or assistant SQL. ATTACH/DETACH are
/// refused here; callers that obtained explicit user approval use
/// query_allow_attach instead.
pub fn query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Value> {
    run_query(conn, sql, params, Access::Read)
}

/// query() that may also ATTACH/DETACH: only for calls the user explicitly
/// approved (the assistant's permission prompt).
pub fn query_allow_attach(conn: &Connection, sql: &str, params: &[Value]) -> Result<Value> {
    run_query(conn, sql, params, Access::ReadAttach)
}

fn run_query(conn: &Connection, sql: &str, params: &[Value], access: Access) -> Result<Value> {
    let guard = Guard::new(conn, access)?;
    guard.run(|| {
        let mut stmt = conn.prepare(sql)?;
        if !stmt.readonly() {
            bail!("query() only accepts read-only SQL; use exec for writes");
        }
        bind_params(&mut stmt, params)?;
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let ncols = cols.len();
        let mut rows_out = Vec::new();
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next()? {
            let mut arr = Vec::with_capacity(ncols);
            for i in 0..ncols {
                arr.push(sql_to_json(row.get_ref(i)?));
            }
            rows_out.push(Value::Array(arr));
            if rows_out.len() >= 50_000 {
                bail!("query returned more than 50000 rows; add a LIMIT");
            }
        }
        Ok(json!({"columns": cols, "rows": rows_out}))
    })
}

/// Snapshot a file's outgoing content into the history table before a write or
/// a delete. Public so a template update can record its file writes the same
/// way an ordinary edit does — and stay revertible from the file browser.
pub fn record_file_history(conn: &Connection, op: &Op, name: &str, action: &str) -> Result<()> {
    // Snapshot the PREVIOUS content (if any) so any change is revertible.
    // The sqlar row already holds the sqlar-compressed form — copy data + sz
    // verbatim (sz == length(data) still means "stored raw", like sqlar).
    conn.execute(
        "INSERT INTO uapp_file_history(name, ts, device, user, action, data, sz)
         SELECT ?1, ?2, ?3, ?4, ?5, data, sz FROM sqlar WHERE name = ?1",
        rusqlite::params![name, op.ts as i64, op.dev, op.user, action],
    )?;
    // Keep history bounded per file.
    conn.execute(
        "DELETE FROM uapp_file_history WHERE name=?1 AND id NOT IN
         (SELECT id FROM uapp_file_history WHERE name=?1 ORDER BY id DESC LIMIT 40)",
        rusqlite::params![name],
    )?;
    Ok(())
}

/// Apply one op to the live database. The caller (`Engine::local_op`) runs
/// it inside a transaction, so a failing op leaves no partial effects.
pub fn apply_op(conn: &Connection, op: &Op) -> Result<Value> {
    let p = &op.payload;
    match op.kind.as_str() {
        "sql" => {
            let sql = p["sql"]
                .as_str()
                .ok_or_else(|| anyhow!("sql op missing sql"))?;
            let params: Vec<Value> = p["params"].as_array().cloned().unwrap_or_default();
            let guard = Guard::new(conn, Access::Write)?;
            guard.run(|| {
                let mut stmt = conn.prepare(sql)?;
                bind_params(&mut stmt, &params)?;
                // Statements that produce columns (INSERT/UPDATE/DELETE ...
                // RETURNING, or a stray SELECT) return their rows, so callers
                // can read back e.g. the new id in the SAME statement — no
                // insert-then-SELECT-MAX(id) race.
                if stmt.column_count() > 0 {
                    let cols: Vec<String> =
                        stmt.column_names().iter().map(|s| s.to_string()).collect();
                    let ncols = cols.len();
                    let mut rows_out = Vec::new();
                    let mut rows = stmt.raw_query();
                    while let Some(row) = rows.next()? {
                        let mut arr = Vec::with_capacity(ncols);
                        for i in 0..ncols {
                            arr.push(sql_to_json(row.get_ref(i)?));
                        }
                        rows_out.push(Value::Array(arr));
                    }
                    return Ok(
                        json!({"changes": rows_out.len(), "columns": cols, "rows": rows_out}),
                    );
                }
                let n = stmt.raw_execute()?;
                drop(stmt);
                // Rowid of the row this write created (0 = none: UPDATE/DELETE/
                // DDL or a table without a rowid alias). Lets actions do
                // `const {insert_id} = await uapp.exec("INSERT ...")`.
                let iid = if matches!(first_keyword(sql).as_str(), "insert" | "replace") {
                    conn.last_insert_rowid()
                } else {
                    0
                };
                Ok(
                    json!({"changes": n, "insert_id": if iid > 0 { json!(iid) } else { Value::Null } }),
                )
            })
        }
        "batch" => {
            let sql = p["sql"]
                .as_str()
                .ok_or_else(|| anyhow!("batch op missing sql"))?;
            let guard = Guard::new(conn, Access::Write)?;
            guard.run(|| Ok(conn.execute_batch(sql)?))?;
            Ok(json!({"ok": true}))
        }
        "import" => {
            // One op = one whole dataset. {table, columns, rows, create?, truncate?}
            let table = p["table"]
                .as_str()
                .ok_or_else(|| anyhow!("import missing table"))?;
            if !table.chars().all(|c| c.is_alphanumeric() || c == '_') {
                bail!("import table name must be alphanumeric/underscore");
            }
            let guard = Guard::new(conn, Access::Write)?;
            guard.run(|| import_rows(conn, table, p))
        }
        // Names are normalized here, not at the caller: a name from before the
        // app/data split (or a root-absolute one from an app) lands in its
        // canonical place whoever sent it.
        "file_put" => {
            let name = p["name"]
                .as_str()
                .ok_or_else(|| anyhow!("file_put missing name"))?;
            let name = canon_name(name);
            let data = base64::engine::general_purpose::STANDARD
                .decode(p["data"].as_str().unwrap_or_default())?;
            record_file_history(conn, op, &name, "put")?;
            sqlar_write(conn, &name, (op.ts / 1000) as i64, &data)?;
            Ok(json!({"ok": true, "bytes": data.len()}))
        }
        "file_del" => {
            let name = p["name"]
                .as_str()
                .ok_or_else(|| anyhow!("file_del missing name"))?;
            // Delete whatever the name resolves to today; an old op naming
            // `index.html` still removes `app/index.html`.
            let name = resolve_name(conn, name)?.unwrap_or_else(|| canon_name(name));
            record_file_history(conn, op, &name, "del")?;
            conn.execute("DELETE FROM sqlar WHERE name=?1", rusqlite::params![name])?;
            Ok(json!({"ok": true}))
        }
        // One op = one whole "update this app from a template" (see
        // `crate::template`): app files replaced, schema reconciled additively,
        // user data untouched. It rides the op path so the engine's single
        // transaction makes the whole update atomic.
        "template_update" => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(p["b64"].as_str().unwrap_or_default())?;
            let src = crate::template::Source::from_bytes(&bytes, p["password"].as_str())?;
            crate::template::apply(
                conn,
                src.conn(),
                op,
                p["remove_stale"].as_bool().unwrap_or(true),
            )
        }
        "publish" => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(p["b64"].as_str().unwrap_or_default())?;
            let src = crate::template::Source::from_bytes(&bytes, None)?;
            crate::template::publish(conn, src.conn(), op, p["data"].as_bool().unwrap_or(false))
        }
        // The app's display name. Its own op (rather than SQL on uapp_meta) so
        // the generic SQL paths never need write access to uApp's tables.
        "app_rename" => {
            let name = p["name"]
                .as_str()
                .ok_or_else(|| anyhow!("app_rename missing name"))?;
            conn.execute(
                "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES('name',?1)",
                rusqlite::params![name],
            )?;
            Ok(json!({"ok": true}))
        }
        "config_set" => {
            let key = p["key"]
                .as_str()
                .ok_or_else(|| anyhow!("config_set missing key"))?;
            conn.execute(
                "INSERT OR REPLACE INTO uapp_config(key,value) VALUES(?1,?2)",
                rusqlite::params![key, p["value"].to_string()],
            )?;
            Ok(json!({"ok": true}))
        }
        "chat" => {
            let mid = p["mid"]
                .as_str()
                .ok_or_else(|| anyhow!("chat missing mid"))?;
            conn.execute(
                "INSERT OR IGNORE INTO uapp_chat(mid,ts,ctr,device,user,role,content,session)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                rusqlite::params![
                    mid,
                    op.ts as i64,
                    // ctr was the old sync engine's same-millisecond tiebreak;
                    // single-writer inserts are ordered by rowid now.
                    0i64,
                    op.dev,
                    op.user,
                    p["role"].as_str().unwrap_or("user"),
                    p["content"].to_string(),
                    // No session means the main chat.
                    p["session"].as_str().unwrap_or("main")
                ],
            )?;
            Ok(json!({"ok": true}))
        }
        "chat_clear" => {
            // No session in the payload = clear every session.
            match p["session"].as_str() {
                Some(s) => conn.execute(
                    "DELETE FROM uapp_chat WHERE ts <= ?1 AND COALESCE(session,'main') = ?2",
                    rusqlite::params![op.ts as i64, s],
                )?,
                None => conn.execute(
                    "DELETE FROM uapp_chat WHERE ts <= ?1",
                    rusqlite::params![op.ts as i64],
                )?,
            };
            Ok(json!({"ok": true}))
        }
        // Create/update one chat session's metadata. Only the fields present in
        // the payload overwrite; `created` comes from the payload when given.
        "session_meta" => {
            let id = p["id"]
                .as_str()
                .ok_or_else(|| anyhow!("session_meta missing id"))?;
            let created = p["created"].as_i64().unwrap_or(op.ts as i64);
            conn.execute(
                "INSERT INTO uapp_sessions(id,title,created,kind,parent,archived)
                 VALUES(?1,?2,?3,COALESCE(?4,'chat'),?5,COALESCE(?6,0))
                 ON CONFLICT(id) DO UPDATE SET
                   title=COALESCE(?2,title), kind=COALESCE(?4,kind),
                   parent=COALESCE(?5,parent), archived=COALESCE(?6,archived)",
                rusqlite::params![
                    id,
                    p["title"].as_str(),
                    created,
                    p["kind"].as_str(),
                    p["parent"].as_str(),
                    p["archived"]
                        .as_bool()
                        .map(|b| b as i64)
                        .or_else(|| p["archived"].as_i64())
                ],
            )?;
            Ok(json!({"ok": true}))
        }
        // Drop a session and its messages. 'main' is implicit in the UI, so
        // deleting it just empties it.
        "session_del" => {
            let id = p["id"]
                .as_str()
                .ok_or_else(|| anyhow!("session_del missing id"))?;
            conn.execute(
                "DELETE FROM uapp_sessions WHERE id=?1",
                rusqlite::params![id],
            )?;
            conn.execute(
                "DELETE FROM uapp_chat WHERE COALESCE(session,'main')=?1",
                rusqlite::params![id],
            )?;
            Ok(json!({"ok": true}))
        }
        // Replace everything up to (upto_ts, upto_ctr) in one session with a
        // single summary row. The summary TEXT is computed by the caller before
        // the op runs (no model is called inside the transaction).
        "chat_compact" => {
            let session = p["session"].as_str().unwrap_or("main");
            let mid = p["mid"]
                .as_str()
                .ok_or_else(|| anyhow!("chat_compact missing mid"))?;
            let upto_ts = p["upto_ts"]
                .as_i64()
                .ok_or_else(|| anyhow!("chat_compact missing upto_ts"))?;
            let upto_ctr = p["upto_ctr"].as_i64().unwrap_or(0);
            // Lexicographic (ts, ctr, device) cutoff matching the chat sort
            // order; ops from older binaries carry no upto_dev, so the device
            // tie-break falls back to the old inclusive behavior ('~' sorts
            // after any device id).
            let upto_dev = p["upto_dev"].as_str().unwrap_or("~~~~");
            let removed = conn.execute(
                "DELETE FROM uapp_chat WHERE COALESCE(session,'main')=?1
                 AND (ts < ?2 OR (ts = ?2 AND (ctr < ?3 OR (ctr = ?3 AND device <= ?4))))",
                rusqlite::params![session, upto_ts, upto_ctr, upto_dev],
            )?;
            // The summary stands exactly where the compacted range ended, so
            // it sorts BEFORE the kept tail (using op.ts would file it after
            // the messages it summarizes). Both values come from the payload.
            conn.execute(
                "INSERT OR IGNORE INTO uapp_chat(mid,ts,ctr,device,user,role,content,session)
                 VALUES(?1,?2,?3,?4,?5,'summary',?6,?7)",
                rusqlite::params![
                    mid,
                    upto_ts,
                    upto_ctr,
                    op.dev,
                    op.user,
                    json!({"text": p["summary"].as_str().unwrap_or("")}).to_string(),
                    session
                ],
            )?;
            Ok(json!({"ok": true, "removed": removed}))
        }
        other => bail!("unknown op kind {other}"),
    }
}

/// The "import" op's body: one whole dataset `{table, columns, rows, create?,
/// truncate?}` into an app table. Runs under the sandbox (see `apply_op`).
fn import_rows(conn: &Connection, table: &str, p: &Value) -> Result<Value> {
    if let Some(create) = p["create"].as_str() {
        conn.execute_batch(create)?;
    }
    if p["truncate"].as_bool().unwrap_or(false) {
        conn.execute(&format!("DELETE FROM \"{}\"", table), [])?;
    }
    let cols: Vec<String> = p["columns"]
        .as_array()
        .ok_or_else(|| anyhow!("import missing columns"))?
        .iter()
        .map(|c| c.as_str().unwrap_or_default().to_string())
        .collect();
    if cols
        .iter()
        .any(|c| !c.chars().all(|ch| ch.is_alphanumeric() || ch == '_'))
    {
        bail!("import column names must be alphanumeric/underscore");
    }
    let placeholders = vec!["?"; cols.len()].join(",");
    let quoted: Vec<String> = cols.iter().map(|c| format!("\"{}\"", c)).collect();
    let sql = format!(
        "INSERT INTO \"{}\" ({}) VALUES ({})",
        table,
        quoted.join(","),
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = p["rows"]
        .as_array()
        .ok_or_else(|| anyhow!("import missing rows"))?;
    for row in rows {
        let vals = row
            .as_array()
            .ok_or_else(|| anyhow!("import row not array"))?;
        bind_params(&mut stmt, vals)?;
        stmt.raw_execute()?;
    }
    Ok(json!({"inserted": rows.len()}))
}

pub fn config_get(conn: &Connection, key: &str) -> Result<Option<Value>> {
    let mut stmt = conn.prepare("SELECT value FROM uapp_config WHERE key=?1")?;
    let mut rows = stmt.query(rusqlite::params![key])?;
    if let Some(row) = rows.next()? {
        let s: String = row.get(0)?;
        Ok(Some(serde_json::from_str(&s).unwrap_or(Value::String(s))))
    } else {
        Ok(None)
    }
}

/// zlib-compress file content exactly like the reference sqlar tool /
/// sqlar_compress(): keep the compressed form only when it is strictly
/// smaller, and record the ORIGINAL length in sz so readers can tell the
/// two apart (sz == length(data) means stored uncompressed). Fixed
/// compression level, so the same content always gives the same bytes.
fn sqlar_deflate(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::new(6),
    );
    enc.write_all(data)?;
    let out = enc.finish()?;
    Ok(if out.len() < data.len() {
        out
    } else {
        data.to_vec()
    })
}

/// True when the app still has the default, un-customized index.html — i.e. a
/// "new" app that hasn't been built yet. Drives the initial chat-sidebar state
/// (open for a new app, closed once it's a real app).
pub fn is_blank_app(conn: &Connection) -> bool {
    match sqlar_read(conn, "app/index.html") {
        Ok(Some(data)) => data == DEFAULT_INDEX_HTML.as_bytes(),
        _ => true, // no index at all → also "new"
    }
}

/// Insert/replace one file row, sqlar-style (zlib-compressed when smaller).
/// Writes always land in the canonical location, so the archive can never grow
/// a third top-level directory.
pub fn sqlar_write(conn: &Connection, name: &str, mtime: i64, data: &[u8]) -> Result<()> {
    let name = canon_name(name);
    let stored = sqlar_deflate(data)?;
    conn.execute(
        "INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?1,420,?2,?3,?4)
         ON CONFLICT(name) DO UPDATE SET mtime=?2, sz=?3, data=?4",
        rusqlite::params![name, mtime, data.len() as i64, stored],
    )?;
    Ok(())
}

/// Read one file's content, transparently inflating zlib-compressed rows —
/// same rules as sqlar_uncompress(X, SZ): sz <= 0 or sz == length(data)
/// means the blob is the content as-is; length(data) < sz means zlib.
/// Handles archives written by the reference sqlar/sqlite3 tools too.
/// Accepts a canonical name or any reference that resolves to one
/// (`name_candidates`), so `index.html` still reads `app/index.html`.
pub fn sqlar_read(conn: &Connection, name: &str) -> Result<Option<Vec<u8>>> {
    let mut stmt = conn.prepare("SELECT sz, data FROM sqlar WHERE name=?1")?;
    for cand in name_candidates(name) {
        let mut rows = stmt.query(rusqlite::params![&cand])?;
        let Some(row) = rows.next()? else { continue };
        let sz: i64 = row.get(0)?;
        let data: Option<Vec<u8>> = row.get(1)?;
        // NULL data = directory entry (external archives); not a readable file.
        let Some(data) = data else { return Ok(None) };
        return Ok(Some(sqlar_inflate(Some(sz), data, &cand)?));
    }
    Ok(None)
}

/// The most one archived file may unpack to. `sz` is stored in the file, so
/// it is no limit on its own: a crafted entry could claim gigabytes.
pub const MAX_FILE_BYTES: u64 = 512 << 20;

/// sqlar_uncompress(X, SZ) semantics for any (sz, data) pair: sz NULL/<=0 or
/// sz == length(data) means the blob is the content as-is; smaller data is
/// zlib-inflated to exactly sz bytes.
pub fn sqlar_inflate(sz: Option<i64>, data: Vec<u8>, what: &str) -> Result<Vec<u8>> {
    let Some(sz) = sz else { return Ok(data) };
    if sz <= 0 || sz == data.len() as i64 {
        return Ok(data);
    }
    if sz as u64 > MAX_FILE_BYTES {
        bail!(
            "{what} would unpack to {sz} bytes, more than the {} MiB uApp allows for one file",
            MAX_FILE_BYTES >> 20
        );
    }
    use std::io::Read as _;
    // sz is attacker-controlled (it's a column in the opened file): cap the
    // pre-allocation, and cap inflation at sz+1 so a mismatched/bomb entry
    // errors instead of exhausting memory.
    let mut out = Vec::with_capacity((sz as usize).min(1 << 24));
    flate2::read::ZlibDecoder::new(&data[..])
        .take(sz as u64 + 1)
        .read_to_end(&mut out)
        .with_context(|| format!("inflate sqlar entry {what}"))?;
    if out.len() as i64 != sz {
        bail!(
            "sqlar entry {what} is corrupt: expected {sz} bytes, inflated to {}",
            out.len()
        );
    }
    Ok(out)
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM uapp_meta WHERE key=?1")?;
    let mut rows = stmt.query(rusqlite::params![key])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

// ---- wasm exports -----------------------------------------------------------
// The browser build has no temp files: templates are built in a second
// in-memory connection and serialized; full exports serialize the live db.

#[cfg(target_arch = "wasm32")]
fn serialize_conn(conn: &Connection) -> Result<Vec<u8>> {
    unsafe {
        let mut size: i64 = 0;
        let p = rusqlite::ffi::sqlite3_serialize(conn.handle(), c"main".as_ptr(), &mut size, 0);
        if p.is_null() {
            bail!("sqlite3_serialize failed (out of memory?)");
        }
        let out = std::slice::from_raw_parts(p as *const u8, size as usize).to_vec();
        rusqlite::ffi::sqlite3_free(p as *mut std::ffi::c_void);
        Ok(out)
    }
}

/// Load complete .uapp bytes into a fresh in-memory database. The browser
/// build's stand-in for opening a file: used for the app the demo opens and
/// for a template dropped on top of it.
#[cfg(target_arch = "wasm32")]
pub fn deserialize_bytes(bytes: &[u8]) -> Result<Connection> {
    if bytes.len() < 16 || &bytes[..15] != b"SQLite format 3" {
        bail!(
            "this is not a plain .uapp/SQLite file — encrypted apps can't be \
             opened in the browser demo (decrypt it in the desktop app first)"
        );
    }
    let db = Connection::open_in_memory()?;
    unsafe {
        let len = bytes.len();
        let buf = rusqlite::ffi::sqlite3_malloc64(len as u64) as *mut u8;
        if buf.is_null() {
            bail!("out of memory loading the app ({len} bytes)");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, len);
        const FREEONCLOSE: std::os::raw::c_uint = 1;
        const RESIZEABLE: std::os::raw::c_uint = 2;
        let rc = rusqlite::ffi::sqlite3_deserialize(
            db.handle(),
            c"main".as_ptr(),
            buf,
            len as i64,
            len as i64,
            FREEONCLOSE | RESIZEABLE,
        );
        if rc != rusqlite::ffi::SQLITE_OK {
            bail!("could not load the .uapp bytes (sqlite error {rc})");
        }
    }
    let verdict: String = db.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if verdict != "ok" {
        bail!("the file failed its integrity check: {verdict}");
    }
    Ok(db)
}

#[cfg(target_arch = "wasm32")]
pub fn export_full(db: &Connection, _key: Option<&str>) -> Result<Vec<u8>> {
    serialize_conn(db)
}

/// The browser build's copy for anything that leaves this device: Download a
/// copy, /download.uapp, publishing. On native, API keys, MCP headers and
/// "always allow" approvals live outside the file (secrets.rs); in the
/// browser they are in it, which is fine for the copy kept in OPFS (that is
/// device-local, see `export_full`) but not for one the user hands around.
/// So: a full copy, minus the same three things `populate_export` drops,
/// then VACUUMed so the removed values aren't left behind in free pages.
#[cfg(target_arch = "wasm32")]
pub fn export_shareable(db: &Connection) -> Result<Vec<u8>> {
    let copy = deserialize_bytes(&serialize_conn(db)?)?;
    let mut edits: Vec<(String, Option<String>)> = Vec::new();
    {
        let mut sel = copy.prepare("SELECT key, value FROM uapp_config")?;
        let mut rows = sel.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            if key.starts_with("approvals.") {
                edits.push((key, None));
                continue;
            }
            if key != "ai" && key != "mcp" {
                continue;
            }
            let raw: String = row.get(1)?;
            let Ok(mut v) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if let Some(o) = v.as_object_mut() {
                o.remove("api_key");
            }
            if let Some(servers) = v.as_array_mut() {
                for srv in servers.iter_mut().filter_map(|s| s.as_object_mut()) {
                    srv.remove("headers");
                }
            }
            edits.push((key, Some(v.to_string())));
        }
    }
    for (key, value) in edits {
        match value {
            Some(v) => copy.execute(
                "UPDATE uapp_config SET value=?2 WHERE key=?1",
                rusqlite::params![key, v],
            )?,
            None => copy.execute("DELETE FROM uapp_config WHERE key=?1", [key])?,
        };
    }
    copy.execute_batch("VACUUM")?;
    serialize_conn(&copy)
}

#[cfg(target_arch = "wasm32")]
pub fn export_template(mem: &Connection, app_name: &str) -> Result<Vec<u8>> {
    let out = Connection::open_in_memory()?;
    populate_export(&out, mem, app_name, false, false)?;
    serialize_conn(&out)
}

// ---- App-triggered tool approval storage -----------------------------------

/// Check if a tool has been approved for app-triggered calls.
/// Returns true if the tool is in the app's approval list.
pub fn check_app_approval(conn: &Connection, app_id: &str, tool: &str) -> bool {
    let key = format!("approvals.app.{}", app_id);
    match config_get(conn, &key) {
        Ok(Some(Value::Array(approvals))) => approvals.iter().any(|v| v.as_str() == Some(tool)),
        _ => false,
    }
}

/// Add a tool to an app's approval list (user clicked "always allow").
pub fn add_app_approval(conn: &Connection, app_id: &str, tool: &str) -> Result<()> {
    let key = format!("approvals.app.{}", app_id);
    let mut approvals: Vec<String> = match config_get(conn, &key)? {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    if !approvals.iter().any(|t| t == tool) {
        approvals.push(tool.to_string());
    }
    let value = serde_json::to_string(&approvals)?;
    conn.execute(
        "INSERT OR REPLACE INTO uapp_config(key,value) VALUES(?1,?2)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 100-byte plain-SQLite header claiming `pages` pages of `page` bytes.
    fn header(page: u16, pages: u32) -> Vec<u8> {
        let mut h = vec![0u8; 100];
        h[..16].copy_from_slice(b"SQLite format 3\0");
        h[16..18].copy_from_slice(&page.to_be_bytes());
        h[24..28].copy_from_slice(&7u32.to_be_bytes()); // change counter
        h[28..32].copy_from_slice(&pages.to_be_bytes());
        h[92..96].copy_from_slice(&7u32.to_be_bytes()); // version-valid-for
        h
    }

    /// A bootstrapped in-memory app with the live connection's hardening.
    fn app() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        bootstrap_conn(&c, "t").unwrap();
        harden(&c).unwrap();
        c
    }

    fn op(kind: &str, payload: Value) -> Op {
        Op {
            dev: "d".into(),
            ts: 1,
            user: "u".into(),
            kind: kind.into(),
            payload,
        }
    }

    /// One exec/batch the way the engine runs it: inside BEGIN IMMEDIATE,
    /// committed on success and rolled back on failure.
    fn write(c: &Connection, kind: &str, sql: &str) -> Result<Value> {
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let r = apply_op(c, &op(kind, json!({"sql": sql, "params": []})));
        match &r {
            Ok(_) => c.execute_batch("COMMIT").unwrap(),
            Err(_) => c.execute_batch("ROLLBACK").unwrap(),
        }
        r
    }

    fn err_of<T: std::fmt::Debug>(r: Result<T>) -> String {
        format!("{:#}", r.expect_err("should be refused"))
    }

    #[test]
    fn sandbox_refuses_dangerous_pragmas_in_queries() {
        let c = app();
        for sql in [
            "PRAGMA rekey='x'",
            "PRAGMA key='x'",
            "PRAGMA synchronous=OFF",
            "PRAGMA journal_mode=WAL",
            "PRAGMA writable_schema=ON",
            "PRAGMA user_version=5",
        ] {
            let e = err_of(query(&c, sql, &[]));
            assert!(e.contains("PRAGMA"), "{sql}: {e}");
            assert!(!e.contains("not authorized"), "{sql}: says why ({e})");
        }
        let jm: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm, "memory", "journal mode untouched");
        // The schema-inspection PRAGMAs the SQL panel and the assistant use.
        for sql in [
            "PRAGMA table_info(uapp_meta)",
            "PRAGMA table_xinfo('sqlar')",
            "PRAGMA index_list(sqlar)",
            "PRAGMA foreign_key_list(uapp_chat)",
            "PRAGMA integrity_check",
            "PRAGMA user_version",
            "SELECT count(*) FROM pragma_table_info('uapp_chat')",
            "SELECT value FROM uapp_meta WHERE key='app_id'",
        ] {
            query(&c, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e:#}"));
        }
    }

    #[test]
    fn sandbox_refuses_attach_and_vacuum_into() {
        let c = app();
        let dir = std::env::temp_dir().join(format!("uapp-sbx-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("other.db");
        let out = dir.join("copy.db");
        let attach = format!("ATTACH '{}' AS o", other.display());
        assert!(err_of(query(&c, &attach, &[])).contains("approval"));
        assert!(err_of(write(&c, "sql", &attach)).contains("ATTACH"));
        let vac = format!("VACUUM INTO '{}'", out.display());
        assert!(query(&c, &vac, &[]).is_err());
        assert!(write(&c, "batch", &format!("COMMIT; {vac}; BEGIN;")).is_err());
        assert!(!out.exists(), "nothing written outside the app");

        // The approved path: ATTACH works, reads of it work, writes don't,
        // and the next unapproved ATTACH is refused again.
        {
            let o = Connection::open(&other).unwrap();
            o.execute_batch("CREATE TABLE ext(v); INSERT INTO ext VALUES (7);")
                .unwrap();
        }
        query_allow_attach(&c, &attach, &[]).unwrap();
        let r = query(&c, "SELECT v FROM o.ext", &[]).unwrap();
        assert_eq!(r["rows"][0][0], json!(7));
        assert!(err_of(write(&c, "sql", "INSERT INTO o.ext VALUES (8)")).contains("attached"));
        let again = format!("ATTACH '{}' AS p", other.display());
        assert!(query(&c, &again, &[]).is_err());
        query_allow_attach(&c, "DETACH o", &[]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_keeps_batches_atomic() {
        let c = app();
        write(&c, "batch", "CREATE TABLE t(x)").unwrap();
        let e = err_of(write(
            &c,
            "batch",
            "INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2);",
        ));
        assert!(e.contains("COMMIT"), "{e}");
        let n: i64 = c
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the whole batch rolled back");
        for sql in ["BEGIN", "ROLLBACK", "SAVEPOINT s", "END"] {
            assert!(write(&c, "batch", sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn sandbox_protects_uapp_tables() {
        let c = app();
        for sql in [
            "INSERT INTO uapp_config(key,value) VALUES('ai','{}')",
            "UPDATE uapp_meta SET value='x' WHERE key='app_id'",
            "DELETE FROM uapp_chat",
            "UPDATE sqlar SET data=NULL",
            "DELETE FROM SQLAR",
            "DROP TABLE uapp_file_history",
            "ALTER TABLE uapp_chat ADD COLUMN z",
            "CREATE TABLE uapp_extra(x)",
            "CREATE TEMP TABLE uapp_meta(key, value)",
            "CREATE INDEX i ON uapp_chat(ts)",
            "CREATE TRIGGER tr AFTER INSERT ON sqlar BEGIN SELECT 1; END",
            "CREATE VIEW uapp_v AS SELECT 1",
        ] {
            let e = err_of(write(&c, "batch", sql));
            assert!(
                e.contains("uApp's own tables") || e.contains("sqlar holds"),
                "{sql}: {e}"
            );
        }
        // A trigger on an app table can't reach them either.
        write(
            &c,
            "batch",
            "CREATE TABLE notes(t);
             CREATE TRIGGER sneak AFTER INSERT ON notes BEGIN
               INSERT OR REPLACE INTO uapp_config(key,value) VALUES('ai','{}');
             END;",
        )
        .unwrap();
        assert!(write(&c, "sql", "INSERT INTO notes VALUES ('hi')").is_err());
        assert_eq!(config_get(&c, "ai").unwrap(), None);
        // Reads stay allowed, and ordinary app SQL is unaffected.
        query(&c, "SELECT * FROM uapp_config", &[]).unwrap();
        write(
            &c,
            "batch",
            "DROP TRIGGER sneak; INSERT INTO notes VALUES ('ok'); \
             CREATE TABLE seq(id INTEGER PRIMARY KEY AUTOINCREMENT, v); \
             INSERT INTO seq(v) VALUES (1); ALTER TABLE notes RENAME TO notes2; \
             CREATE INDEX n2 ON notes2(t); ANALYZE; DROP TABLE seq;",
        )
        .unwrap();
        // foreign_keys used to be a silent no-op inside the write transaction;
        // it still is (rather than an error that breaks an app's startup).
        write(&c, "sql", "PRAGMA foreign_keys = ON").unwrap();
        // Internal code still writes uApp's tables directly.
        c.execute_batch("BEGIN").unwrap();
        apply_op(&c, &op("app_rename", json!({"name": "Renamed"}))).unwrap();
        c.execute_batch("COMMIT").unwrap();
        assert_eq!(meta_get(&c, "name").unwrap().as_deref(), Some("Renamed"));
    }

    #[test]
    fn sandbox_allows_fts5_and_generated_columns() {
        let c = app();
        write(
            &c,
            "batch",
            "CREATE VIRTUAL TABLE docs USING fts5(body);
             INSERT INTO docs(body) VALUES ('hello world'), ('other');
             CREATE TABLE g(a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2));
             INSERT INTO g(a) VALUES (21);",
        )
        .unwrap();
        let r = query(&c, "SELECT body FROM docs WHERE docs MATCH 'hello'", &[]).unwrap();
        assert_eq!(r["rows"][0][0], json!("hello world"));
        let r = query(&c, "SELECT b FROM g", &[]).unwrap();
        assert_eq!(r["rows"][0][0], json!(42));
        // FTS5's own tables are off limits to plain SQL (DEFENSIVE).
        assert!(write(&c, "sql", "DELETE FROM docs_data").is_err());
    }

    fn header_of(c: &Connection) -> (i32, i64) {
        (
            c.query_row("PRAGMA application_id", [], |r| r.get(0))
                .unwrap(),
            c.query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
        )
    }

    #[test]
    fn new_files_are_stamped() {
        let c = app();
        assert_eq!(header_of(&c), (APPLICATION_ID, FORMAT_VERSION));
        assert_eq!(
            meta_get(&c, "format_version").unwrap(),
            Some(FORMAT_VERSION.to_string())
        );
        // Opening again changes nothing.
        let changes = c.total_changes();
        bootstrap_conn(&c, "t").unwrap();
        assert_eq!(
            c.total_changes(),
            changes,
            "an up-to-date file isn't written"
        );
    }

    #[test]
    fn unstamped_files_are_upgraded() {
        // A file from before format versioning: no sz/session columns, the
        // old sync table, a pre-split file name, and a user_version the app
        // set for itself.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
             CREATE TABLE uapp_meta(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE uapp_config(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE uapp_chat(id INTEGER PRIMARY KEY AUTOINCREMENT, mid TEXT UNIQUE,
               ts INTEGER, ctr INTEGER, device TEXT, user TEXT, role TEXT, content TEXT);
             CREATE TABLE uapp_file_history(id INTEGER PRIMARY KEY AUTOINCREMENT,
               name TEXT, ts INTEGER, device TEXT, user TEXT, action TEXT, data BLOB);
             CREATE TABLE uapp_applied(dev TEXT);
             INSERT INTO uapp_meta VALUES ('app_id','old'),('name','Old'),('format_version','1');
             INSERT INTO sqlar VALUES ('index.html', 420, 0, 2, X'6869');
             PRAGMA user_version = 7;",
        )
        .unwrap();
        bootstrap_conn(&c, "Old").unwrap();
        assert_eq!(header_of(&c), (APPLICATION_ID, FORMAT_VERSION));
        assert!(has_column(&c, "uapp_chat", "session").unwrap());
        assert!(has_column(&c, "uapp_file_history", "sz").unwrap());
        let applied: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='uapp_applied'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, 0);
        assert_eq!(
            sqlar_read(&c, "app/index.html").unwrap().as_deref(),
            Some(&b"hi"[..])
        );
        assert_eq!(meta_get(&c, "app_id").unwrap().as_deref(), Some("old"));
        assert_eq!(
            meta_get(&c, "app_user_version").unwrap().as_deref(),
            Some("7"),
            "the app's own user_version is kept"
        );
    }

    #[test]
    fn newer_and_foreign_files_are_refused() {
        // Stamped by a newer uApp.
        let c = app();
        c.pragma_update(None, "user_version", FORMAT_VERSION + 1)
            .unwrap();
        let e = err_of(bootstrap_conn(&c, "t"));
        assert!(e.contains("newer uApp") && e.contains("Update uApp"), "{e}");
        // Unstamped, but uapp_meta says so.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE uapp_meta(key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO uapp_meta VALUES ('format_version','99');",
        )
        .unwrap();
        assert!(err_of(bootstrap_conn(&c, "t")).contains("newer uApp"));
        let tables: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "nothing was created in the refused file");
        // Some other program's SQLite file.
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "application_id", 0x4750_4b47)
            .unwrap();
        assert!(err_of(bootstrap_conn(&c, "t")).contains("another program"));
    }

    #[test]
    fn a_failed_migration_changes_nothing() {
        // uapp_chat as a view: adding its session column has to fail.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE uapp_meta(key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO uapp_meta VALUES ('app_id','x');
             CREATE VIEW uapp_chat AS SELECT 1 AS id;",
        )
        .unwrap();
        assert!(err_of(bootstrap_conn(&c, "t")).contains("upgrading the file to format 1"));
        let tables: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('sqlar','uapp_config')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "the half-done upgrade was rolled back");
        assert_eq!(header_of(&c), (0, 0));
    }

    #[test]
    fn inflation_is_capped() {
        let small = sqlar_deflate(&[0u8; 4096]).unwrap();
        let e = err_of(sqlar_inflate(Some(1 << 40), small.clone(), "app/bomb.bin"));
        assert!(e.contains("app/bomb.bin") && e.contains("512 MiB"), "{e}");
        assert_eq!(
            sqlar_inflate(Some(4096), small, "ok").unwrap(),
            vec![0u8; 4096]
        );
    }

    #[test]
    fn schema_sql_must_be_one_create() {
        let c = app();
        assert_eq!(
            leading_keywords("  -- x\n/* y */ CREATE  virtual TABLE", 2),
            ["create", "virtual"]
        );
        assert!(only_trivia("  ; -- done\n /* end */ ;"));
        assert!(!only_trivia("; ATTACH 'x' AS y"));
        for (typ, sql) in [
            ("table", "ATTACH 'x' AS y"),
            ("table", "CREATE VIEW v AS SELECT 1"),
            ("view", "CREATE TEMP VIEW v AS SELECT 1"),
            ("index", "DROP TABLE sqlar"),
            ("table", "CREATE TABLE t(x); PRAGMA rekey='x'"),
        ] {
            let e = err_of(exec_schema_sql(&c, typ, "obj", sql));
            assert!(e.contains("tampered"), "{sql}: {e}");
        }
        let e = err_of(exec_schema_sql(
            &c,
            "table",
            "obj",
            "CREATE TABLE uapp_config2(x)",
        ));
        assert!(e.contains("uApp's own tables"), "{e}");
        exec_schema_sql(&c, "table", "t", "CREATE TABLE t(x) ; -- fine").unwrap();
        exec_schema_sql(&c, "index", "i", "CREATE UNIQUE INDEX i ON t(x)").unwrap();
    }

    #[test]
    fn whole_file_check() {
        assert!(check_whole_bytes(&[], 0).is_err(), "empty");
        assert!(check_whole_bytes(&header(4096, 3), 3 * 4096).is_ok());
        assert!(
            check_whole_bytes(&header(4096, 3), 3 * 4096 + 10).is_ok(),
            "trailing bytes are harmless"
        );
        assert!(
            check_whole_bytes(&header(4096, 3), 2 * 4096).is_err(),
            "cut off"
        );
        assert!(
            check_whole_bytes(&header(4096, 0), 100).is_err(),
            "less than one page"
        );
        assert!(
            check_whole_bytes(&header(1000, 1), 4096).is_err(),
            "bad page size"
        );
        // Stale in-header count (counter != valid-for): only the page floor applies.
        let mut stale = header(4096, 9);
        stale[92] = 1;
        assert!(check_whole_bytes(&stale, 4096).is_ok());
        // Encrypted: whole pages only.
        let enc = [0x5au8; 100];
        assert!(check_whole_bytes(&enc, 8192).is_ok());
        assert!(check_whole_bytes(&enc, 8000).is_err());
        assert!(check_whole_bytes(&enc[..10], 512).is_err());
    }

    #[test]
    fn whole_file_check_real_file() {
        let dir = std::env::temp_dir().join(format!("uapp-whole-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.uapp");
        bootstrap(&p, "a", None).unwrap();
        assert!(check_whole_file(&p).is_ok());
        let len = std::fs::metadata(&p).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(len / 2).unwrap();
        assert!(check_whole_file(&p).is_err(), "half a file");
        f.set_len(0).unwrap();
        assert!(check_whole_file(&p).is_err(), "zero bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
