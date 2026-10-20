//! What belongs to the person at this machine rather than to the shared .uapp
//! file: the AI provider's API key, MCP server credentials, and the "always
//! allow" answers for tools an app calls. All three used to live in the file's
//! `uapp_config`, so a copy sent to someone else carried them along (or
//! arrived with approvals already planted), and app code could read the key
//! with `config.get` or plain SQL.
//!
//! Native builds keep them in `<config_dir>/uapp/secrets/<key>.json`, one file
//! per .uapp, where `<key>` is the file's registry key (a hash of its path), so
//! a copy of the file somewhere else starts with nothing. Each one is
//! owner-only (0600) and replaced atomically.
//!
//! Each credential is bound to where it may be sent, so app code that can
//! rewrite the `ai` or `mcp` config can't redirect it: the API key is handed
//! out only while the config still names the provider and base URL it was
//! entered for, and MCP headers only go to the URL they were saved with.
//!
//! The browser (wasm) build has no machine-local storage, so there the values
//! stay in the file as before. `config.get` redacts them on both builds.
//!
//! Also here: the session/publish token helpers, since a token is the other
//! secret the server handles.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::app::App;

// ---- tokens -----------------------------------------------------------------

/// A fresh session token: 32 bytes from the OS CSPRNG, hex-encoded (safe in a
/// URL, a header and a cookie as is).
#[cfg(not(target_arch = "wasm32"))]
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("the OS random number generator failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compare a presented token with the real one in time that doesn't depend on
/// where they first differ. (The length can leak; it isn't secret.)
pub fn token_eq(given: &str, want: &str) -> bool {
    let (a, b) = (given.as_bytes(), want.as_bytes());
    if a.len() != b.len() || b.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Refuse a `--token` / `UAPP_TOKEN` that can't travel in a cookie or an
/// `Authorization` header. Checked at startup: such a token used to panic the
/// first request that set the cookie.
pub fn check_token(t: &str) -> Result<()> {
    if t.is_empty() {
        bail!("the token is empty");
    }
    // RFC 6265 cookie-octet: visible ASCII minus space, `"`, `,`, `;`, `\`.
    if let Some(c) = t
        .chars()
        .find(|c| !c.is_ascii_graphic() || matches!(c, '"' | ',' | ';' | '\\'))
    {
        bail!(
            "the token contains {c:?}; use letters, digits and -._~ (e.g. `openssl rand -hex 32`)"
        );
    }
    Ok(())
}

// ---- storage ----------------------------------------------------------------

/// Write `bytes` to `path` so it is never readable by anyone but this user,
/// not even for a moment: a temp file created 0600 in the same directory,
/// fsync'd, then renamed over the target. A crash leaves the old file or the
/// new one, never half of either.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("secret"),
        std::process::id(),
        fastrand::u64(..)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(target_arch = "wasm32"))]
mod local {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// One process serves one file, but its RPCs run on many threads.
    static LOCK: Mutex<()> = Mutex::new(());

    fn dir() -> PathBuf {
        dirs::config_dir()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
            .join("uapp")
            .join("secrets")
    }

    fn file(key: &str) -> PathBuf {
        dir().join(format!("{key}.json"))
    }

    pub fn load(key: &str) -> Value {
        let _g = crate::app::lock(&LOCK);
        read(key)
    }

    fn read(key: &str) -> Value {
        std::fs::read_to_string(file(key))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    /// Read-modify-write one file's record. An empty record removes the file.
    pub fn update(key: &str, f: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Result<()> {
        let _g = crate::app::lock(&LOCK);
        let mut v = read(key);
        let Some(o) = v.as_object_mut() else {
            unreachable!("read() always returns an object")
        };
        f(o);
        o.retain(|_, v| !v.is_null() && v != &json!({}));
        if o.is_empty() {
            match std::fs::remove_file(file(key)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir());
        }
        write_private(&file(key), serde_json::to_string_pretty(&v)?.as_bytes())?;
        Ok(())
    }

    /// The record key of the file `app` has open.
    pub fn key(app: &App) -> String {
        crate::registry::key(&app.eng().path)
    }

    pub fn copy(from: &str, to: &str, remove_old: bool) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let rec = load(from);
        if rec.as_object().is_some_and(|o| !o.is_empty()) {
            update(to, |o| {
                if let Some(src) = rec.as_object() {
                    *o = src.clone();
                }
            })?;
        }
        if remove_old {
            update(from, |o| o.clear())?;
        }
        Ok(())
    }
}

// ---- the AI key -------------------------------------------------------------

/// Where a key may be sent: the provider and base URL of an `ai` config.
fn endpoint(cfg: &Value) -> (String, String) {
    (
        cfg["provider"].as_str().unwrap_or("").trim().to_string(),
        cfg["base_url"]
            .as_str()
            .unwrap_or("")
            .trim()
            .trim_end_matches('/')
            .to_string(),
    )
}

/// The API key to send with requests made under `cfg` (the file's `ai`
/// config), or "" when there is none for that provider and base URL. Don't
/// hold the engine lock while calling.
pub fn ai_key(app: &App, cfg: &Value) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let rec = &local::load(&local::key(app))["ai"];
        if rec.is_object() && endpoint(rec) == endpoint(cfg) {
            return rec["api_key"].as_str().unwrap_or("").to_string();
        }
        String::new()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        cfg["api_key"].as_str().unwrap_or("").to_string()
    }
}

/// `config.get "ai"`: the stored config without the key, plus whether one is
/// saved, so the settings panel can say so without ever seeing it.
pub fn redact_ai(app: &App, mut cfg: Value) -> Value {
    if !cfg.is_object() {
        return cfg;
    }
    let has = !ai_key(app, &cfg).is_empty();
    if let Some(o) = cfg.as_object_mut() {
        o.remove("api_key");
        o.insert("has_api_key".into(), json!(has));
    }
    cfg
}

/// `config.set "ai"`: file the key where it belongs and return what to store
/// in the file. `api_key` in `new`:
///   * a non-empty string saves that key for the new provider + base URL;
///   * `null` removes the saved key;
///   * absent or "" keeps the saved key, but only if the provider and base URL
///     are the ones it was saved for. Changing either without giving the key
///     again drops it, so app code can't point the user's key at its own
///     server.
///
/// Don't hold the engine lock while calling.
pub fn prepare_ai(app: &App, mut new: Value) -> Result<Value> {
    let given = match new.get("api_key") {
        Some(Value::String(k)) if !k.trim().is_empty() => Some(Some(k.trim().to_string())),
        Some(Value::Null) => Some(None),
        _ => None,
    };
    if let Some(o) = new.as_object_mut() {
        o.remove("api_key");
        o.remove("has_api_key");
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let key = local::key(app);
        if !new.is_object() {
            local::update(&key, |o| {
                o.remove("ai");
            })?;
            return Ok(new);
        }
        let (provider, base_url) = endpoint(&new);
        local::update(&key, |o| {
            let keep = match &given {
                Some(k) => k.clone(),
                None => o
                    .get("ai")
                    .filter(|rec| endpoint(rec) == (provider.clone(), base_url.clone()))
                    .and_then(|rec| rec["api_key"].as_str())
                    .map(str::to_string),
            };
            match keep {
                Some(k) => {
                    o.insert(
                        "ai".into(),
                        json!({"api_key": k, "provider": provider, "base_url": base_url}),
                    );
                }
                None => {
                    o.remove("ai");
                }
            }
        })?;
        Ok(new)
    }
    #[cfg(target_arch = "wasm32")]
    {
        if !new.is_object() {
            return Ok(new);
        }
        let old = crate::store::config_get(&app.eng().db, "ai")?.unwrap_or(Value::Null);
        let keep = match given {
            Some(k) => k,
            None if old.is_object() && endpoint(&old) == endpoint(&new) => old["api_key"]
                .as_str()
                .filter(|k| !k.is_empty())
                .map(str::to_string),
            None => None,
        };
        if let (Some(k), Some(o)) = (keep, new.as_object_mut()) {
            o.insert("api_key".into(), json!(k));
        }
        Ok(new)
    }
}

// ---- MCP credentials --------------------------------------------------------

/// The file's MCP server list with each server's credentials filled in from
/// this machine, where the saved URL still matches. Credentials found in the
/// file itself are ignored on native builds (they are moved out on open).
pub fn mcp_with_credentials(app: &App, servers: Vec<Value>) -> Vec<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let creds = local::load(&local::key(app))["mcp"].clone();
        servers
            .into_iter()
            .map(|mut s| {
                if let Some(o) = s.as_object_mut() {
                    o.remove("headers");
                }
                let name = s["name"].as_str().unwrap_or("");
                let c = &creds[name];
                if c["url"].is_string() && c["url"] == s["url"] && c["headers"].is_object() {
                    s["headers"] = c["headers"].clone();
                }
                s
            })
            .collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        servers
    }
}

/// Before an MCP server list is written to the file: keep each server's
/// `headers` on this machine (bound to its URL) and return the list without
/// them. A server given without headers keeps the ones saved for it if its URL
/// is unchanged; credentials of servers that are gone are dropped.
pub fn mcp_store_credentials(app: &App, servers: Vec<Value>) -> Result<Vec<Value>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let key = local::key(app);
        let old = local::load(&key)["mcp"].clone();
        let mut creds = serde_json::Map::new();
        let mut out = Vec::with_capacity(servers.len());
        for mut s in servers {
            let name = s["name"].as_str().unwrap_or("").to_string();
            let headers = s
                .as_object_mut()
                .and_then(|o| {
                    o.remove("hasAuth");
                    o.remove("headers")
                })
                .filter(|h| h.as_object().is_some_and(|h| !h.is_empty()));
            if let Some(h) = headers {
                creds.insert(name, json!({"url": s["url"], "headers": h}));
            } else if old[&name]["url"] == s["url"] && old[&name]["headers"].is_object() {
                let saved = old[&name].clone();
                creds.insert(name, saved);
            }
            out.push(s);
        }
        local::update(&key, |o| {
            o.insert("mcp".into(), Value::Object(creds));
        })?;
        Ok(out)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        Ok(servers)
    }
}

/// `config.get "mcp"`: the list without credentials, plus whether each server
/// has some.
pub fn redact_mcp(app: &App, servers: Value) -> Value {
    let Some(list) = servers.as_array() else {
        return servers;
    };
    json!(mcp_with_credentials(app, list.clone())
        .into_iter()
        .map(|mut s| {
            let has = s
                .as_object_mut()
                .and_then(|o| o.remove("headers"))
                .is_some();
            if s.is_object() {
                s["hasAuth"] = json!(has);
            }
            s
        })
        .collect::<Vec<_>>())
}

// ---- "always allow" for app-triggered tools ----------------------------------

/// Has the user answered "always allow" for `tool` when this app calls it?
/// On native builds only this machine's answers count: approvals stored in
/// the file are ignored, since anyone can plant them there.
pub fn app_approved(app: &App, app_id: &str, tool: &str) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        local::load(&local::key(app))["approvals"][app_id]
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t.as_str() == Some(tool)))
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::store::check_app_approval(&app.eng().db, app_id, tool)
    }
}

/// Remember "always allow" for `tool` when this app calls it.
pub fn approve_app(app: &App, app_id: &str, tool: &str) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        local::update(&local::key(app), |o| {
            let all = o.entry("approvals").or_insert_with(|| json!({}));
            if !all.is_object() {
                *all = json!({});
            }
            let list = &mut all[app_id];
            if !list.is_array() {
                *list = json!([]);
            }
            if let Some(a) = list.as_array_mut() {
                if !a.iter().any(|t| t.as_str() == Some(tool)) {
                    a.push(json!(tool));
                }
            }
        })
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::store::add_app_approval(&app.eng().db, app_id, tool)
    }
}

/// Config keys that only this module may write: `approvals.app.<id>` lived
/// in the file before and is ignored there now.
pub fn reserved_config_key(key: &str) -> bool {
    key.starts_with("approvals.")
}

// ---- lifecycle --------------------------------------------------------------

/// On open: move an API key and MCP credentials that an older build left in
/// the file to this machine, and take them out of the file. The local copy is
/// written first, so a failure part way loses nothing. Approvals found in the
/// file are left where they are and never read.
#[cfg(not(target_arch = "wasm32"))]
pub fn migrate(app: &App) -> Result<()> {
    let key = local::key(app);
    let (ai, mcp) = {
        let eng = app.eng();
        (
            crate::store::config_get(&eng.db, "ai")?,
            crate::store::config_get(&eng.db, "mcp")?,
        )
    };
    if let Some(mut ai) = ai.filter(|v| v["api_key"].as_str().is_some_and(|k| !k.is_empty())) {
        let (provider, base_url) = endpoint(&ai);
        let k = ai["api_key"].as_str().unwrap_or("").to_string();
        local::update(&key, |o| {
            o.insert(
                "ai".into(),
                json!({"api_key": k, "provider": provider, "base_url": base_url}),
            );
        })?;
        if let Some(o) = ai.as_object_mut() {
            o.remove("api_key");
        }
        app.eng()
            .local_op("config_set", json!({"key": "ai", "value": ai}))?;
    }
    if let Some(list) = mcp.as_ref().and_then(Value::as_array) {
        if list.iter().any(|s| s.get("headers").is_some()) {
            let stripped = mcp_store_credentials(app, list.clone())?;
            app.eng()
                .local_op("config_set", json!({"key": "mcp", "value": stripped}))?;
        }
    }
    Ok(())
}

/// The open file moved to `to` (Save As): bring this machine's secrets along.
/// `consumed` = the old file is gone (a scratch app saved for the first time),
/// so its record goes too; otherwise both files keep one.
#[cfg(not(target_arch = "wasm32"))]
pub fn follow_save_as(from: &std::path::Path, to: &std::path::Path, consumed: bool) {
    let (a, b) = (crate::registry::key(from), crate::registry::key(to));
    if let Err(e) = local::copy(&a, &b, consumed) {
        eprintln!("uapp: could not carry this app's saved keys over to the new file: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abd", "abc"));
        assert!(!token_eq("ab", "abc"));
        assert!(!token_eq("", ""));
    }

    #[test]
    fn tokens_that_cannot_be_a_cookie_are_refused() {
        assert!(check_token("test-token-0123456789").is_ok());
        assert!(check_token(&new_token()).is_ok());
        for bad in ["", "a b", "a;b", "a\nb", "a\"b", "é"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
        assert_eq!(new_token().len(), 64);
        assert_ne!(new_token(), new_token());
    }

    #[test]
    fn the_key_is_bound_to_provider_and_base_url() {
        let a = json!({"provider": "openai", "base_url": "https://api.example.com/v1/"});
        let b = json!({"provider": "openai", "base_url": "https://api.example.com/v1"});
        let c = json!({"provider": "openai", "base_url": "https://evil.example/v1"});
        assert_eq!(endpoint(&a), endpoint(&b));
        assert_ne!(endpoint(&a), endpoint(&c));
        assert_ne!(endpoint(&a), endpoint(&json!({"provider": "zai"})));
    }
}
