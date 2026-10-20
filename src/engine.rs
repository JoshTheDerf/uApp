//! The per-app engine: one direct SQLite connection on the .uapp file.
//!
//! Single-user model: one process owns the file (exclusive lock in the
//! per-user servers registry, see `registry`), and every write is one SQLite
//! transaction committed straight into the file in rollback-journal mode
//! (`journal_mode=DELETE`, `synchronous=FULL`). While a commit is in progress
//! SQLite keeps `<name>.uapp-journal` next to the file and deletes it when
//! the commit is done, so between writes the .uapp stands alone. If the
//! process dies mid-commit (a crash, an Android process kill), the journal
//! stays behind and SQLite uses it to roll the half-written transaction back
//! the next time the file is opened. That only works while the journal sits
//! next to the file it belongs to, so code that moves or replaces a .uapp
//! deals with its journal too (see `store::journal_path`).
//!
//! Durability contract:
//!   * a write is acknowledged only after its transaction has committed
//!     (synchronous=FULL: journal and pages are fsync'd);
//!   * a crash at any point leaves the file holding either the old state or
//!     the new one;
//!   * for damage from outside (a truncated copy, a sync tool writing half a
//!     file) there are rolling snapshots in app-private state, one directory
//!     per .uapp path (`registry::snapshot_dir`). A snapshot is taken on
//!     open when the file changed since the newest one, periodically while
//!     writes happen, and before risky whole-file transforms (encryption
//!     changes). `open` detects an unreadable file, moves it aside, and
//!     restores the newest snapshot of that same path.

use anyhow::Result;
#[cfg(not(target_arch = "wasm32"))]
use anyhow::{Context, anyhow, bail};
use rusqlite::Connection;
use serde_json::Value;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
use crate::registry;
use crate::store::{self, Op, now_ms};

pub const SNAPSHOT_KEEP: usize = 10;
/// In a snapshot dir: the canonical path of the file the snapshots are of.
#[cfg(not(target_arch = "wasm32"))]
const SNAPSHOT_SOURCE: &str = "source";
/// In a snapshot dir: `file_state` of the file when the newest snapshot was
/// taken.
#[cfg(not(target_arch = "wasm32"))]
const SNAPSHOT_STATE: &str = "state";
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
            "config_set" | "app_rename" => self.config = true,
            // A template update rewrites app files, reconciles the schema and
            // can fill in config keys — everything but the chat.
            "template_update" => {
                self.files = true;
                self.data = true;
                self.config = true;
            }
            // A publish replaces app (and maybe data) files and the site's
            // own tables wholesale; config and chat are untouched.
            "publish" => {
                self.files = true;
                self.data = true;
            }
            _ => self.data = true,
        }
    }
    pub fn any(&self) -> bool {
        self.data || self.files || self.chat || self.config
    }
}

pub struct Engine {
    pub path: PathBuf, // the .uapp file — the live database
    /// This path's rolling snapshots (`registry::snapshot_dir`).
    #[cfg(not(target_arch = "wasm32"))]
    snapshot_dir: PathBuf,
    pub device: String,
    pub user: String,
    pub db: Connection,
    pub app_id: String,
    /// Present iff this app is encrypted at rest (SQLCipher master password).
    #[cfg(not(target_arch = "wasm32"))]
    pub crypt: Option<crate::cipher::Key>,
    /// Registry lock — held for the engine's lifetime so a second instance
    /// fails fast (and the caller falls back to reusing the running server).
    /// `reencrypt` hands a duplicate of it to the re-opened engine, so the
    /// path is never unlocked in between.
    #[cfg(not(target_arch = "wasm32"))]
    lock: File,
    dirty: bool,
    /// Committed local ops so far. Cheap change detection for caches built
    /// from the database (the public `/site.uapp`): every write is one op.
    pub writes: u64,
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
        Self::open_locked(path, device, user, passphrase, lock)
    }

    /// `open_with_passphrase` with the path's registry lock already held.
    fn open_locked(
        path: PathBuf,
        device: String,
        user: String,
        passphrase: Option<String>,
        lock: File,
    ) -> Result<Engine> {
        // A file we have served before that is now zero bytes was emptied by
        // something outside us (an interrupted copy or sync, a provider
        // write-back cut short). Opening it would quietly start a blank app,
        // and on Android the write-back mirror would then copy that blank
        // app over the user's original. Treat it as damage instead.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0)
            && registry::known_encrypted(&path).is_some()
            && Self::restore_newest_snapshot(&path).is_none()
        {
            tracing::warn!(
                "{} is empty and has no snapshot to restore, so it opens as a new app",
                path.display()
            );
        }
        let db = match Self::open_db(&path, passphrase.as_deref()) {
            Ok(db) => db,
            Err(first_err) => {
                // Damage rescue: if the failure looks like damage (never a
                // missing/wrong password), move the file aside, restore the
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
        // The file says what its app_id is; only a well-formed one is used.
        let app_id = store::meta_get(&db, "app_id")?
            .filter(|id| registry::valid_app_id(id))
            .unwrap_or_else(|| "unknown".into());
        registry::write_app_map(&path, &app_id, crypt.is_some());

        let mut eng = Engine {
            snapshot_dir: registry::snapshot_dir(&path),
            path,
            device,
            user,
            db,
            app_id,
            crypt,
            lock,
            dirty: false,
            writes: 0,
            last_snapshot_ms: 0,
        };
        // The open-time rescue point (also what a corrupt future open
        // restores), unless the newest snapshot already holds this exact file:
        // opening an unchanged app ten times shouldn't churn all ten slots.
        if !eng.snapshot_is_current()
            && let Err(e) = eng.snapshot()
        {
            tracing::warn!("snapshot on open failed: {e:#}");
        }
        Ok(eng)
    }

    /// Open the live connection: checked, migrated and hardened (see
    /// `store::open_file` and `store::harden`).
    fn open_db(path: &Path, passphrase: Option<&str>) -> Result<Connection> {
        let app_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".into());
        let is_sqlite = store::looks_like_sqlite(path);
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !is_sqlite && len > 0 && passphrase.is_none() {
            // Not a plain SQLite file. Either an encrypted .uapp (openable
            // with the master password) or a foreign/damaged file.
            return Err(anyhow!(
                "{} exists but is not a plain .uapp/SQLite file — it may be \
                 password-encrypted; pass --password or set UAPP_PASSWORD",
                path.display()
            ));
        }
        if is_sqlite && let Some(pw) = passphrase {
            // A plaintext file opened WITH a password: encrypt it in place
            // so the password takes effect immediately.
            crate::cipher::encrypt_file_in_place(path, pw)?;
            tracing::info!(
                "{} was not encrypted, so it was encrypted in place with the provided password",
                path.display()
            );
        }
        // An encrypted file is keyed (and the key verified, with a clear
        // "wrong password" error) before anything else reads it.
        let conn = store::open_file(path, &app_name, passphrase)?;
        store::harden(&conn)?;
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
        // open is a password problem or a foreign file — leave it alone. So
        // is anything that isn't SQLite saying the bytes are bad (a lock, a
        // permission problem, a file from a newer uApp).
        let unreadable = err.chain().any(|e| {
            e.downcast_ref::<rusqlite::Error>()
                .and_then(|e| e.sqlite_error_code())
                .is_some_and(|c| {
                    matches!(
                        c,
                        rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt
                    )
                })
        });
        // A plaintext file whose SQLite header is gone is refused before
        // SQLite ever sees it (it might be encrypted), so check that here.
        let header_gone =
            !store::looks_like_sqlite(path) && std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
        (unreadable || header_gone) && registry::known_encrypted(path) == Some(false)
    }

    /// Move an unreadable .uapp aside and put the newest rolling snapshot in
    /// its place. Returns the aside path on success, None when we have no
    /// snapshots for this path. (Snapshots are written atomically, so the
    /// newest one is always whole.)
    fn restore_newest_snapshot(path: &Path) -> Option<PathBuf> {
        let snap = Self::snapshot_for(path)?;
        let aside = path.with_extension(format!("uapp.corrupt-{}", now_ms()));
        if path.exists() && std::fs::rename(path, &aside).is_err() {
            return None;
        }
        // A journal left by a crash belongs to the damaged file: keep it with
        // that file, so it can't be "rolled back" into the restored one.
        let journal = store::journal_path(path);
        if journal.exists() {
            let _ = std::fs::rename(&journal, store::journal_path(&aside));
        }
        match std::fs::copy(&snap, path) {
            Ok(_) => {
                tracing::warn!(
                    "{} was unreadable, so the latest snapshot ({}) was restored; the damaged file was kept as {}",
                    path.display(),
                    snap.display(),
                    aside.display()
                );
                Some(aside)
            }
            Err(e) => {
                tracing::warn!("snapshot restore failed: {e}");
                // Put the original back so the caller's error talks about it.
                let _ = std::fs::rename(&aside, path);
                None
            }
        }
    }

    /// The newest snapshot taken of this path. Its own snapshot dir must say
    /// it holds this path (a different file can't end up restored here). A
    /// file last opened by an older uApp has no such dir yet; for that one
    /// the old per-app_id dir is used.
    fn snapshot_for(path: &Path) -> Option<PathBuf> {
        let dir = registry::snapshot_dir(path);
        match std::fs::read_to_string(dir.join(SNAPSHOT_SOURCE)) {
            Ok(source) if source == registry::canonical(path).to_string_lossy() => {
                newest_snapshot(&dir)
            }
            Ok(_) => None,
            Err(_) => registry::legacy_snapshot_dir(path).and_then(|d| newest_snapshot(&d)),
        }
    }

    /// Does the newest snapshot already hold the file as it is now? Compared
    /// by size, modification time and the first bytes of the file (SQLite
    /// bumps a change counter in the header on every commit; an encrypted
    /// header is re-encrypted), as recorded when that snapshot was taken.
    fn snapshot_is_current(&self) -> bool {
        let recorded = std::fs::read_to_string(self.snapshot_dir.join(SNAPSHOT_STATE)).ok();
        recorded.is_some()
            && recorded == file_state(&self.path)
            && Self::snapshot_for(&self.path).is_some()
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
        if self.dirty
            && now_ms().saturating_sub(self.last_snapshot_ms) >= SNAPSHOT_INTERVAL_MS
            && let Err(e) = self.snapshot()
        {
            tracing::warn!("periodic snapshot failed: {e:#}");
        }
    }

    /// Shutdown rescue point: snapshot now if any write happened since the
    /// last one (no time gate — the process is going away).
    pub fn snapshot_if_dirty(&mut self) {
        if self.dirty
            && let Err(e) = self.snapshot()
        {
            tracing::warn!("snapshot on shutdown failed: {e:#}");
        }
    }

    /// Write one rolling snapshot into this path's snapshot dir (via the
    /// backup API — a raw byte copy of the live db could capture a torn
    /// mid-transaction state) and rotate old ones out.
    pub fn snapshot(&mut self) -> Result<()> {
        // After a failed Save As or encryption change that couldn't re-open
        // the file, `db` is an empty stand-in: never let it replace a real
        // snapshot.
        if self.db.path().is_none_or(str::is_empty) {
            bail!("the app's file isn't open, so there is nothing to snapshot");
        }
        let snapdir = self.snapshot_dir.clone();
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
        // Which file these snapshots are of: checked before any restore.
        let source = registry::canonical(&self.path)
            .to_string_lossy()
            .to_string();
        if std::fs::read_to_string(snapdir.join(SNAPSHOT_SOURCE))
            .ok()
            .as_ref()
            != Some(&source)
        {
            std::fs::write(snapdir.join(SNAPSHOT_SOURCE), &source)?;
        }
        std::fs::rename(&tmp, snapdir.join(format!("{}.uapp", now_ms())))?;
        if let Some(state) = file_state(&self.path) {
            let _ = std::fs::write(snapdir.join(SNAPSHOT_STATE), state);
        }
        // Rolling retention, plus any temp file a crash left behind (only
        // this process writes here: the dir belongs to the locked path).
        let mut snaps: Vec<PathBuf> = Vec::new();
        for p in std::fs::read_dir(&snapdir)?.flatten().map(|e| e.path()) {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            let name = name.as_deref().unwrap_or("");
            if p.extension().is_some_and(|e| e == "uapp") {
                snaps.push(p);
            } else if name.starts_with("snapshot-")
                && (name.ends_with(".tmp") || name.ends_with(".tmp-journal"))
            {
                let _ = std::fs::remove_file(&p);
            }
        }
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
    /// the open window switches over with no reload. If anything fails, the
    /// engine stays on the file it had.
    pub fn save_as(&mut self, new_path: PathBuf) -> Result<()> {
        if registry::key(&new_path) == registry::key(&self.path) {
            // Every write is already committed to this very file.
            return Ok(());
        }
        // Claim the destination first: another window may have it open, and
        // replacing a file under a live connection would break that window.
        let lock = registry::lock_exclusive(&new_path)?;
        let tmp = new_path.with_extension(format!("uapp.tmp-{}", fastrand::u32(..)));
        let write = (|| -> Result<()> {
            let mut dst = self.open_writer(&tmp)?;
            let backup = rusqlite::backup::Backup::new(&self.db, &mut dst)?;
            backup.run_to_completion(512, std::time::Duration::from_millis(0), None)?;
            drop(backup);
            drop(dst);
            File::open(&tmp)?.sync_all()?;
            // A journal at the destination belongs to the file being replaced.
            let _ = std::fs::remove_file(store::journal_path(&new_path));
            std::fs::rename(&tmp, &new_path)
                .with_context(|| format!("writing {}", new_path.display()))?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_file(store::journal_path(&tmp));
            return Err(e);
        }
        fsync_parent(&new_path);
        let reopened = Engine::open_locked(
            new_path,
            self.device.clone(),
            self.user.clone(),
            self.passphrase().map(|s| s.to_string()),
            lock,
        )
        .context("opening the saved copy")?;
        registry::remove_addr(&self.path); // the old path is no longer served
        let writes = self.writes + 1;
        *self = reopened;
        self.writes = writes;
        Ok(())
    }

    /// Change this app's at-rest encryption from the running engine (the
    /// Settings → Encryption panel): `None` → remove encryption, `Some(pw)` →
    /// encrypt a plaintext app or re-key an already-encrypted one. The file is
    /// transformed in place (atomic temp + rename) and the engine re-opens
    /// with the new key — same path, so the server/port and open window are
    /// unaffected. Copies under the old key or in plaintext (older snapshots,
    /// damaged files kept aside) are removed once the change has landed.
    pub fn reencrypt(&mut self, new_passphrase: Option<String>) -> Result<()> {
        let currently_encrypted = self.crypt.is_some();
        if !currently_encrypted && new_passphrase.is_none() {
            return Ok(()); // already plaintext, nothing to do
        }
        let old_pw = self.passphrase().map(|s| s.to_string());
        // Rescue point before a whole-file transform.
        if let Err(e) = self.snapshot() {
            tracing::warn!("snapshot before encryption change failed: {e:#}");
        }
        let old_snapshots = snapshot_files(&self.snapshot_dir);
        // The re-opened engine shares our lock, so the path stays locked.
        let lock = self.lock.try_clone()?;
        // Close our live connection first: the transform replaces the file by
        // rename, which Windows refuses while SQLite holds the old file open.
        drop(std::mem::replace(
            &mut self.db,
            Connection::open_in_memory()?,
        ));
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
        // on failure (so a failed change leaves a working engine).
        let reopen_with = match &transform {
            Ok(()) => new_passphrase.clone(),
            Err(_) => old_pw,
        };
        let reopened = Engine::open_locked(
            self.path.clone(),
            self.device.clone(),
            self.user.clone(),
            reopen_with,
            lock,
        )
        .context("re-opening after the encryption change")?;
        let writes = self.writes + 1;
        *self = reopened;
        self.writes = writes;
        transform?;
        self.forget_old_copies(&old_snapshots, new_passphrase.is_some());
        Ok(())
    }

    /// After an encryption change: delete the snapshots taken before it (in
    /// plaintext or under the old key; the re-open took a fresh one), and
    /// when the file is now encrypted, the damaged copies kept beside it.
    fn forget_old_copies(&mut self, old_snapshots: &[PathBuf], encrypted: bool) {
        for p in old_snapshots {
            let _ = std::fs::remove_file(p);
        }
        // Snapshots an older build kept per app_id.
        if let Some(dir) = registry::legacy_snapshot_dir(&self.path) {
            for p in snapshot_files(&dir) {
                let _ = std::fs::remove_file(p);
            }
        }
        if encrypted {
            let prefix = format!(
                "{}.corrupt-",
                self.path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
            let dir = self.path.parent().unwrap_or(Path::new("."));
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if snapshot_files(&self.snapshot_dir).is_empty()
            && let Err(e) = self.snapshot()
        {
            tracing::warn!("snapshot after encryption change failed: {e:#}");
        }
    }
}

// Shared by native and wasm builds.
impl Engine {
    /// Reclaim the space deleted rows and files left behind (Settings →
    /// Maintenance), then refresh the rescue snapshot.
    pub fn compact(&mut self) -> Result<()> {
        use rusqlite::limits::Limit;
        // VACUUM rebuilds the file through a temporary attached database, so
        // the live connection's no-ATTACH limit is lifted just for it.
        let prev = self.db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)?;
        let done = self.db.execute_batch("VACUUM");
        self.db.set_limit(Limit::SQLITE_LIMIT_ATTACHED, prev)?;
        done?;
        // Same content, new bytes: anything cached from the file (the public
        // /site.uapp) has to be rebuilt.
        self.dirty = true;
        self.writes += 1;
        if let Err(e) = self.snapshot() {
            tracing::warn!("snapshot after compacting failed: {e:#}");
        }
        Ok(())
    }

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
                    self.writes += 1;
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
        let db = match bytes {
            Some(b) => store::deserialize_bytes(&b)?,
            None => Connection::open_in_memory()?,
        };
        store::bootstrap_conn(&db, name)?; // idempotent: fills in missing schema
        store::harden(&db)?;
        let app_id = store::meta_get(&db, "app_id")?.unwrap_or_else(|| "unknown".into());
        Ok(Engine {
            path: PathBuf::from(format!("{name}.uapp")),
            device,
            user,
            db,
            app_id,
            dirty: false,
            writes: 0,
            last_snapshot_ms: 0,
        })
    }

    /// The complete current state as .uapp file bytes (sqlite3_serialize) —
    /// what OPFS persistence and the Download button save.
    pub fn serialize_bytes(&self) -> Result<Vec<u8>> {
        unsafe {
            let mut size: i64 = 0;
            let p =
                rusqlite::ffi::sqlite3_serialize(self.db.handle(), c"main".as_ptr(), &mut size, 0);
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
/// Size, modification time and a hash of the first KiB of a file: enough to
/// tell whether it changed since it was last looked at.
fn file_state(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut head = vec![0u8; 1024];
    let n = File::open(path).ok()?.read(&mut head).ok()?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &head[..n] {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{} {mtime} {h:016x}", meta.len()))
}

#[cfg(not(target_arch = "wasm32"))]
/// Every `<ms>.uapp` in a snapshot dir.
fn snapshot_files(snapdir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(snapdir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "uapp"))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn fsync_parent(path: &Path) {
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Newest `<ms>.uapp` in a snapshot dir, if any.
fn newest_snapshot(snapdir: &Path) -> Option<PathBuf> {
    snapshot_files(snapdir).into_iter().max()
}
