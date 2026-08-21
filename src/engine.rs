//! The per-app engine: one direct SQLite connection on the .uapp file.
//!
//! Single-user model — one process owns the file (exclusive lock in the
//! per-user servers registry, see `registry`), and every write is one SQLite
//! transaction committed straight into the file. No sibling files ever appear
//! next to the .uapp: the rollback journal is kept in memory
//! (`journal_mode=MEMORY`, `synchronous=FULL`).
//!
//! Durability contract:
//!   * a write is acknowledged only after its transaction has committed
//!     (synchronous=FULL — the pages are fsync'd);
//!   * the in-memory journal means a crash in the middle of a commit can tear
//!     the file. The rescue net is the rolling snapshot directory in app-
//!     private state (`<data_local>/uapp/<app_id>/snapshots/`): a snapshot is
//!     taken on every open, periodically while writes happen, and before
//!     risky whole-file transforms (encryption changes). `open` detects an
//!     unreadable file, moves it aside, and restores the newest snapshot.

use anyhow::Result;
#[cfg(not(target_arch = "wasm32"))]
use anyhow::{anyhow, Context};
use rusqlite::Connection;
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
use crate::registry;
use crate::store::{self, now_ms, Op};

pub const SNAPSHOT_KEEP: usize = 10;
/// While writes keep happening, refresh the rolling snapshot this often
/// (driven by the server's background timer calling `maybe_snapshot`).
pub const SNAPSHOT_INTERVAL_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChangeSet {
    pub data: bool,
    pub files: bool,
    pub chat: bool,
    pub config: bool,
}

impl ChangeSet {
    pub fn note(&mut self, kind: &str) {
        match kind {
            "sql" | "batch" | "import" => self.data = true,
            "file_put" | "file_del" => self.files = true,
            "chat" | "chat_clear" | "chat_compact" | "session_meta" | "session_del" => {
                self.chat = true
            }
            "config_set" => self.config = true,
            _ => self.data = true,
        }
    }
    pub fn any(&self) -> bool {
        self.data || self.files || self.chat || self.config
    }
    pub fn merge(&mut self, o: ChangeSet) {
        self.data |= o.data;
        self.files |= o.files;
        self.chat |= o.chat;
        self.config |= o.config;
    }
}

pub struct Engine {
    pub path: PathBuf,      // the .uapp file — the live database
    pub local_dir: PathBuf, // per-app private state (snapshots)
    pub device: String,
    pub user: String,
    pub db: Connection,
    pub app_id: String,
    /// Present iff this app is encrypted at rest (SQLCipher master password).
    #[cfg(not(target_arch = "wasm32"))]
    pub crypt: Option<crate::cipher::Key>,
    /// Registry lock — held for the engine's lifetime so a second instance
    /// fails fast (and the caller falls back to reusing the running server).
    /// Option so `reencrypt` can release it before re-opening the same path.
    #[cfg(not(target_arch = "wasm32"))]
    _lock: Option<File>,
    dirty: bool,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    last_snapshot_ms: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl Engine {
    pub fn open(path: PathBuf, device: String, user: String) -> Result<Engine> {
        Self::open_with_passphrase(path, device, user, None)
    }

    pub fn open_with_passphrase(
        path: PathBuf,
        device: String,
        user: String,
        passphrase: Option<String>,
    ) -> Result<Engine> {
        let lock = registry::lock_exclusive(&path)?;
        let db = match Self::open_db(&path, passphrase.as_deref()) {
            Ok(db) => db,
            Err(first_err) => {
                // Crash rescue: a crash mid-commit (in-memory journal) can
                // tear the file. If the failure looks like damage — never a
                // missing/wrong password — move the file aside, restore the
                // newest snapshot, and retry once (rolled back on failure).
                if !Self::damage_shaped(&first_err, &path) {
                    return Err(first_err);
                }
                match Self::restore_newest_snapshot(&path) {
                    Some(aside) => Self::open_db(&path, passphrase.as_deref()).map_err(|e| {
                        // The snapshot didn't open either — put the original
                        // back so nothing is lost and the error talks about it.
                        let _ = std::fs::remove_file(&path);
                        let _ = std::fs::rename(&aside, &path);
                        e.context("restored snapshot did not open either")
                    })?,
                    None => return Err(first_err),
                }
            }
        };
        let crypt = passphrase.map(|passphrase| crate::cipher::Key { passphrase });
        let app_id = store::meta_get(&db, "app_id")?.unwrap_or_else(|| "unknown".into());
        let local_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("uapp")
            .join(&app_id);
        std::fs::create_dir_all(local_dir.join("snapshots"))?;
        registry::write_app_map(&path, &app_id, crypt.is_some());

        let mut eng = Engine {
            path,
            local_dir,
            device,
            user,
            db,
            app_id,
            crypt,
            _lock: Some(lock),
            dirty: false,
            last_snapshot_ms: 0,
        };
        // The open-time rescue point (also what a corrupt future open restores).
        if let Err(e) = eng.snapshot() {
            eprintln!("uapp: snapshot on open failed: {e:#}");
        }
        Ok(eng)
    }

    /// Bootstrap (idempotent schema/meta) and open the live connection.
    fn open_db(path: &Path, passphrase: Option<&str>) -> Result<Connection> {
        let app_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".into());
        let is_sqlite = store::looks_like_sqlite(path);
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !is_sqlite && len > 0 {
            // Not a plain SQLite file. Either an encrypted .uapp (openable
            // with the master password) or a foreign/damaged file.
            let Some(pw) = passphrase else {
                return Err(anyhow!(
                    "{} exists but is not a plain .uapp/SQLite file — it may be \
                     password-encrypted; pass --password or set UAPP_PASSWORD",
                    path.display()
                ));
            };
            // bootstrap verifies the key (clear "wrong password" error) and
            // fills in any missing schema inside the encrypted file.
            store::bootstrap(path, &app_name, Some(pw))?;
        } else if is_sqlite {
            if let Some(pw) = passphrase {
                // A plaintext file opened WITH a password: encrypt it in place
                // so the password takes effect immediately.
                crate::cipher::encrypt_file_in_place(path, pw)?;
                eprintln!(
                    "uapp: {} was not encrypted — encrypted in place with the provided password",
                    path.display()
                );
                store::bootstrap(path, &app_name, Some(pw))?;
            } else {
                store::bootstrap(path, &app_name, None)?; // idempotent: fills in missing tables
            }
        } else {
            // Empty/missing: create (encrypted if a password was given).
            store::bootstrap(path, &app_name, passphrase)?;
        }

        let conn = store::with_open_retry(|| match passphrase {
            Some(p) => crate::cipher::open_file(path, p),
            None => Ok(Connection::open(path)?),
        })
        .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // The no-sibling-files rule: rollback journal in memory. A crash mid-
        // commit can tear the file — that's what the snapshots are for.
        conn.pragma_update(None, "journal_mode", "MEMORY")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        if verdict != "ok" {
            return Err(anyhow!(
                "{} failed its integrity check: {verdict}",
                path.display()
            ));
        }
        Ok(conn)
    }

    /// Does this open failure look like file damage (vs a missing/wrong
    /// password)? Restore must never fire on password problems: an encrypted
    /// file opened without its password is indistinguishable from a torn one
    /// by content, so the registry's record of the file's last-known
    /// encryption state is the tiebreaker.
    fn damage_shaped(err: &anyhow::Error, path: &Path) -> bool {
        // Our own quick_check verdict: the file opened (and keyed) but its
        // pages are torn — always damage.
        if format!("{err:#}").contains("failed its integrity check") {
            return true;
        }
        // Unreadable header/pages on a file we remember serving as PLAINTEXT:
        // damage. If we remember it encrypted (or never served it), a bad
        // open is a password problem or a foreign file — leave it alone.
        registry::known_encrypted(path) == Some(false)
    }

    /// Move an unreadable .uapp aside and put the newest rolling snapshot in
    /// its place. Returns the aside path on success, None when we have no
    /// snapshots for this path. (Snapshots are written atomically, so the
    /// newest one is always whole.)
    fn restore_newest_snapshot(path: &Path) -> Option<PathBuf> {
        let local_dir = registry::known_local_dir(path)?;
        let snap = newest_snapshot(&local_dir.join("snapshots"))?;
        let aside = path.with_extension(format!("uapp.corrupt-{}", now_ms()));
        if path.exists() && std::fs::rename(path, &aside).is_err() {
            return None;
        }
        match std::fs::copy(&snap, path) {
            Ok(_) => {
                eprintln!(
                    "uapp: {} was unreadable — restored the latest snapshot ({}); the damaged file was kept as {}",
                    path.display(),
                    snap.display(),
                    aside.display()
                );
                Some(aside)
            }
            Err(e) => {
                eprintln!("uapp: snapshot restore failed: {e}");
                // Put the original back so the caller's error talks about it.
                let _ = std::fs::rename(&aside, path);
                None
            }
        }
    }

    /// The master password, when this app is encrypted at rest.
    pub fn passphrase(&self) -> Option<&str> {
        self.crypt.as_ref().map(|c| c.passphrase.as_str())
    }

    /// Open a destination file for a full copy of the app: SQLCipher-encrypted
    /// when this app is encrypted, a plain SQLite file otherwise.
    fn open_writer(&self, dst: &Path) -> Result<Connection> {
        let conn = match self.passphrase() {
            Some(p) => crate::cipher::open_file(dst, p)?,
            None => Connection::open(dst)?,
        };
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(conn)
    }

    /// Refresh the rolling snapshot if there have been writes since the last
    /// one and it's been a while. Called from the server's background timer.
    pub fn maybe_snapshot(&mut self) {
        if self.dirty && now_ms().saturating_sub(self.last_snapshot_ms) >= SNAPSHOT_INTERVAL_MS {
            if let Err(e) = self.snapshot() {
                eprintln!("uapp: periodic snapshot failed: {e:#}");
            }
        }
    }

    /// Shutdown rescue point: snapshot now if any write happened since the
    /// last one (no time gate — the process is going away).
    pub fn snapshot_if_dirty(&mut self) {
        if self.dirty {
            if let Err(e) = self.snapshot() {
                eprintln!("uapp: snapshot on shutdown failed: {e:#}");
            }
        }
    }

    /// Write one rolling snapshot into the app-private snapshot dir (via the
    /// backup API — a raw byte copy of the live db could capture a torn
    /// mid-transaction state) and rotate old ones out.
    pub fn snapshot(&mut self) -> Result<()> {
        let snapdir = self.local_dir.join("snapshots");
        std::fs::create_dir_all(&snapdir)?;
        let tmp = snapdir.join(format!("snapshot-{}.tmp", fastrand::u32(..)));
        let write = (|| -> Result<()> {
            let mut dst = self.open_writer(&tmp)?;
            let backup = rusqlite::backup::Backup::new(&self.db, &mut dst)?;
            backup.run_to_completion(512, std::time::Duration::from_millis(0), None)?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        {
            let f = File::open(&tmp)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, snapdir.join(format!("{}.uapp", now_ms())))?;
        // Rolling retention.
        let mut snaps: Vec<PathBuf> = std::fs::read_dir(&snapdir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "uapp").unwrap_or(false))
            .collect();
        snaps.sort();
        while snaps.len() > SNAPSHOT_KEEP {
            let old = snaps.remove(0);
            let _ = std::fs::remove_file(old);
        }
        self.dirty = false;
        self.last_snapshot_ms = now_ms();
        Ok(())
    }

    /// "Save As" for a live app (the native window's Save dialog): write the
    /// current state into `new_path` (atomic temp + fsync + rename, encrypted
    /// to match this app), then re-home the running engine there. The server
    /// keeps the same port/token/WebSocket — only the backing file changes, so
    /// the open window switches over with no reload.
    pub fn save_as(&mut self, new_path: PathBuf) -> Result<()> {
        let tmp =
            new_path.with_extension(format!("uapp.tmp-{}-{}", self.device, fastrand::u32(..)));
        let write = (|| -> Result<()> {
            let mut dst = self.open_writer(&tmp)?;
            let backup = rusqlite::backup::Backup::new(&self.db, &mut dst)?;
            backup.run_to_completion(512, std::time::Duration::from_millis(0), None)?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        {
            let f = File::open(&tmp)?;
            f.sync_all()?;
        }
        // "Save As" onto the file we're already serving: release our own
        // connection and registry lock first — the rename below would fail on
        // Windows over an open db, and the re-open would deadlock on our lock.
        let same_file = registry::key(&new_path) == registry::key(&self.path);
        if same_file {
            drop(std::mem::replace(&mut self.db, Connection::open_in_memory()?));
            self._lock = None;
        }
        std::fs::rename(&tmp, &new_path)
            .with_context(|| format!("writing {}", new_path.display()))?;
        if let Some(dir) = new_path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        // Re-open at the new location with the same passphrase (new registry
        // lock; the old lock and connection are released when self is replaced).
        let passphrase = self.passphrase().map(|s| s.to_string());
        let reopened = Engine::open_with_passphrase(
            new_path,
            self.device.clone(),
            self.user.clone(),
            passphrase,
        )?;
        if !same_file {
            registry::remove_addr(&self.path); // the old path is no longer served
        }
        *self = reopened;
        Ok(())
    }

    /// Change this app's at-rest encryption from the running engine (the
    /// Settings → Encryption panel): `None` → remove encryption, `Some(pw)` →
    /// encrypt a plaintext app or re-key an already-encrypted one. The file is
    /// transformed in place (atomic temp + rename) and the engine re-opens
    /// with the new key — same path, so the server/port and open window are
    /// unaffected.
    pub fn reencrypt(&mut self, new_passphrase: Option<String>) -> Result<()> {
        let currently_encrypted = self.crypt.is_some();
        if !currently_encrypted && new_passphrase.is_none() {
            return Ok(()); // already plaintext, nothing to do
        }
        let old_pw = self.passphrase().map(|s| s.to_string());
        // Rescue point before a whole-file transform.
        if let Err(e) = self.snapshot() {
            eprintln!("uapp: snapshot before encryption change failed: {e:#}");
        }
        // Close our live connection first: the transform replaces the file by
        // rename, which Windows refuses while SQLite holds the old file open.
        drop(std::mem::replace(&mut self.db, Connection::open_in_memory()?));
        let transform = match (currently_encrypted, new_passphrase.as_deref()) {
            (false, Some(pw)) => crate::cipher::encrypt_file_in_place(&self.path, pw),
            (true, None) => {
                let old = old_pw
                    .as_deref()
                    .ok_or_else(|| anyhow!("internal: encrypted app with no in-memory password"));
                old.and_then(|o| crate::cipher::decrypt_file_in_place(&self.path, o))
            }
            (true, Some(new)) => {
                let old = old_pw
                    .as_deref()
                    .ok_or_else(|| anyhow!("internal: encrypted app with no in-memory password"));
                old.and_then(|o| crate::cipher::rekey_file(&self.path, o, new))
            }
            (false, None) => unreachable!(),
        };
        // Re-open at the same path — with the new key on success, the old key
        // on failure (so a failed change leaves a working engine). Release our
        // registry lock first; re-opening the same path re-takes it.
        self._lock = None;
        let reopen_with = match &transform {
            Ok(()) => new_passphrase,
            Err(_) => old_pw,
        };
        let reopened = Engine::open_with_passphrase(
            self.path.clone(),
            self.device.clone(),
            self.user.clone(),
            reopen_with,
        )
        .context("re-opening after the encryption change")?;
        *self = reopened;
        transform
    }
}

// Shared by native and wasm builds.
impl Engine {
    /// Execute one local operation as one committed transaction against the
    /// file. On any failure the database is left untouched.
    pub fn local_op(&mut self, kind: &str, payload: Value) -> Result<(Value, Op)> {
        let op = Op {
            dev: self.device.clone(),
            ts: now_ms(),
            user: self.user.clone(),
            kind: kind.to_string(),
            payload,
        };
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        match store::apply_op(&self.db, &op) {
            Ok(v) => match self.db.execute_batch("COMMIT") {
                Ok(()) => {
                    self.dirty = true;
                    Ok((v, op))
                }
                Err(e) => {
                    let _ = self.db.execute_batch("ROLLBACK");
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

}

#[cfg(target_arch = "wasm32")]
impl Engine {
    /// Browser build: the engine lives on an in-memory SQLite database. `bytes`
    /// (a complete .uapp file, e.g. picked/dropped by the user or restored from
    /// OPFS) is loaded via sqlite3_deserialize; None starts a fresh app.
    pub fn open_from_bytes(
        name: &str,
        bytes: Option<Vec<u8>>,
        device: String,
        user: String,
    ) -> Result<Engine> {
        let db = Connection::open_in_memory()?;
        if let Some(b) = bytes {
            if b.len() < 16 || &b[..15] != b"SQLite format 3" {
                anyhow::bail!(
                    "this is not a plain .uapp/SQLite file — encrypted apps can't be \
                     opened in the browser demo (decrypt it in the desktop app first)"
                );
            }
            unsafe {
                let len = b.len();
                let buf = rusqlite::ffi::sqlite3_malloc64(len as u64) as *mut u8;
                if buf.is_null() {
                    anyhow::bail!("out of memory loading the app ({len} bytes)");
                }
                std::ptr::copy_nonoverlapping(b.as_ptr(), buf, len);
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
                    anyhow::bail!("could not load the .uapp bytes (sqlite error {rc})");
                }
            }
            let verdict: String = db.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
            if verdict != "ok" {
                anyhow::bail!("the file failed its integrity check: {verdict}");
            }
        }
        store::bootstrap_conn(&db, name)?; // idempotent: fills in missing schema
        let app_id = store::meta_get(&db, "app_id")?.unwrap_or_else(|| "unknown".into());
        Ok(Engine {
            path: PathBuf::from(format!("{name}.uapp")),
            local_dir: PathBuf::new(),
            device,
            user,
            db,
            app_id,
            dirty: false,
            last_snapshot_ms: 0,
        })
    }

    /// The complete current state as .uapp file bytes (sqlite3_serialize) —
    /// what OPFS persistence and the Download button save.
    pub fn serialize_bytes(&self) -> Result<Vec<u8>> {
        unsafe {
            let mut size: i64 = 0;
            let p = rusqlite::ffi::sqlite3_serialize(self.db.handle(), c"main".as_ptr(), &mut size, 0);
            if p.is_null() {
                anyhow::bail!("sqlite3_serialize failed (out of memory?)");
            }
            let out = std::slice::from_raw_parts(p as *const u8, size as usize).to_vec();
            rusqlite::ffi::sqlite3_free(p as *mut std::ffi::c_void);
            Ok(out)
        }
    }

    pub fn passphrase(&self) -> Option<&str> {
        None
    }
    pub fn maybe_snapshot(&mut self) {}
    pub fn snapshot_if_dirty(&mut self) {}
    pub fn snapshot(&mut self) -> Result<()> {
        Ok(())
    }
    /// Any write since open (drives the shell's OPFS auto-save on wasm).
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn save_as(&mut self, _new_path: PathBuf) -> Result<()> {
        anyhow::bail!("Save As is not available in the browser demo — use Download instead")
    }
    pub fn reencrypt(&mut self, _new_passphrase: Option<String>) -> Result<()> {
        anyhow::bail!("encryption is not available in the browser demo")
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Newest `<ms>.uapp` in a snapshot dir, if any.
fn newest_snapshot(snapdir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(snapdir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "uapp").unwrap_or(false))
        .max()
}
