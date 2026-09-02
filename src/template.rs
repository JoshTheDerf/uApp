//! Updating a live app from a template .uapp — dropping a new version of the
//! app on top of the data someone already has.
//!
//! A template (`store::export_template`) carries app code plus an empty
//! schema; the file it lands on carries the user's content. So an update is
//! deliberately one-directional and additive:
//!
//!   * `app/` files are taken from the template (added, replaced, and — when
//!     asked — the ones the template no longer has are removed). Every write
//!     goes through the normal file-history recorder, so any of it can be
//!     reverted from the file browser afterwards.
//!   * user tables are reconciled ADDITIVELY: missing tables are created,
//!     missing columns are added. Nothing is ever dropped or retyped, because
//!     that is where the rows live. Views/indexes/triggers hold no data, so
//!     they are replaced outright when the template's definition differs.
//!   * `data/` files, table rows, chat, file history, the app's `app_id` and
//!     its name are never touched — the app keeps its identity and its data,
//!     which is the whole point.
//!
//! Anything that can't be reconciled (a new PRIMARY KEY column, a changed
//! column type) is reported as a warning rather than forced: the user sees it
//! in the confirmation dialog and can fix it in the SQL panel.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::store::{self, Op};

/// Quote an identifier for use in DDL.
fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Compare two DDL statements while ignoring the formatting the writer chose.
fn norm_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_string()
}

// ---- the opened source ------------------------------------------------------

/// A template .uapp opened for reading.
///
/// Native: the bytes are written to a temp file and opened from there (the
/// file is removed when this is dropped) — the same trick the exports use, and
/// it keeps SQLCipher available for an encrypted source. wasm: deserialized
/// into an in-memory database, like the engine's own `open_from_bytes`.
pub struct Source {
    conn: Connection,
    #[cfg(not(target_arch = "wasm32"))]
    tmp: Option<std::path::PathBuf>,
}

impl Source {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The template's own name/app_id — what the confirmation dialog shows.
    pub fn describe(&self) -> Value {
        json!({
            "name": store::meta_get(&self.conn, "name").ok().flatten().unwrap_or_default(),
            "appId": store::meta_get(&self.conn, "app_id").ok().flatten().unwrap_or_default(),
            "created": store::meta_get(&self.conn, "created").ok().flatten().unwrap_or_default(),
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_bytes(bytes: &[u8], password: Option<&str>) -> Result<Source> {
        if bytes.len() < 16 {
            bail!("that file is empty (or far too small to be a .uapp)");
        }
        let tmp = std::env::temp_dir().join(format!(
            "uapp-tplsrc-{}.uapp",
            (0..10).map(|_| fastrand::alphanumeric()).collect::<String>()
        ));
        let opened = (|| -> Result<Connection> {
            std::fs::write(&tmp, bytes)?;
            // Templates are always plaintext (export_template writes an
            // unencrypted file); a FULL copy of an encrypted app is not, and
            // then we need its master password to read the app code at all.
            let conn = if &bytes[..15] == b"SQLite format 3" {
                Connection::open(&tmp)?
            } else {
                match password {
                    Some(pw) => crate::cipher::open_file(&tmp, pw)?,
                    None => bail!(
                        "that file isn't a plain SQLite database — if it's an ENCRYPTED \
                         .uapp, decrypt it (or supply its master password) and try again"
                    ),
                }
            };
            validate(&conn)?;
            Ok(conn)
        })();
        match opened {
            Ok(conn) => Ok(Source { conn, tmp: Some(tmp) }),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn from_bytes(bytes: &[u8], _password: Option<&str>) -> Result<Source> {
        let conn = store::deserialize_bytes(bytes)?;
        validate(&conn)?;
        Ok(Source { conn })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Source {
    fn drop(&mut self) {
        // Close the connection before removing the file (Windows).
        let dead = Connection::open_in_memory();
        if let Ok(dead) = dead {
            drop(std::mem::replace(&mut self.conn, dead));
        }
        if let Some(tmp) = self.tmp.take() {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

/// Is this really a .uapp we can read? Checked before anything is compared, so
/// dropping a spreadsheet on the window says so instead of failing halfway.
fn validate(conn: &Connection) -> Result<()> {
    let verdict: String = conn
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .context("reading the dropped file")?;
    if verdict != "ok" {
        bail!("that .uapp failed its integrity check: {verdict}");
    }
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE type='table' AND name IN ('sqlar','uapp_meta')",
        [],
        |r| r.get(0),
    )?;
    if tables < 2 {
        bail!("that file is a database, but not a .uapp (no sqlar/uapp_meta tables)");
    }
    if let Some(v) = store::meta_get(conn, "format_version")? {
        if v.trim().parse::<i64>().unwrap_or(0) > store::FORMAT_VERSION {
            bail!(
                "that template was written by a newer uapp (format {v}; this build \
                 understands {}) — update uapp first",
                store::FORMAT_VERSION
            );
        }
    }
    Ok(())
}

// ---- reading the two sides --------------------------------------------------

/// Every app-role file with its content, inflated so two archives compressed
/// by different builds still compare equal.
fn app_files(conn: &Connection) -> Result<BTreeMap<String, Vec<u8>>> {
    files_by_role(conn, &["app"])
}

/// One schema object belonging to the app (never uapp_* / sqlar internals).
struct Obj {
    typ: String,
    name: String,
    sql: String,
}

fn user_objects(conn: &Connection) -> Result<Vec<Obj>> {
    let mut stmt = conn.prepare(
        "SELECT type, name, sql FROM sqlite_master
         WHERE sql IS NOT NULL
           AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'uapp_%' AND name != 'sqlar'
           AND (tbl_name IS NULL OR (tbl_name NOT LIKE 'uapp_%' AND tbl_name != 'sqlar'))
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Obj { typ: r.get(0)?, name: r.get(1)?, sql: r.get(2)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

struct Col {
    name: String,
    ty: String,
    notnull: bool,
    dflt: Option<String>,
    pk: bool,
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<Col>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", q(table)))?;
    let rows = stmt.query_map([], |r| {
        Ok(Col {
            name: r.get(1)?,
            ty: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            notnull: r.get::<_, i64>(3)? != 0,
            dflt: r.get::<_, Option<String>>(4)?,
            pk: r.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The `ALTER TABLE … ADD COLUMN` clause for one of the template's columns,
/// plus a note when it couldn't be carried over exactly. SQLite refuses a
/// PRIMARY KEY column outright, and a NOT NULL column with no default has no
/// value to give the rows that already exist — so that one is added nullable,
/// which keeps the new app code working instead of failing the whole update.
fn add_column(table: &str, c: &Col) -> (Option<String>, Option<String>) {
    if c.pk {
        return (
            None,
            Some(format!(
                "{table}.{}: SQLite can't add a PRIMARY KEY column to an existing table — \
                 add it by hand in the SQL panel if the app needs it",
                c.name
            )),
        );
    }
    let mut decl = q(&c.name);
    if !c.ty.is_empty() {
        decl.push(' ');
        decl.push_str(&c.ty);
    }
    let mut note = None;
    if let Some(d) = &c.dflt {
        decl.push_str(&format!(" DEFAULT {d}"));
    }
    if c.notnull {
        if c.dflt.is_some() {
            decl.push_str(" NOT NULL");
        } else {
            note = Some(format!(
                "{table}.{}: added without NOT NULL — the rows you already have \
                 would have no value for it",
                c.name
            ));
        }
    }
    (Some(format!("ALTER TABLE {} ADD COLUMN {decl}", q(table))), note)
}

// ---- the plan ---------------------------------------------------------------

/// Everything an update would do, computed against the live database. Purely
/// read-only: this is what the confirmation dialog shows, and `apply` executes
/// exactly this.
pub fn plan(target: &Connection, src: &Connection) -> Result<Value> {
    let tpl_files = app_files(src)?;
    let cur_files = app_files(target)?;
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = 0usize;
    for (name, data) in &tpl_files {
        match cur_files.get(name) {
            None => added.push(name.clone()),
            Some(cur) if cur != data => changed.push(name.clone()),
            Some(_) => unchanged += 1,
        }
    }
    // App files this template doesn't have: stale app code (or something added
    // locally). Removing them is the caller's choice — see `apply`.
    let stale: Vec<String> =
        cur_files.keys().filter(|n| !tpl_files.contains_key(*n)).cloned().collect();

    let mut warnings: Vec<String> = Vec::new();
    let src_objs = user_objects(src)?;
    let cur_objs = user_objects(target)?;
    let cur_by_name: BTreeMap<&str, &Obj> =
        cur_objs.iter().map(|o| (o.name.as_str(), o)).collect();

    let mut create_tables: Vec<Value> = Vec::new();
    let mut alters: Vec<Value> = Vec::new();
    let mut create_objs: Vec<Value> = Vec::new();
    let mut replace_objs: Vec<Value> = Vec::new();

    for o in src_objs.iter().filter(|o| o.typ == "table") {
        match cur_by_name.get(o.name.as_str()) {
            None => create_tables.push(json!({"table": o.name, "sql": o.sql})),
            Some(cur) if cur.typ != "table" => warnings.push(format!(
                "the template has a table named {} but this app has a {} with that name — skipped",
                o.name, cur.typ
            )),
            Some(cur) => {
                let have: BTreeSet<String> =
                    columns(target, &o.name)?.into_iter().map(|c| c.name).collect();
                let want = columns(src, &o.name)?;
                let mut missing = 0usize;
                for c in want.iter().filter(|c| !have.contains(&c.name)) {
                    missing += 1;
                    let (sql, note) = add_column(&o.name, c);
                    if let Some(n) = note {
                        warnings.push(n);
                    }
                    if let Some(sql) = sql {
                        alters.push(json!({"table": o.name, "column": c.name, "sql": sql}));
                    }
                }
                // Same columns on both sides but a different definition: a
                // retype, a renamed column, or a new constraint. None of those
                // can be applied without rewriting the table (and its rows),
                // so say so rather than half-doing it.
                if missing == 0 && norm_sql(&cur.sql) != norm_sql(&o.sql) {
                    warnings.push(format!(
                        "table {} is defined differently in the template (constraints or \
                         column types changed) — its rows are kept and the definition is \
                         left alone; migrate it in the SQL panel if the app needs it",
                        o.name
                    ));
                }
            }
        }
    }
    // Views, indexes and triggers hold no rows: replace them outright.
    for o in src_objs.iter().filter(|o| o.typ != "table") {
        match cur_by_name.get(o.name.as_str()) {
            None => create_objs.push(json!({"type": o.typ, "name": o.name, "sql": o.sql})),
            Some(cur) if cur.typ != o.typ => warnings.push(format!(
                "the template's {} {} clashes with a {} of the same name here — skipped",
                o.typ, o.name, cur.typ
            )),
            Some(cur) if norm_sql(&cur.sql) != norm_sql(&o.sql) => {
                replace_objs.push(json!({"type": o.typ, "name": o.name, "sql": o.sql}))
            }
            Some(_) => {}
        }
    }

    // User tables the template doesn't know about stay exactly as they are.
    let src_names: BTreeSet<&str> = src_objs.iter().map(|o| o.name.as_str()).collect();
    let kept_tables: Vec<String> = cur_objs
        .iter()
        .filter(|o| o.typ == "table" && !src_names.contains(o.name.as_str()))
        .map(|o| o.name.clone())
        .collect();

    // Config: fill in what this app doesn't have yet, never overwrite the
    // user's own settings (their provider, key, MCP servers, approvals).
    let mut config_add: Vec<String> = Vec::new();
    {
        let mut stmt = src.prepare("SELECT key FROM uapp_config ORDER BY key")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let taken: i64 = target.query_row(
                "SELECT count(*) FROM uapp_config WHERE key=?1",
                rusqlite::params![key],
                |r| r.get(0),
            )?;
            if taken == 0 {
                config_add.push(key);
            }
        }
    }

    // Whatever user content the source happens to carry (it's a full app copy,
    // not a template) is ignored — say so, so nobody expects it to arrive.
    let src_data_files: i64 = src.query_row(
        "SELECT count(*) FROM sqlar WHERE name LIKE 'data/%'",
        [],
        |r| r.get(0),
    )?;
    let mut src_rows = 0i64;
    for o in src_objs.iter().filter(|o| o.typ == "table") {
        let n: i64 = src
            .query_row(&format!("SELECT count(*) FROM {}", q(&o.name)), [], |r| r.get(0))
            .unwrap_or(0);
        src_rows += n;
    }

    Ok(json!({
        // Who wrote this update. The RPC layer adds the dropped file name.
        "source": {
            "name": store::meta_get(src, "name")?.unwrap_or_default(),
            "appId": store::meta_get(src, "app_id")?.unwrap_or_default(),
            "created": store::meta_get(src, "created")?.unwrap_or_default(),
        },
        "target": {
            "name": store::meta_get(target, "name")?.unwrap_or_default(),
            "appId": store::meta_get(target, "app_id")?.unwrap_or_default(),
        },
        "files": {
            "added": added, "changed": changed, "stale": stale, "unchanged": unchanged,
        },
        "schema": {
            "createTables": create_tables,
            "addColumns": alters,
            "createObjects": create_objs,
            "replaceObjects": replace_objs,
            "keptTables": kept_tables,
        },
        "config": {"added": config_add},
        "ignored": {"dataFiles": src_data_files, "rows": src_rows},
        "warnings": warnings,
    }))
}

// ---- applying ---------------------------------------------------------------

/// Apply a template over the live app. Runs inside the engine's transaction
/// (see `store::apply_op`), so a failure anywhere leaves the file untouched.
///
/// `remove_stale` deletes app files the template no longer carries. It's the
/// truthful default for "update the app", but it's the one destructive part of
/// this, so the caller decides and the user is shown the list first.
pub fn apply(target: &Connection, src: &Connection, op: &Op, remove_stale: bool) -> Result<Value> {
    let mut plan = plan(target, src)?;
    let mut warnings: Vec<String> = plan["warnings"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mtime = (op.ts / 1000) as i64;

    // --- app files (history recorded, so every write stays revertible) ---
    let names = |key: &str| -> Vec<String> {
        plan["files"][key]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let mut written = 0usize;
    for name in names("added").into_iter().chain(names("changed")) {
        let Some(data) = store::sqlar_read(src, &name)? else { continue };
        store::record_file_history(target, op, &name, "put")?;
        store::sqlar_write(target, &name, mtime, &data)?;
        written += 1;
    }
    let stale = names("stale");
    let mut removed = 0usize;
    if remove_stale {
        for name in &stale {
            store::record_file_history(target, op, name, "del")?;
            target.execute("DELETE FROM sqlar WHERE name=?1", rusqlite::params![name])?;
            removed += 1;
        }
    }

    // --- schema (additive; a statement we can't run becomes a warning) ---
    let exec = |sql: &str, what: &str, warnings: &mut Vec<String>| {
        if let Err(e) = target.execute_batch(sql) {
            warnings.push(format!("{what}: {e}"));
            false
        } else {
            true
        }
    };
    let mut tables_created = 0usize;
    for t in plan["schema"]["createTables"].as_array().cloned().unwrap_or_default() {
        let (name, sql) = (t["table"].as_str().unwrap_or(""), t["sql"].as_str().unwrap_or(""));
        if exec(sql, &format!("creating table {name}"), &mut warnings) {
            tables_created += 1;
        }
    }
    let mut columns_added = 0usize;
    for a in plan["schema"]["addColumns"].as_array().cloned().unwrap_or_default() {
        let what = format!("adding {}.{}", a["table"].as_str().unwrap_or(""), a["column"].as_str().unwrap_or(""));
        if exec(a["sql"].as_str().unwrap_or(""), &what, &mut warnings) {
            columns_added += 1;
        }
    }
    let mut objects = 0usize;
    for o in plan["schema"]["replaceObjects"].as_array().cloned().unwrap_or_default() {
        let (typ, name) = (o["type"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""));
        let drop = match typ {
            "view" => format!("DROP VIEW IF EXISTS {}", q(name)),
            "index" => format!("DROP INDEX IF EXISTS {}", q(name)),
            "trigger" => format!("DROP TRIGGER IF EXISTS {}", q(name)),
            _ => continue,
        };
        if exec(&drop, &format!("replacing {typ} {name}"), &mut warnings)
            && exec(o["sql"].as_str().unwrap_or(""), &format!("recreating {typ} {name}"), &mut warnings)
        {
            objects += 1;
        }
    }
    for o in plan["schema"]["createObjects"].as_array().cloned().unwrap_or_default() {
        let (typ, name) = (o["type"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""));
        if exec(o["sql"].as_str().unwrap_or(""), &format!("creating {typ} {name}"), &mut warnings) {
            objects += 1;
        }
    }

    // --- config: only the keys this app doesn't have (never a stray API key) ---
    let mut config_added = 0usize;
    for key in plan["config"]["added"].as_array().cloned().unwrap_or_default() {
        let Some(key) = key.as_str() else { continue };
        let raw: String = match src.query_row(
            "SELECT value FROM uapp_config WHERE key=?1",
            rusqlite::params![key],
            |r| r.get(0),
        ) {
            Ok(v) => v,
            Err(_) => continue,
        };
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
        target.execute(
            "INSERT OR IGNORE INTO uapp_config(key,value) VALUES(?1,?2)",
            rusqlite::params![key, value],
        )?;
        config_added += 1;
    }

    // --- provenance: where this app's code last came from ---
    let src_id = store::meta_get(src, "app_id")?.unwrap_or_default();
    let src_name = store::meta_get(src, "name")?.unwrap_or_default();
    for (k, v) in [
        ("template_source", src_id.as_str()),
        ("template_source_name", src_name.as_str()),
        ("template_updated", &op.ts.to_string()),
    ] {
        target.execute(
            "INSERT OR REPLACE INTO uapp_meta(key,value) VALUES(?1,?2)",
            rusqlite::params![k, v],
        )?;
    }

    plan["warnings"] = json!(warnings);
    plan["applied"] = json!({
        "filesWritten": written,
        "filesRemoved": removed,
        "staleKept": if remove_stale { 0 } else { stale.len() },
        "tablesCreated": tables_created,
        "columnsAdded": columns_added,
        "objectsUpdated": objects,
        "configAdded": config_added,
    });
    Ok(plan)
}

// ---- publish: a site's edited copy replacing the served one ------------------

/// Make `target` (the served site) carry exactly what `src` (a copy edited in
/// the browser — `store::export_public` output) carries, without touching what
/// that copy never had: config (API keys), chat, sessions, file history, the
/// app's identity. This is the write half of `uapp serve`: the update goes
/// the other way from a template — the CONTENT is authoritative, the schema
/// is whatever it needs to be.
///
///   * `app/` files: added, replaced, and removed to match `src`. Every write
///     is recorded in the file history, so a bad publish can be reverted.
///   * `data/` files: the same, but only when `include_data` — a copy that was
///     exported without `data/` says nothing about it, so the server's stays.
///   * user tables, views, indexes, triggers: dropped and recreated from `src`
///     WITH its rows. These are the site's own machinery (a posts table, a
///     search index); their state lives in the copy, not on the server.
///
/// Runs inside the caller's transaction (`Engine::local_op`), so the whole
/// publish lands or nothing does.
pub fn publish(target: &Connection, src: &Connection, op: &Op, include_data: bool) -> Result<Value> {
    let mtime = (op.ts / 1000) as i64;
    let roles: &[&str] = if include_data { &["app", "data"] } else { &["app"] };

    let src_files = files_by_role(src, roles)?;
    let cur_files = files_by_role(target, roles)?;
    let (mut written, mut removed, mut unchanged) = (0usize, 0usize, 0usize);
    for (name, data) in &src_files {
        if cur_files.get(name) == Some(data) {
            unchanged += 1;
            continue;
        }
        store::record_file_history(target, op, name, "put")?;
        store::sqlar_write(target, name, mtime, data)?;
        written += 1;
    }
    for name in cur_files.keys().filter(|n| !src_files.contains_key(*n)) {
        store::record_file_history(target, op, name, "del")?;
        target.execute("DELETE FROM sqlar WHERE name=?1", rusqlite::params![name])?;
        removed += 1;
    }

    // Schema: everything the site owns goes, then comes back as the copy has
    // it. Tables first and rows before indexes/triggers — a trigger created
    // early would fire on the copied rows (same order as `export_public`).
    for o in user_objects(target)?.iter().rev() {
        let kind = match o.typ.as_str() {
            "table" => "TABLE",
            "view" => "VIEW",
            "index" => "INDEX",
            "trigger" => "TRIGGER",
            _ => continue,
        };
        target.execute_batch(&format!("DROP {kind} IF EXISTS {}", q(&o.name)))?;
    }
    let objs = user_objects(src)?;
    let mut tables = 0usize;
    let mut rows = 0usize;
    for o in objs.iter().filter(|o| o.typ == "table") {
        target.execute_batch(&o.sql)?;
        tables += 1;
        let quoted = q(&o.name);
        let mut sel = src.prepare(&format!("SELECT * FROM {quoted}"))?;
        let ncols = sel.column_count();
        if ncols == 0 {
            continue;
        }
        let holders = (1..=ncols).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
        let mut ins = target.prepare(&format!("INSERT INTO {quoted} VALUES({holders})"))?;
        let mut it = sel.query([])?;
        while let Some(r) = it.next()? {
            let vals: Vec<rusqlite::types::Value> = (0..ncols)
                .map(|i| r.get::<_, rusqlite::types::Value>(i))
                .collect::<rusqlite::Result<_>>()?;
            ins.execute(rusqlite::params_from_iter(vals.iter()))?;
            rows += 1;
        }
    }
    let mut objects = 0usize;
    for o in objs.iter().filter(|o| o.typ != "table") {
        target.execute_batch(&o.sql)?;
        objects += 1;
    }

    // Config stays the server's — except the app's own look: the toolbar
    // default (hidden / shortcut, see toolbar.rs) is a property of the site
    // its editor decides on, not a secret, and the served copy carries it to
    // every visitor. Everything else in uapp_config (API keys, MCP servers)
    // never moves.
    let mut carried = 0usize;
    if let Some(v) = store::config_get(src, "toolbar")? {
        target.execute(
            "INSERT OR REPLACE INTO uapp_config(key,value) VALUES('toolbar',?1)",
            rusqlite::params![v.to_string()],
        )?;
        carried += 1;
    }

    Ok(json!({
        "ok": true,
        "files": {"written": written, "removed": removed, "unchanged": unchanged},
        "config": {"carried": carried},
        "schema": {"tables": tables, "rows": rows, "objects": objects},
        "data": include_data,
    }))
}

/// Files of the given roles with their content, inflated so two archives
/// compressed by different builds still compare equal. Directory entries
/// (NULL data) are skipped.
fn files_by_role(conn: &Connection, roles: &[&str]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT name, sz, data FROM sqlar ORDER BY name")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let name: String = r.get(0)?;
        if !roles.contains(&store::file_role(&name)) || name.ends_with('/') {
            continue;
        }
        let sz: Option<i64> = r.get(1)?;
        let Some(data) = r.get::<_, Option<Vec<u8>>>(2)? else { continue };
        out.insert(name.clone(), store::sqlar_inflate(sz, data, &name)?);
    }
    Ok(out)
}
