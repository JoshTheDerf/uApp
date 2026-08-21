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

pub const FORMAT_VERSION: i64 = 1;

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

/// Create/complete the schema in a possibly-empty .uapp file. Handles the
/// "file manager created a 0-byte file from a template" case. With a master
/// password the file is (or becomes) SQLCipher-encrypted.
#[cfg(not(target_arch = "wasm32"))]
pub fn bootstrap(path: &Path, app_name: &str, key: Option<&str>) -> Result<()> {
    // The WHOLE body is retried on a transient sharing/lock violation: on
    // Windows, creating/writing the file trips real-time AV (Defender), which
    // briefly locks it — and SQLite's busy_timeout does NOT cover an OS-level
    // lock (it's not SQLite's own locking protocol), so a mid-write statement
    // can fail with ERROR_LOCK_VIOLATION. Every statement here is idempotent
    // (CREATE IF NOT EXISTS / INSERT OR REPLACE / guarded inserts), so simply
    // re-running the whole thing is safe.
    with_open_retry(|| {
        let conn = match key {
            Some(p) => crate::cipher::open_file(path, p)?,
            None => Connection::open(path)?,
        };
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        bootstrap_conn(&conn, app_name)
    })
    .with_context(|| format!("bootstrapping {}", path.display()))
}

/// The platform-independent part of bootstrap: create/complete the schema and
/// seed a brand-new app, on an already-open connection. Idempotent.
pub fn bootstrap_conn(conn: &Connection, app_name: &str) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate(conn);
    let has_meta: i64 = conn.query_row(
        "SELECT count(*) FROM uapp_meta WHERE key='app_id'",
        [],
        |r| r.get(0),
    )?;
    if has_meta == 0 {
        let app_id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        conn.execute(
            "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES
             ('app_id',?1),('name',?2),('format_version',?3),('created',?4)",
            rusqlite::params![
                app_id,
                app_name,
                FORMAT_VERSION.to_string(),
                now_ms().to_string()
            ],
        )?;
    }
    // migrate() above has already moved any pre-split `index.html` here.
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
    // Durable before anyone opens it.
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

/// Columns added to existing tables after the fact. Every statement is a
/// best-effort `ALTER TABLE` — it fails harmlessly when the column is already
/// there (new files get it from SCHEMA), so this runs on every open of a
/// checkpoint AND on every materialized snapshot (SCHEMA's
/// `CREATE TABLE IF NOT EXISTS` can't add columns to a table that exists).
fn migrate(conn: &Connection) {
    // sz on uapp_file_history (added when history snapshots became
    // sqlar-compressed). Old rows keep sz NULL = uncompressed.
    let _ = conn.execute("ALTER TABLE uapp_file_history ADD COLUMN sz INT", []);
    // session on uapp_chat (multi-chat). NULL == 'main' everywhere.
    let _ = conn.execute("ALTER TABLE uapp_chat ADD COLUMN session TEXT", []);
    // The old sync engine's per-device watermark table — gone with it.
    let _ = conn.execute("DROP TABLE IF EXISTS uapp_applied", []);
    // app/ + data/ split: fold pre-split archive names into the two roots.
    if let Err(e) = migrate_paths(conn) {
        eprintln!("uapp: could not migrate archive paths: {e:#}");
    }
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
    for cand in [canon_name(&n), format!("{APP_DIR}{bare}"), format!("{DATA_DIR}{bare}")] {
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

/// One-time move of an archive written before the app/data split: legacy names
/// go to their canonical home, honouring (and then dropping) the old
/// `file_roles` overrides so a seed dataset someone had marked "app" stays app
/// code. Deterministic and idempotent — it runs on every device at open, and
/// op replay re-normalizes anything that arrives later (see `apply_op`), so
/// every replica converges on the same layout.
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
        let mut stmt =
            conn.prepare("SELECT DISTINCT name FROM uapp_file_history ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for old in names.iter().chain(hnames.iter().filter(|h| !names.contains(h))) {
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

#[cfg(not(target_arch = "wasm32"))]
/// Build a template .uapp from the live app: everything under `app/` (copied
/// verbatim, still sqlar-compressed) + empty user tables (schema only) +
/// config minus the API key. No user data: no `data/` files, no table rows, no
/// chat, no file history, and a fresh app_id so the template is a new lineage.
pub fn export_template(mem: &Connection, app_name: &str) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!(
        "uapp-template-{}.uapp",
        (0..10).map(|_| fastrand::alphanumeric()).collect::<String>()
    ));
    let build = (|| -> Result<()> {
        let out = Connection::open(&tmp)?;
        out.pragma_update(None, "journal_mode", "DELETE")?;
        out.execute_batch(SCHEMA)?;
        let app_id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
        out.execute(
            "INSERT INTO uapp_meta(key,value) VALUES
             ('app_id',?1),('name',?2),('format_version',?3),('created',?4)",
            rusqlite::params![
                app_id,
                app_name,
                FORMAT_VERSION.to_string(),
                now_ms().to_string()
            ],
        )?;
        // App files, stored form copied verbatim (compression preserved).
        {
            let mut sel = mem.prepare("SELECT name, mode, mtime, sz, data FROM sqlar")?;
            let mut ins = out.prepare(
                "INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?1,?2,?3,?4,?5)",
            )?;
            let mut rows = sel.query([])?;
            while let Some(row) = rows.next()? {
                let name: String = row.get(0)?;
                if file_role(&name) != "app" {
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
        // User tables: schema only, no rows (tables first, then indexes etc.).
        for phase in ["type = 'table'", "type != 'table'"] {
            let mut sel = mem.prepare(&format!(
                "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND {phase}
                 AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'uapp_%' AND name != 'sqlar'
                 ORDER BY rowid"
            ))?;
            let mut rows = sel.query([])?;
            while let Some(row) = rows.next()? {
                let sql: String = row.get(0)?;
                out.execute_batch(&sql)?;
            }
        }
        // Config: everything except secrets — the ai entry is copied minus
        // api_key so the template keeps the provider/model choice.
        {
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
        }
        out.pragma_update(None, "synchronous", "FULL")?;
        drop(out);
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
        (0..12).map(|_| fastrand::alphanumeric()).collect::<String>()
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
/// A non-empty file that isn't plain SQLite: an encrypted .uapp (SQLCipher) that
/// needs a master password before it can be opened. Used to decide whether to
/// show the unlock prompt.
pub fn is_encrypted_file(path: &Path) -> bool {
    !path.is_dir()
        && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
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
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, json_to_sql(p)?)?;
    }
    Ok(())
}

/// First SQL keyword, skipping whitespace and -- / /* */ comments.
fn first_keyword(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if sql[i..].starts_with("--") {
            i += sql[i..].find('\n').map(|n| n + 1).unwrap_or(sql.len() - i);
        } else if sql[i..].starts_with("/*") {
            match sql[i..].find("*/") { Some(n) => i += n + 2, None => return String::new() }
        } else {
            break;
        }
    }
    sql[i..].chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase()
}

/// Is this statement an ATTACH or DETACH? SQLite reports both as "read-only",
/// but they open arbitrary local database files.
pub fn is_attach_stmt(sql: &str) -> bool {
    let kw = first_keyword(sql);
    kw == "attach" || kw == "detach"
}

/// Word-boundary scan of a whole (possibly multi-statement) SQL text for
/// ATTACH/DETACH — coarse (a string literal can false-positive), but a false
/// positive is a clear error rather than a data leak.
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

/// Run a read-only query against the materialized db. ATTACH/DETACH are
/// refused here; callers that obtained explicit user approval use
/// query_allow_attach instead.
pub fn query(conn: &Connection, sql: &str, params: &[Value]) -> Result<Value> {
    if is_attach_stmt(sql) {
        bail!("ATTACH/DETACH opens another database file on this machine and needs explicit user approval — ask via the assistant, which prompts for permission");
    }
    query_allow_attach(conn, sql, params)
}

/// query() without the ATTACH refusal — only for calls the user explicitly
/// approved (the assistant's permission prompt).
pub fn query_allow_attach(conn: &Connection, sql: &str, params: &[Value]) -> Result<Value> {
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

/// Apply one op to the materialized db. Deterministic: given the same op
/// sequence every device computes identical state. Caller wraps in a
/// SAVEPOINT so a failing op leaves no partial effects.
pub fn apply_op(conn: &Connection, op: &Op) -> Result<Value> {
    let p = &op.payload;
    match op.kind.as_str() {
        "sql" => {
            let sql = p["sql"].as_str().ok_or_else(|| anyhow!("sql op missing sql"))?;
            // ATTACH would open (or create) an arbitrary local file with the
            // app's privileges — never allowed from app-originated writes.
            if is_attach_stmt(sql) {
                bail!("ATTACH/DETACH can't run in app writes — read the other database with an approved read-only query and inline the values instead");
            }
            let params: Vec<Value> = p["params"].as_array().cloned().unwrap_or_default();
            let mut stmt = conn.prepare(sql)?;
            bind_params(&mut stmt, &params)?;
            // Statements that produce columns (INSERT/UPDATE/DELETE ...
            // RETURNING, or a stray SELECT) return their rows, so callers can
            // read back e.g. the new id in the SAME statement — no
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
                return Ok(json!({"changes": rows_out.len(), "columns": cols, "rows": rows_out}));
            }
            let n = stmt.raw_execute()?;
            drop(stmt);
            // Rowid of the row this write created (0 = none: UPDATE/DELETE/DDL
            // or a table without a rowid alias). Lets actions do
            // `const {insert_id} = await uapp.exec("INSERT ...")`.
            let iid = conn.last_insert_rowid();
            Ok(json!({"changes": n, "insert_id": if iid > 0 { json!(iid) } else { Value::Null } }))
        }
        "batch" => {
            let sql = p["sql"].as_str().ok_or_else(|| anyhow!("batch op missing sql"))?;
            if mentions_attach(sql) {
                bail!("ATTACH/DETACH can't run in app writes — read the other database with an approved read-only query and inline the values instead");
            }
            conn.execute_batch(sql)?;
            Ok(json!({"ok": true}))
        }
        "import" => {
            // One op = one whole dataset. {table, columns, rows, create?, truncate?}
            let table = p["table"].as_str().ok_or_else(|| anyhow!("import missing table"))?;
            if !table.chars().all(|c| c.is_alphanumeric() || c == '_') {
                bail!("import table name must be alphanumeric/underscore");
            }
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
            if cols.iter().any(|c| !c.chars().all(|ch| ch.is_alphanumeric() || ch == '_')) {
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
            let rows = p["rows"].as_array().ok_or_else(|| anyhow!("import missing rows"))?;
            for row in rows {
                let vals = row.as_array().ok_or_else(|| anyhow!("import row not array"))?;
                bind_params(&mut stmt, vals)?;
                stmt.raw_execute()?;
            }
            Ok(json!({"inserted": rows.len()}))
        }
        // Names are normalized here, not at the caller: ops written before the
        // app/data split (or by an app using root-absolute paths) must replay
        // into the same canonical location on every device.
        "file_put" => {
            let name = p["name"].as_str().ok_or_else(|| anyhow!("file_put missing name"))?;
            let name = canon_name(name);
            let data = base64::engine::general_purpose::STANDARD
                .decode(p["data"].as_str().unwrap_or_default())?;
            record_file_history(conn, op, &name, "put")?;
            sqlar_write(conn, &name, (op.ts / 1000) as i64, &data)?;
            Ok(json!({"ok": true, "bytes": data.len()}))
        }
        "file_del" => {
            let name = p["name"].as_str().ok_or_else(|| anyhow!("file_del missing name"))?;
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
            crate::template::apply(conn, src.conn(), op, p["remove_stale"].as_bool().unwrap_or(true))
        }
        "config_set" => {
            let key = p["key"].as_str().ok_or_else(|| anyhow!("config_set missing key"))?;
            conn.execute(
                "INSERT OR REPLACE INTO uapp_config(key,value) VALUES(?1,?2)",
                rusqlite::params![key, p["value"].to_string()],
            )?;
            Ok(json!({"ok": true}))
        }
        "chat" => {
            let mid = p["mid"].as_str().ok_or_else(|| anyhow!("chat missing mid"))?;
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
                    // Old logs carry no session — they replay into 'main'.
                    p["session"].as_str().unwrap_or("main")
                ],
            )?;
            Ok(json!({"ok": true}))
        }
        "chat_clear" => {
            // No session in the payload = clear every session (old logs).
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
        // the payload overwrite; `created` is carried in the payload (never
        // read from the clock) so replay is deterministic.
        "session_meta" => {
            let id = p["id"].as_str().ok_or_else(|| anyhow!("session_meta missing id"))?;
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
                    p["archived"].as_bool().map(|b| b as i64).or_else(|| p["archived"].as_i64())
                ],
            )?;
            Ok(json!({"ok": true}))
        }
        // Drop a session and its messages. 'main' is implicit in the UI, so
        // deleting it just empties it.
        "session_del" => {
            let id = p["id"].as_str().ok_or_else(|| anyhow!("session_del missing id"))?;
            conn.execute("DELETE FROM uapp_sessions WHERE id=?1", rusqlite::params![id])?;
            conn.execute(
                "DELETE FROM uapp_chat WHERE COALESCE(session,'main')=?1",
                rusqlite::params![id],
            )?;
            Ok(json!({"ok": true}))
        }
        // Replace everything up to (upto_ts, upto_ctr) in one session with a
        // single summary row. The summary TEXT is computed by the caller before
        // the op is emitted — replay never calls a model.
        "chat_compact" => {
            let session = p["session"].as_str().unwrap_or("main");
            let mid = p["mid"].as_str().ok_or_else(|| anyhow!("chat_compact missing mid"))?;
            let upto_ts = p["upto_ts"].as_i64().ok_or_else(|| anyhow!("chat_compact missing upto_ts"))?;
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
            // the messages it summarizes). Both values come from the payload,
            // so replay stays deterministic.
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
/// compression level so op replay produces identical bytes on every device.
fn sqlar_deflate(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::new(6),
    );
    enc.write_all(data)?;
    let out = enc.finish()?;
    Ok(if out.len() < data.len() { out } else { data.to_vec() })
}

/// Insert/replace one file row, sqlar-style (zlib-compressed when smaller).
/// True when the app still has the default, un-customized index.html — i.e. a
/// "new" app that hasn't been built yet. Drives the initial chat-sidebar state
/// (open for a new app, closed once it's a real app).
pub fn is_blank_app(conn: &Connection) -> bool {
    match sqlar_read(conn, "app/index.html") {
        Ok(Some(data)) => data == DEFAULT_INDEX_HTML.as_bytes(),
        _ => true, // no index at all → also "new"
    }
}

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

/// sqlar_uncompress(X, SZ) semantics for any (sz, data) pair: sz NULL/<=0 or
/// sz == length(data) means the blob is the content as-is; smaller data is
/// zlib-inflated to exactly sz bytes.
pub fn sqlar_inflate(sz: Option<i64>, data: Vec<u8>, what: &str) -> Result<Vec<u8>> {
    let Some(sz) = sz else { return Ok(data) };
    if sz <= 0 || sz == data.len() as i64 {
        return Ok(data);
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
        bail!("sqlar entry {what} is corrupt: expected {sz} bytes, inflated to {}", out.len());
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

#[cfg(target_arch = "wasm32")]
pub fn export_template(mem: &Connection, app_name: &str) -> Result<Vec<u8>> {
    let out = Connection::open_in_memory()?;
    out.execute_batch(SCHEMA)?;
    let app_id: String = (0..16).map(|_| fastrand::alphanumeric()).collect();
    out.execute(
        "INSERT INTO uapp_meta(key,value) VALUES
         ('app_id',?1),('name',?2),('format_version',?3),('created',?4)",
        rusqlite::params![app_id, app_name, FORMAT_VERSION.to_string(), now_ms().to_string()],
    )?;
    // App files, stored form copied verbatim (compression preserved).
    {
        let mut sel = mem.prepare("SELECT name, mode, mtime, sz, data FROM sqlar")?;
        let mut ins =
            out.prepare("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?1,?2,?3,?4,?5)")?;
        let mut rows = sel.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            if file_role(&name) != "app" {
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
    // User tables: schema only, no rows (tables first, then indexes etc.).
    for phase in ["type = 'table'", "type != 'table'"] {
        let mut sel = mem.prepare(&format!(
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND {phase}
             AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'uapp_%' AND name != 'sqlar'
             ORDER BY rowid"
        ))?;
        let mut rows = sel.query([])?;
        while let Some(row) = rows.next()? {
            let sql: String = row.get(0)?;
            out.execute_batch(&sql)?;
        }
    }
    // Config: everything except secrets.
    {
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
    }
    serialize_conn(&out)
}

// ---- App-triggered tool approval storage -----------------------------------

/// Check if a tool has been approved for app-triggered calls.
/// Returns true if the tool is in the app's approval list.
pub fn check_app_approval(conn: &Connection, app_id: &str, tool: &str) -> bool {
    let key = format!("approvals.app.{}", app_id);
    match config_get(conn, &key) {
        Ok(Some(Value::Array(approvals))) => {
            approvals.iter().any(|v| v.as_str() == Some(tool))
        }
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
