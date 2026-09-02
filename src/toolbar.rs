//! The **toolbar**: the bar of app controls the shell wraps around the app
//! frame — app name, Files, Database, Tools, Settings, chat, reload — plus the
//! panels those buttons open. Hiding it leaves the app on its own, filling the
//! window, which is what an app being *used* rather than *built* wants.
//!
//! Two things live here, and keeping them apart is the whole point:
//!
//!   * The **saved default** (`hidden`, and the `shortcut` that toggles it)
//!     lives in the app's config inside the .uapp, so it travels with the
//!     file: an app can ship as a plain-looking app with no bar around it.
//!   * **Visibility right now** is session state and is written NOWHERE.
//!     Opening a hidden toolbar to change one thing must not quietly change
//!     what the app looks like the next time it is opened, so every open
//!     starts from the saved default again.
//!
//! ("Shortcut" here is a keystroke. `shortcut.rs` is the unrelated business of
//! installing OS launcher entries.)

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// F9 because it is the one key that is free everywhere: no browser binds it
/// (unlike Ctrl+Shift+B, which is Firefox's bookmarks library and Chrome's
/// bookmarks bar), no desktop grabs it before the page sees it (unlike
/// Ctrl+Alt+T on GNOME), and no keyboard layout needs it to type a character
/// (unlike Ctrl+Alt+<letter>, which is AltGr on Windows).
pub const DEFAULT_SHORTCUT: &str = "F9";

/// At most this many alternatives in one shortcut string. A handful covers
/// "F9, Mod+Alt+B" for people whose F-keys are media keys; a hundred is
/// someone pasting junk into the field.
const MAX_ALTERNATIVES: usize = 4;

/// Keys that may be a shortcut all by themselves. Everything else needs a
/// modifier: a bare letter would fire on every `b` typed into the app.
fn standalone(key: &str) -> bool {
    matches!(key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()), Some(1..=24))
}

/// Named keys, lowercase input -> the canonical spelling we store and the
/// shell matches against `KeyboardEvent.key`.
const NAMED: &[(&str, &str)] = &[
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("enter", "Enter"),
    ("return", "Enter"),
    ("tab", "Tab"),
    ("space", "Space"),
    ("backspace", "Backspace"),
    ("delete", "Delete"),
    ("del", "Delete"),
    ("insert", "Insert"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PageUp"),
    ("pagedown", "PageDown"),
    ("arrowup", "ArrowUp"),
    ("up", "ArrowUp"),
    ("arrowdown", "ArrowDown"),
    ("down", "ArrowDown"),
    ("arrowleft", "ArrowLeft"),
    ("left", "ArrowLeft"),
    ("arrowright", "ArrowRight"),
    ("right", "ArrowRight"),
];

/// Check and canonicalize a shortcut string: `"f9"` -> `"F9"`,
/// `"cmd+alt+b"` -> `"Mod+Alt+B"`, `"F9, mod+alt+b"` -> `"F9, Mod+Alt+B"`.
/// Empty (or all-blank) means "no shortcut" and is allowed — that is how the
/// toggle is switched off.
///
/// `Mod` is the portable modifier: Cmd on macOS, Ctrl everywhere else. A
/// recorded shortcut is stored that way so the same app opens with a shortcut
/// that reads right on whichever machine opens it.
pub fn normalize_shortcut(s: &str) -> Result<String> {
    let mut out: Vec<String> = Vec::new();
    for combo in s.split(',') {
        let combo = combo.trim();
        if combo.is_empty() {
            continue;
        }
        if out.len() == MAX_ALTERNATIVES {
            bail!("at most {MAX_ALTERNATIVES} shortcuts, separated by commas");
        }
        out.push(normalize_combo(combo)?);
    }
    Ok(out.join(", "))
}

fn normalize_combo(combo: &str) -> Result<String> {
    let mut parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    // A shortcut ending in the "+" key: "Mod++" splits to ["Mod", "", ""].
    if parts.len() > 1 && parts.last() == Some(&"") {
        parts.pop();
        *parts.last_mut().unwrap() = "+";
    }
    let key = parts.pop().filter(|k| !k.is_empty()).unwrap_or_default();
    let (mut m, mut c, mut a, mut sh, mut me) = (false, false, false, false, false);
    for p in parts {
        match p.to_ascii_lowercase().as_str() {
            "" => bail!("\"{combo}\" has an empty part"),
            "mod" => m = true,
            "ctrl" | "control" => c = true,
            "alt" | "option" | "opt" => a = true,
            "shift" => sh = true,
            "meta" | "cmd" | "command" | "super" | "win" => me = true,
            other => bail!("\"{other}\" is not a modifier (use Mod, Ctrl, Alt, Shift or Meta)"),
        }
    }
    let lower = key.to_ascii_lowercase();
    let key = if let Some((_, canon)) = NAMED.iter().find(|(alias, _)| *alias == lower) {
        (*canon).to_string()
    } else if standalone(&lower) {
        lower.to_uppercase()
    } else if key.chars().count() == 1 {
        key.to_uppercase()
    } else if key.is_empty() {
        bail!("\"{combo}\" has modifiers but no key");
    } else {
        bail!("\"{key}\" is not a key we can match (try a letter, a digit or F1-F12)");
    };
    if !(m || c || a || sh || me) && !standalone(&key.to_ascii_lowercase()) {
        bail!("\"{key}\" on its own would fire while typing in the app — add Ctrl, Alt or Shift, or use an F-key");
    }
    // Fixed order, so the same shortcut always reads and compares the same way.
    let mut s = String::new();
    for (on, name) in [(m, "Mod"), (c, "Ctrl"), (me, "Meta"), (a, "Alt"), (sh, "Shift")] {
        if on {
            s.push_str(name);
            s.push('+');
        }
    }
    s.push_str(&key);
    Ok(s)
}

/// The saved default, with everything filled in — what a shell starts from.
/// A config value written by an older/other version (or by hand) is tolerated
/// field by field rather than rejected: a bad shortcut falls back to the
/// default one instead of leaving the app with no way to bring the bar back.
pub fn settings(stored: Option<&Value>) -> Value {
    let hidden = stored
        .and_then(|v| v.get("hidden"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let shortcut = stored
        .and_then(|v| v.get("shortcut"))
        .and_then(Value::as_str)
        .and_then(|s| normalize_shortcut(s).ok())
        .unwrap_or_else(|| DEFAULT_SHORTCUT.to_string());
    json!({"hidden": hidden, "shortcut": shortcut})
}

/// Apply a patch (either field, or both) to the saved default. Unknown fields
/// are an error rather than a silent no-op — a typo'd `hide: true` that
/// reported success would look like the setting simply did not work.
pub fn merge(stored: Option<&Value>, patch: &Value) -> Result<Value> {
    let mut out = settings(stored);
    if let Some(obj) = patch.as_object() {
        for k in obj.keys() {
            if !matches!(k.as_str(), "hidden" | "shortcut") {
                bail!("unknown toolbar setting \"{k}\" (expected hidden and/or shortcut)");
            }
        }
    }
    let mut touched = false;
    if let Some(h) = patch.get("hidden") {
        let h = h.as_bool().ok_or_else(|| anyhow::anyhow!("hidden must be true or false"))?;
        out["hidden"] = json!(h);
        touched = true;
    }
    if let Some(s) = patch.get("shortcut") {
        let s = s.as_str().ok_or_else(|| anyhow::anyhow!("shortcut must be a string"))?;
        out["shortcut"] = json!(normalize_shortcut(s)?);
        touched = true;
    }
    if !touched {
        bail!("nothing to change (pass hidden and/or shortcut)");
    }
    Ok(out)
}

/// The panels the toolbar's buttons open, named for what they are rather than
/// for the element ids the shell happens to use (`sidebar`, `sqlpanel`, …).
/// These are the names in the tool schema, so they are also what the model
/// sees.
pub const PANELS: &[&str] = &["chat", "files", "database", "settings", "tools"];

/// The other names people and models reach for. Cheap to accept, and the
/// alternative is a tool call that fails on a synonym: the panel is titled
/// "Database" but its query tab says SQL, so both are the obvious word for it.
const PANEL_ALIASES: &[(&str, &str)] = &[
    ("sql", "database"),
    ("db", "database"),
    ("data", "database"),
    ("file", "files"),
    ("filebrowser", "files"),
    ("file browser", "files"),
    ("ai", "chat"),
    ("assistant", "chat"),
    ("setting", "settings"),
    ("tool", "tools"),
];

/// Resolve a panel name to its canonical form, or say what the choices are.
pub fn normalize_panel(name: &str) -> Result<&'static str> {
    let want = name.trim().to_ascii_lowercase();
    if let Some(p) = PANELS.iter().find(|p| **p == want) {
        return Ok(p);
    }
    if let Some((_, canon)) = PANEL_ALIASES.iter().find(|(alias, _)| *alias == want) {
        return Ok(canon);
    }
    bail!("no panel called \"{name}\" (one of: {})", PANELS.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_spelling_so_the_same_shortcut_compares_equal() {
        for (input, want) in [
            ("f9", "F9"),
            ("F9", "F9"),
            // An explicit Cmd means Cmd; `Mod` is the portable one, and it
            // is what the shortcut recorder writes when someone presses the
            // platform's own modifier.
            ("cmd+alt+b", "Meta+Alt+B"),
            ("Command + Option + B", "Meta+Alt+B"),
            ("mod+alt+b", "Mod+Alt+B"),
            ("control+shift+k", "Ctrl+Shift+K"),
            // Modifier order in, fixed order out.
            ("shift+alt+mod+j", "Mod+Alt+Shift+J"),
            ("mod+arrowup", "Mod+ArrowUp"),
            ("mod+up", "Mod+ArrowUp"),
            ("alt+esc", "Alt+Escape"),
            ("mod+space", "Mod+Space"),
            ("f9, mod+alt+b", "F9, Mod+Alt+B"),
            // Blank means "no shortcut" — the toggle is off.
            ("", ""),
            ("  ", ""),
            (" , ", ""),
        ] {
            assert_eq!(normalize_shortcut(input).unwrap(), want, "input {input:?}");
        }
    }

    #[test]
    fn a_bare_printable_key_is_refused() {
        // It would fire on every "b" typed into the app, and the app frame is
        // where the typing happens.
        for bad in ["b", "B", "1", ".", "space", "enter", "escape"] {
            let e = normalize_shortcut(bad).unwrap_err().to_string();
            assert!(e.contains("would fire while typing"), "{bad:?} -> {e}");
        }
        // With a modifier the same keys are fine.
        assert_eq!(normalize_shortcut("mod+b").unwrap(), "Mod+B");
        assert_eq!(normalize_shortcut("shift+escape").unwrap(), "Shift+Escape");
    }

    #[test]
    fn f_keys_stand_alone_but_only_the_ones_that_exist() {
        assert_eq!(normalize_shortcut("f1").unwrap(), "F1");
        assert_eq!(normalize_shortcut("f24").unwrap(), "F24");
        // F0 and F25 are not keys; they must not slip through as "some F-key".
        assert!(normalize_shortcut("f0").is_err());
        assert!(normalize_shortcut("f25").is_err());
        assert!(normalize_shortcut("f").is_err());
    }

    #[test]
    fn junk_is_reported_not_stored() {
        for bad in [
            "ctrl+",             // modifiers, no key
            "hyper+b",           // not a modifier
            "mod+notakey",       // not a key
            "mod++b",            // empty part
            "f9, f8, f7, f6, f5" // more than we accept
        ] {
            assert!(normalize_shortcut(bad).is_err(), "{bad:?} was accepted");
        }
        // "+" as the key itself still works.
        assert_eq!(normalize_shortcut("mod++").unwrap(), "Mod++");
    }

    #[test]
    fn defaults_fill_in_and_a_broken_stored_shortcut_falls_back() {
        assert_eq!(settings(None), json!({"hidden": false, "shortcut": "F9"}));
        assert_eq!(
            settings(Some(&json!({"hidden": true}))),
            json!({"hidden": true, "shortcut": "F9"}),
        );
        // Hand-edited nonsense must not leave the app with no way back.
        assert_eq!(
            settings(Some(&json!({"hidden": true, "shortcut": "b"}))),
            json!({"hidden": true, "shortcut": "F9"}),
        );
        assert_eq!(
            settings(Some(&json!({"hidden": true, "shortcut": ""}))),
            json!({"hidden": true, "shortcut": ""}),
        );
    }

    #[test]
    fn panel_names_resolve_including_the_obvious_synonyms() {
        for (input, want) in [
            ("chat", "chat"),
            ("Chat", "chat"),
            ("  files  ", "files"),
            ("database", "database"),
            // The panel says "Database" at the top and "SQL" on its query tab;
            // both are the natural word, and a tool call should not fail over it.
            ("sql", "database"),
            ("SQL", "database"),
            ("file browser", "files"),
            ("ai", "chat"),
        ] {
            assert_eq!(normalize_panel(input).unwrap(), want, "input {input:?}");
        }
        let e = normalize_panel("sidebar").unwrap_err().to_string();
        // Not the element id — and the error has to say what IS accepted.
        assert!(e.contains("no panel called"), "{e}");
        assert!(e.contains("chat, files, database, settings, tools"), "{e}");
    }

    #[test]
    fn merge_touches_only_what_was_passed() {
        let stored = json!({"hidden": true, "shortcut": "Mod+Alt+B"});
        assert_eq!(
            merge(Some(&stored), &json!({"hidden": false})).unwrap(),
            json!({"hidden": false, "shortcut": "Mod+Alt+B"}),
        );
        assert_eq!(
            merge(Some(&stored), &json!({"shortcut": "f9"})).unwrap(),
            json!({"hidden": true, "shortcut": "F9"}),
        );
        // A typo'd field would otherwise report success and change nothing.
        assert!(merge(Some(&stored), &json!({"hide": true})).is_err());
        assert!(merge(Some(&stored), &json!({})).is_err());
        assert!(merge(Some(&stored), &json!({"hidden": "yes"})).is_err());
        assert!(merge(Some(&stored), &json!({"shortcut": "b"})).is_err());
    }
}
