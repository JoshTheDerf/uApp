//! Optional at-rest encryption for a .uapp file.
//!
//! Threat model: anyone holding the file (cloud provider, stolen backups)
//! sees ciphertext only. The device that legitimately opens the app (it has
//! the master password) trusts its own RAM and its localhost browser session,
//! which see plaintext. There is no escrow: a lost password means a lost
//! file — warn users accordingly.
//!
//! The `.uapp` is a SQLCipher database (AES-256-CBC + HMAC per page; the
//! passphrase is stretched with SQLCipher 4 defaults: PBKDF2-HMAC-SHA512,
//! 256k iterations, per-file random salt). Snapshots and exports of an
//! encrypted app are written through SQLCipher too, so no plaintext copy
//! ever rests on disk.
//!
//! Plaintext files keep working exactly as before; encryption is opt-in per
//! file (`uapp encrypt <file>` or open with a password).

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// The master password of one encrypted app.
#[derive(Clone)]
pub struct Key {
    pub passphrase: String,
}

/// Escape a string into a single-quoted SQL literal (for `PRAGMA key`).
pub fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// SQLCipher prints its own errors to stderr ("ERROR CORE sqlite3Codec: error
/// decrypting page 1") on every wrong password, which we already report in
/// plain words. Its log level is process-wide; keep it off unless debug
/// logging is on. Doesn't read the database, so it can run before the key.
fn quiet_sqlcipher_log(conn: &Connection) {
    let level = if tracing::enabled!(tracing::Level::DEBUG) {
        "ERROR"
    } else {
        "NONE"
    };
    let _ = conn.execute_batch(&format!("PRAGMA cipher_log_level = {level};"));
}

/// Apply the master password to a connection. MUST be the first statement on
/// it — SQLCipher refuses to decrypt anything read before the key is set.
fn apply_key(conn: &Connection, passphrase: &str) -> Result<()> {
    quiet_sqlcipher_log(conn);
    // execute_batch (not pragma_update) so the passphrase is a real SQL
    // literal, never a bound parameter.
    conn.execute_batch(&format!("PRAGMA key = {};", sql_quote(passphrase)))
        .context("setting encryption key")?;
    Ok(())
}

/// Key + verify: reading sqlite_master forces SQLCipher to actually decrypt
/// page 1, so a wrong password fails HERE with a clear message instead of
/// deep inside a backup or query later.
fn apply_and_verify(conn: &Connection, passphrase: &str, what: &str) -> Result<()> {
    apply_key(conn, passphrase)?;
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::NotADatabase =>
        {
            bail!("wrong master password (or {what} is not an encrypted uapp file)")
        }
        Err(e) => Err(e).with_context(|| format!("verifying encryption key on {what}")),
    }
}

/// Open (creating if absent) a SQLCipher database file read-write.
pub fn open_file(path: &Path, passphrase: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    apply_and_verify(&conn, passphrase, &path.display().to_string())?;
    Ok(conn)
}

/// Open an existing SQLCipher database read-only: a full copy of an
/// encrypted app dropped on a window as a template, which is only read.
pub fn open_file_readonly(path: &Path, passphrase: &str) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    apply_and_verify(&conn, passphrase, &path.display().to_string())?;
    Ok(conn)
}

// ---- whole-file conversions -------------------------------------------

fn fsync_file(p: &Path) -> Result<()> {
    std::fs::File::open(p)?.sync_all()?;
    Ok(())
}

fn fsync_dir(p: &Path) {
    if let Some(dir) = p.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

/// Encrypt a plaintext .uapp in place: write an encrypted copy to a temp
/// file via `sqlcipher_export` (SQLite's backup API refuses a plaintext →
/// encrypted copy, so the copy runs inside SQLite itself), then atomically
/// rename over the original. The plaintext original is gone the moment the
/// rename lands.
pub fn encrypt_file_in_place(path: &Path, passphrase: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "uapp.enc-{}",
        (0..8).map(|_| fastrand::alphanumeric()).collect::<String>()
    ));
    let result = (|| -> Result<()> {
        let src = Connection::open(path)?;
        quiet_sqlcipher_log(&src);
        src.execute_batch(&format!(
            "ATTACH DATABASE {} AS enc KEY {};",
            sql_quote(&tmp.display().to_string()),
            sql_quote(passphrase)
        ))
        .context("attaching encrypted target")?;
        src.query_row("SELECT sqlcipher_export('enc')", [], |_| Ok(()))
            .context("exporting to the encrypted copy")?;
        copy_header(&src, "enc")?;
        src.execute_batch("DETACH DATABASE enc;")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            fsync_file(&tmp)?;
            std::fs::rename(&tmp, path)?;
            fsync_dir(path);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Decrypt an encrypted .uapp in place (requires the master password). The
/// plaintext copy is produced by `sqlcipher_export` into an attached plain
/// database (the backup API refuses encrypted → plaintext copies). The
/// original stays untouched until the atomic rename.
pub fn decrypt_file_in_place(path: &Path, passphrase: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "uapp.dec-{}",
        (0..8).map(|_| fastrand::alphanumeric()).collect::<String>()
    ));
    let result = (|| -> Result<()> {
        let src = open_file(path, passphrase)?;
        src.execute_batch(&format!(
            "ATTACH DATABASE {} AS plain KEY '';",
            sql_quote(&tmp.display().to_string())
        ))
        .context("attaching plaintext target")?;
        src.query_row("SELECT sqlcipher_export('plain')", [], |_| Ok(()))
            .context("exporting to the plaintext copy")?;
        // Old files stamped a record-sealing key into their meta (the retired
        // sync engine) — a plaintext file must never carry it.
        src.execute_batch("DELETE FROM plain.uapp_meta WHERE key='crypt';")?;
        copy_header(&src, "plain")?;
        src.execute_batch("DETACH DATABASE plain;")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            fsync_file(&tmp)?;
            std::fs::rename(&tmp, path)?;
            fsync_dir(path);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// `sqlcipher_export` copies schema and rows but not the header's
/// application_id and user_version, which say the file is a .uapp and which
/// format it's in (see store.rs). Carry them over to the attached copy.
fn copy_header(src: &Connection, schema: &str) -> Result<()> {
    let app_id: i64 = src.query_row("PRAGMA main.application_id", [], |r| r.get(0))?;
    let version: i64 = src.query_row("PRAGMA main.user_version", [], |r| r.get(0))?;
    src.execute_batch(&format!(
        "PRAGMA {schema}.application_id = {app_id}; PRAGMA {schema}.user_version = {version};"
    ))?;
    Ok(())
}

/// Change the master password (SQLCipher rekey).
pub fn rekey_file(path: &Path, old: &str, new: &str) -> Result<()> {
    let conn = Connection::open(path)?;
    apply_and_verify(&conn, old, &path.display().to_string())?;
    conn.execute_batch(&format!("PRAGMA rekey = {};", sql_quote(new)))
        .context("rekeying database")?;
    // Force a write so the rekeyed pages are durable before we report done.
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS uapp_rekey_stamp(x); DROP TABLE uapp_rekey_stamp;",
    )?;
    Ok(())
}
