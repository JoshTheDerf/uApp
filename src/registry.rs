//! Per-user server registry: which .uapp files are currently being served on
//! this machine, and by which process. Replaces the old sidecar's `.addr` /
//! `.log.lock` files — everything lives in app-private state, never next to
//! the user's file:
//!
//!   <data_local>/uapp/servers/<key>.lock  — exclusive advisory lock held for
//!                                           the engine's lifetime (a second
//!                                           open fails fast and falls back to
//!                                           reusing the running server)
//!   <data_local>/uapp/servers/<key>.addr  — {port, token, pid} of the server,
//!                                           owner-readable only
//!   <data_local>/uapp/servers/<key>.app   — the file's app_id and whether it
//!                                           was encrypted, so a failed open
//!                                           can tell damage from a password
//!                                           problem
//!   <data_local>/uapp/snapshots/<key>/    — the file's rolling snapshots, with
//!                                           a `source` file naming the path
//!                                           they were taken from
//!
//! `<key>` is a hash of the canonicalized path — computable before the file is
//! opened (and for encrypted files), which is what server discovery needs.
//! Snapshots are keyed by it too, never by the app_id inside the file: copies
//! of an app share their app_id (and the file decides what it says), so an
//! app_id-keyed directory let one copy's snapshot be restored over another.

use anyhow::{anyhow, Result};
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Root of uapp's per-user local state (`<data_local>/uapp`): the servers
/// registry and each app's snapshots. Mobile has no data_local dir (the
/// `dirs` crate returns None on Android/iOS), so fall back to the working
/// directory, which the GUI anchors in the app's private data dir. Writers and
/// readers must both go through here, or snapshots get written somewhere a
/// later open never looks.
pub fn local_root() -> PathBuf {
    dirs::data_local_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uapp")
}

fn dir() -> PathBuf {
    local_root().join("servers")
}

/// The canonical form of a path. The file may not exist yet (fresh
/// scratch), so fall back to canonicalizing the parent and re-joining the
/// file name.
pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| match (path.parent(), path.file_name()) {
            (Some(p), Some(f)) => p
                .canonicalize()
                .map(|p| p.join(f))
                .unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        })
}

/// Stable key for a path: FNV-1a 64 over the canonicalized path string.
pub fn key(path: &Path) -> String {
    let canon = canonical(path);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn entry(path: &Path, ext: &str) -> PathBuf {
    dir().join(format!("{}.{ext}", key(path)))
}

/// Take the per-file exclusive lock (held for the engine's lifetime — drop the
/// returned handle to release). Fails when another process serves this file.
pub fn lock_exclusive(path: &Path) -> Result<File> {
    std::fs::create_dir_all(dir())?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(entry(path, "lock"))?;
    f.try_lock_exclusive()
        .map_err(|_| anyhow!("this app is already open in another window"))?;
    Ok(f)
}

/// Advertise the running server for double-open reuse.
pub fn write_addr(path: &Path, port: u16, token: &str) {
    let _ = std::fs::create_dir_all(dir());
    let addr = entry(path, "addr");
    let body = serde_json::json!({"port": port, "token": token, "pid": std::process::id()});
    let _ = std::fs::write(&addr, body.to_string());
    // The .addr holds the session token — keep it owner-readable only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&addr, std::fs::Permissions::from_mode(0o600));
    }
}

/// The advertised server for this path, if any: `(port, token)`.
/// Liveness is the caller's problem (health check).
pub fn read_addr(path: &Path) -> Option<(u16, String)> {
    let body = std::fs::read_to_string(entry(path, "addr")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some((v["port"].as_u64()? as u16, v["token"].as_str()?.to_string()))
}

pub fn remove_addr(path: &Path) {
    let _ = std::fs::remove_file(entry(path, "addr"));
}

/// Drop every record for a path (addr, app map, lock file, snapshots). For
/// paths whose FILE is being deleted (a scratch consumed by Save As): its
/// content lives on in the saved file, and a new file at the same path must
/// not inherit the old one's snapshots. Only call with the path's lock
/// released.
pub fn forget(path: &Path) {
    let _ = std::fs::remove_dir_all(snapshot_dir(path));
    for ext in ["addr", "app", "lock"] {
        let _ = std::fs::remove_file(entry(path, ext));
    }
}

/// An app_id as uApp writes them (16 letters and digits), or close enough to
/// be safe in a file name. Anything else in a file is treated as unknown.
pub fn valid_app_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Where this path's rolling snapshots live.
pub fn snapshot_dir(path: &Path) -> PathBuf {
    local_root().join("snapshots").join(key(path))
}

/// Remember which app_id lives at this path (and whether it was encrypted),
/// so a later open can tell file damage apart from a missing/wrong password
/// even when the .uapp itself has become unreadable.
pub fn write_app_map(path: &Path, app_id: &str, encrypted: bool) {
    let _ = std::fs::create_dir_all(dir());
    let kind = if encrypted { "enc" } else { "plain" };
    let _ = std::fs::write(entry(path, "app"), format!("{app_id} {kind}"));
}

fn read_app_map(path: &Path) -> Option<(String, bool)> {
    let body = std::fs::read_to_string(entry(path, "app")).ok()?;
    let mut it = body.split_whitespace();
    let app_id = it.next()?.to_string();
    if !valid_app_id(&app_id) {
        return None;
    }
    Some((app_id, it.next() == Some("enc")))
}

/// Was this path's app encrypted the last time it was served here?
/// None = we've never served it.
pub fn known_encrypted(path: &Path) -> Option<bool> {
    read_app_map(path).map(|(_, enc)| enc)
}

/// Where older builds kept this path's snapshots
/// (`<data_local>/uapp/<app_id>/snapshots`), for a file last opened by one of
/// them. Shared by every copy of the app, so it is only a fallback.
pub fn legacy_snapshot_dir(path: &Path) -> Option<PathBuf> {
    let (app_id, _) = read_app_map(path)?;
    let d = local_root().join(app_id).join("snapshots");
    d.is_dir().then_some(d)
}
