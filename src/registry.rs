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
//!   <data_local>/uapp/servers/<key>.app   — the file's app_id, so the rolling
//!                                           snapshots can be located even when
//!                                           the .uapp itself is unreadable
//!
//! `<key>` is a hash of the canonicalized path — computable before the file is
//! opened (and for encrypted files), which is what server discovery needs.

use anyhow::{anyhow, Result};
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};

fn dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uapp")
        .join("servers")
}

/// Stable key for a path: FNV-1a 64 over the canonicalized path string. The
/// file may not exist yet (fresh scratch), so fall back to canonicalizing the
/// parent and re-joining the file name.
pub fn key(path: &Path) -> String {
    let canon = path.canonicalize().unwrap_or_else(|_| {
        match (path.parent(), path.file_name()) {
            (Some(p), Some(f)) => p.canonicalize().map(|p| p.join(f)).unwrap_or_else(|_| path.to_path_buf()),
            _ => path.to_path_buf(),
        }
    });
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

/// Drop every registry record for a path (addr, app map, lock file). For
/// paths whose FILE is being deleted (a scratch consumed by Save As) — a
/// stale app map there could otherwise point a future corrupt-restore at the
/// wrong app's snapshots. Only call with the path's lock released.
pub fn forget(path: &Path) {
    for ext in ["addr", "app", "lock"] {
        let _ = std::fs::remove_file(entry(path, ext));
    }
}

/// Remember which app_id lives at this path (and whether it was encrypted),
/// so a later open can find the app's snapshots — and tell file damage apart
/// from a missing/wrong password — even when the .uapp itself has become
/// unreadable.
pub fn write_app_map(path: &Path, app_id: &str, encrypted: bool) {
    let _ = std::fs::create_dir_all(dir());
    let kind = if encrypted { "enc" } else { "plain" };
    let _ = std::fs::write(entry(path, "app"), format!("{app_id} {kind}"));
}

fn read_app_map(path: &Path) -> Option<(String, bool)> {
    let body = std::fs::read_to_string(entry(path, "app")).ok()?;
    let mut it = body.split_whitespace();
    let app_id = it.next()?.to_string();
    if app_id.is_empty() || app_id.contains(['/', '\\']) {
        return None;
    }
    Some((app_id, it.next() == Some("enc")))
}

/// Was this path's app encrypted the last time it was served here?
/// None = we've never served it.
pub fn known_encrypted(path: &Path) -> Option<bool> {
    read_app_map(path).map(|(_, enc)| enc)
}

/// The per-app local state dir (`<data_local>/uapp/<app_id>`) recorded for this
/// path, when we've served it before and the dir still exists.
pub fn known_local_dir(path: &Path) -> Option<PathBuf> {
    let (app_id, _) = read_app_map(path)?;
    let d = dirs::data_local_dir()?.join("uapp").join(app_id);
    d.is_dir().then_some(d)
}
