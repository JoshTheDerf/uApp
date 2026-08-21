//! Machine-local preferences (NOT synced into the shared .uapp file). These
//! describe how *this* user/device opens apps — e.g. system browser vs. the
//! native desktop window — so they must not live in the app config inside the file.
//! Stored as JSON at <config_dir>/uapp/prefs.json.

use anyhow::Result;
use serde_json::{json, Value};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

/// wasm: prefs are per-browser and session-scoped (kept in memory; the shell
/// persists what matters in localStorage itself).
#[cfg(target_arch = "wasm32")]
static MEM: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

#[cfg(target_arch = "wasm32")]
pub fn load() -> Value {
    MEM.lock().unwrap().clone().unwrap_or_else(|| json!({}))
}

#[cfg(target_arch = "wasm32")]
pub fn set(key: &str, value: Value) -> Result<()> {
    let mut g = MEM.lock().unwrap();
    let mut v = g.clone().unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert(key.to_string(), value);
    }
    *g = Some(v);
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn prefs_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uapp")
        .join("prefs.json")
}

/// The whole prefs object (defaults to `{}`).
#[cfg(not(target_arch = "wasm32"))]
pub fn load() -> Value {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

/// A single string pref with a fallback.
pub fn get_str(key: &str, default: &str) -> String {
    load()
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// A single pref as raw JSON (None when unset) — for structured values like
/// the shortcut install records, which aren't plain strings.
pub fn get_value(key: &str) -> Option<Value> {
    load().get(key).cloned()
}

/// Merge one key into the prefs file.
#[cfg(not(target_arch = "wasm32"))]
pub fn set(key: &str, value: Value) -> Result<()> {
    let mut v = load();
    if let Some(o) = v.as_object_mut() {
        o.insert(key.to_string(), value);
    }
    let p = prefs_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
    Ok(())
}

/// Which shell opens apps on this machine: "native" (the UApp desktop window
/// — the primary path and the default; the CLI delegates to the Tauri app) or
/// "browser" (the fallback shell, explicit opt-out — also what the CLI uses,
/// with a note, when the UApp app isn't installed).
pub fn shell() -> String {
    get_str("shell", "native")
}
