//! "Install" a single .uapp as a standalone-looking app: a launcher entry
//! (Linux applications menu, Windows Start Menu, macOS ~/Applications) that
//! runs `uapp <file>` — starting the server on demand, so the shortcut works
//! whether or not anything is currently running. No PWA/browser involved.
//!
//! Every install records the files it created in the machine-local prefs,
//! keyed by app id: file names are derived from the app *name*, which can be
//! renamed after installing, so the slug alone can't find the old files. A
//! re-install removes the recorded files first — no duplicate launcher is
//! left behind after a rename.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Can this platform create launcher entries at all?
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", windows, target_os = "macos"))
}

/// Create (or replace) the launcher entry. `icon_png` is an optional custom
/// icon (already validated as PNG by the caller). Returns a human-readable
/// message saying where it landed.
pub fn install(app_id: &str, name: &str, path: &Path, icon_png: Option<&[u8]>) -> Result<String> {
    let name = display_name(name);
    if !path.is_absolute() {
        bail!("the app file has no stable location yet");
    }
    // The previous install (possibly under an old name) owns files we know
    // only from the record — clear them before creating the new entry.
    if let Some((files, _)) = record(app_id) {
        for f in &files {
            remove_path(f);
        }
    }
    #[cfg(target_os = "linux")]
    let (msg, detail, files) = linux(name, path, icon_png)?;
    #[cfg(windows)]
    let (msg, detail, files) = windows(name, path, icon_png)?;
    #[cfg(target_os = "macos")]
    let (msg, detail, files) = macos(name, path, icon_png)?;
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = icon_png;
        bail!("launcher shortcuts aren't supported on this platform");
    }
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    {
        record_set(
            app_id,
            Some(json!({
                "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                "detail": detail,
            })),
        )?;
        Ok(msg)
    }
}

/// Is a launcher entry currently installed for this app, and where? Trusts
/// the recorded paths first; falls back to the current name's default path
/// (covers a record lost to e.g. a wiped prefs.json).
pub fn status(app_id: &str, name: &str) -> (bool, Option<String>) {
    if let Some((files, detail)) = record(app_id) {
        if files.iter().any(|f| f.exists()) {
            return (true, detail.or_else(|| files.first().map(|f| f.display().to_string())));
        }
    }
    if let Some(p) = default_entry(display_name(name)) {
        if p.exists() {
            return (true, Some(p.display().to_string()));
        }
    }
    (false, None)
}

/// Delete the launcher entry (recorded files, plus the current name's default
/// path in case the record was lost). Errors when nothing was installed.
pub fn remove(app_id: &str, name: &str) -> Result<String> {
    let name = display_name(name);
    let mut targets = record(app_id).map(|(f, _)| f).unwrap_or_default();
    if let Some(p) = default_entry(name) {
        if !targets.contains(&p) {
            targets.push(p);
        }
    }
    let removed = targets.iter().filter(|f| remove_path(f)).count();
    // A stale record (files already gone) is cleared either way.
    record_set(app_id, None)?;
    if removed == 0 {
        bail!("no launcher shortcut is installed for this app");
    }
    #[cfg(target_os = "linux")]
    if let Some(apps) = dirs::data_dir().map(|d| d.join("applications")) {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(&apps)
            .status();
    }
    Ok(format!("Removed the “{name}” launcher shortcut"))
}

/// Names may come in blank (a brand-new app); the entry still needs one.
fn display_name(name: &str) -> &str {
    let t = name.trim();
    if t.is_empty() { "uapp" } else { t }
}

/// Where an install of the *current* name would land — the status/remove
/// fallback when no record survives.
fn default_entry(name: &str) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    return dirs::data_dir()
        .map(|d| d.join("applications").join(format!("uapp-{}.desktop", slug(name).to_lowercase())));
    #[cfg(windows)]
    return dirs::data_dir().map(|d| {
        d.join("Microsoft\\Windows\\Start Menu\\Programs\\uapp")
            .join(format!("{}.lnk", sanitize_win(name)))
    });
    #[cfg(target_os = "macos")]
    return dirs::home_dir()
        .map(|h| h.join("Applications").join(format!("{}.app", sanitize_mac(name))));
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = name;
        None
    }
}

/// Delete one created entry: macOS installs are a directory bundle, the rest
/// plain files. Returns whether something actually existed and was removed.
fn remove_path(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p).is_ok(),
        Ok(_) => std::fs::remove_file(p).is_ok(),
        Err(_) => false,
    }
}

// ---- Install record in machine-local prefs: {"shortcuts": {<app_id>: …}} ---

fn record(app_id: &str) -> Option<(Vec<PathBuf>, Option<String>)> {
    let entry = crate::prefs::get_value("shortcuts")?.get(app_id).cloned()?;
    let files = entry["files"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(PathBuf::from))
        .collect();
    let detail = entry["detail"].as_str().map(String::from);
    Some((files, detail))
}

fn record_set(app_id: &str, entry: Option<Value>) -> Result<()> {
    let mut all = crate::prefs::get_value("shortcuts")
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    if let Some(o) = all.as_object_mut() {
        match entry {
            Some(e) => o.insert(app_id.to_string(), e),
            None => o.remove(app_id),
        };
    }
    crate::prefs::set("shortcuts", all)
}

/// Filesystem-safe version of the app name (also used as the entry id).
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "app".into() } else { s }
}

/// Wrap one PNG as a minimal .icns: modern icns entries (10.7+) may hold raw
/// PNG bytes, so no image conversion is needed — the type code is picked from
/// the PNG's pixel size (nearest not larger; Finder scales). Ungated so every
/// target compiles it and the unit test runs on Linux; only macOS calls it.
#[allow(dead_code)]
fn png_to_icns(png: &[u8]) -> Vec<u8> {
    let be = |off: usize| -> u32 {
        png.get(off..off + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0)
    };
    let side = be(16).max(be(20)); // IHDR width / height
    let kind: &[u8; 4] = if side >= 1024 {
        b"ic10"
    } else if side >= 512 {
        b"ic09"
    } else if side >= 256 {
        b"ic08"
    } else if side >= 128 {
        b"ic07"
    } else if side >= 64 {
        b"ic12"
    } else {
        b"ic11"
    };
    let mut icns = Vec::with_capacity(16 + png.len());
    icns.extend_from_slice(b"icns");
    icns.extend_from_slice(&((16 + png.len()) as u32).to_be_bytes());
    icns.extend_from_slice(kind);
    icns.extend_from_slice(&((8 + png.len()) as u32).to_be_bytes());
    icns.extend_from_slice(png);
    icns
}

/// Custom icons live outside the applications dir so a stray *.desktop scan
/// never mistakes one for an entry. The slug keeps the file name safe.
/// Ungated: `linux()` compiles on every target (only its CALL is gated), so
/// this must exist everywhere too — dead on platforms that never call it.
#[allow(dead_code)]
fn icon_path(name: &str, ext: &str) -> Result<PathBuf> {
    let dir = dirs::data_dir().context("no data dir")?.join("uapp").join("icons");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{}.{ext}", slug(name).to_lowercase())))
}

// ---- Linux / freedesktop: ~/.local/share/applications/uapp-<slug>.desktop --

/// .desktop string value: the name comes from the (shared, untrusted) .uapp
/// meta, so control chars — which would let it inject extra `Key=` lines —
/// are dropped, and backslashes are doubled per the spec's escape rules.
fn desktop_string(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| if c == '\\' { vec!['\\', '\\'] } else { vec![c] })
        .collect()
}

/// One quoted Exec= argument. Inside double quotes the spec reserves
/// `"` `` ` `` `$` `\` (backslash-escaped, with the backslash itself doubled
/// again by the string-escape rule) and `%` is a field code, written `%%`.
fn desktop_exec_arg(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars().filter(|c| !c.is_control()) {
        match c {
            '"' | '`' | '$' => { out.push_str("\\\\"); out.push(c); }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn linux(name: &str, path: &Path, icon_png: Option<&[u8]>) -> Result<(String, String, Vec<PathBuf>)> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let apps = dirs::data_dir()
        .context("no data dir")?
        .join("applications");
    std::fs::create_dir_all(&apps)?;
    let file = apps.join(format!("uapp-{}.desktop", slug(name).to_lowercase()));
    let mut created = vec![file.clone()];
    // Freedesktop allows an absolute path as the Icon= value; without a
    // custom icon the themed "uapp" name stays.
    let icon = match icon_png {
        Some(png) => {
            let p = icon_path(name, "png")?;
            std::fs::write(&p, png)?;
            created.push(p.clone());
            desktop_string(&p.display().to_string())
        }
        None => "uapp".into(),
    };
    std::fs::write(
        &file,
        format!(
            "[Desktop Entry]\nType=Application\nName={}\nComment=uapp application\n\
             Exec={} {}\nIcon={icon}\nCategories=Office;Utility;\nTerminal=false\n",
            desktop_string(name),
            desktop_exec_arg(&exe.display().to_string()),
            desktop_exec_arg(&path.display().to_string()),
        ),
    )?;
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .status();
    Ok((
        format!("Added “{name}” to your applications menu ({})", file.display()),
        format!("applications menu ({})", file.display()),
        created,
    ))
}

// ---- Windows: Start Menu .lnk via the WScript.Shell COM object -------------
#[cfg(windows)]
fn windows(name: &str, path: &Path, icon_png: Option<&[u8]>) -> Result<(String, String, Vec<PathBuf>)> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let dir = dirs::data_dir()
        .context("no data dir")?
        .join("Microsoft\\Windows\\Start Menu\\Programs\\uapp");
    std::fs::create_dir_all(&dir)?;
    let lnk = dir.join(format!("{}.lnk", sanitize_win(name)));
    let mut created = vec![lnk.clone()];
    // .lnk icons must be .ico; Vista+ accepts a PNG embedded verbatim, so a
    // minimal one-entry wrapper is enough — no re-encoding.
    let icon_loc = match icon_png {
        Some(png) => {
            let p = icon_path(name, "ico")?;
            std::fs::write(&p, png_to_ico(png))?;
            created.push(p.clone());
            p.display().to_string()
        }
        None => exe.display().to_string(),
    };
    // Best-effort desktop copy too: most users expect the icon on the desktop.
    // Every interpolated value goes through ps_quote — the name (and even the
    // path) come from a shared, untrusted .uapp file, and an unescaped quote
    // would otherwise run as PowerShell.
    let desktop = dirs::desktop_dir();
    if let Some(d) = &desktop {
        created.push(d.join(lnk.file_name().unwrap()));
    }
    let dirs_ps: Vec<String> = std::iter::once(dir.display().to_string())
        .chain(desktop.iter().map(|d| d.display().to_string()))
        .map(|d| ps_quote(&d))
        .collect();
    let ps = format!(
        "$w=New-Object -ComObject WScript.Shell; \
         foreach($d in @({})) {{ \
           $s=$w.CreateShortcut((Join-Path $d {})); \
           $s.TargetPath={}; $s.Arguments=('\"'+{}+'\"'); $s.IconLocation=({}+',0'); $s.Save() }}",
        dirs_ps.join(","),
        ps_quote(&lnk.file_name().unwrap().to_string_lossy()),
        ps_quote(&exe.display().to_string()),
        ps_quote(&path.display().to_string()),
        ps_quote(&icon_loc),
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .output()
        .context("run powershell")?;
    if !out.status.success() {
        bail!("powershell failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok((
        format!("Added “{name}” to the Start Menu (uapp folder){}",
            if desktop.is_some() { " and your desktop" } else { "" }),
        format!("Start Menu ({})", lnk.display()),
        created,
    ))
}

/// Minimal single-image ICO: 6-byte ICONDIR + one 16-byte ICONDIRENTRY + the
/// PNG verbatim (allowed since Vista). Width/height come from the PNG IHDR
/// (big-endian u32 at offsets 16/20); the entry stores 0 for >= 256.
#[cfg(windows)]
fn png_to_ico(png: &[u8]) -> Vec<u8> {
    let be = |off: usize| -> u32 {
        png.get(off..off + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0)
    };
    let dim = |v: u32| -> u8 { if v == 0 || v >= 256 { 0 } else { v as u8 } };
    let mut ico = Vec::with_capacity(22 + png.len());
    ico.extend_from_slice(&[0, 0, 1, 0, 1, 0]); // reserved, type=icon, count=1
    ico.push(dim(be(16))); // width
    ico.push(dim(be(20))); // height
    ico.extend_from_slice(&[0, 0]); // palette, reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // color planes
    ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes()); // image size
    ico.extend_from_slice(&22u32.to_le_bytes()); // image offset
    ico.extend_from_slice(png);
    ico
}

/// PowerShell single-quoted literal: the only escape is doubling the quote.
/// Control chars are dropped so a hostile name can't smuggle line breaks in.
#[cfg(windows)]
fn ps_quote(s: &str) -> String {
    let body: String = s.chars().filter(|c| !c.is_control())
        .flat_map(|c| if c == '\'' { vec!['\'', '\''] } else { vec![c] })
        .collect();
    format!("'{body}'")
}

#[cfg(windows)]
fn sanitize_win(name: &str) -> String {
    let s: String = name.chars().map(|c| match c {
        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
        c if c.is_control() => '-',
        c => c,
    }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "app".into() } else { s }
}

// ---- macOS: ~/Applications/<Name>.app wrapping `uapp <file>` ---------------
#[cfg(target_os = "macos")]
fn macos(name: &str, path: &Path, icon_png: Option<&[u8]>) -> Result<(String, String, Vec<PathBuf>)> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let home = dirs::home_dir().context("no home dir")?;
    let apps = home.join("Applications");
    let bundle = apps.join(format!("{}.app", sanitize_mac(name)));
    let macos_dir = bundle.join("Contents/MacOS");
    std::fs::create_dir_all(&macos_dir)?;
    let script = macos_dir.join("uapp-launch");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec {} {}\n",
            sh_quote(&exe.display().to_string()),
            sh_quote(&path.display().to_string())),
    )?;
    set_executable(&script)?;
    // Custom icon: PNG wrapped as .icns inside the bundle (removed with it).
    let icon_line = if let Some(png) = icon_png {
        let res = bundle.join("Contents/Resources");
        std::fs::create_dir_all(&res)?;
        std::fs::write(res.join("AppIcon.icns"), png_to_icns(png))?;
        "\t<key>CFBundleIconFile</key><string>AppIcon</string>\n"
    } else {
        ""
    };
    let xname = xml_escape(name);
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             \t<key>CFBundleName</key><string>{xname}</string>\n\
             \t<key>CFBundleDisplayName</key><string>{xname}</string>\n\
             \t<key>CFBundleExecutable</key><string>uapp-launch</string>\n\
             \t<key>CFBundleIdentifier</key><string>com.thederf.uapp.{}</string>\n\
             \t<key>CFBundlePackageType</key><string>APPL</string>\n\
             {icon_line}\
             </dict></plist>\n",
            slug(name).to_lowercase()
        ),
    )?;
    Ok((
        format!("Added “{name}” to ~/Applications ({})", bundle.display()),
        format!("~/Applications ({})", bundle.display()),
        vec![bundle],
    ))
}

#[cfg(target_os = "macos")]
fn sanitize_mac(name: &str) -> String {
    let s: String = name.chars().map(|c| match c {
        '/' | ':' => '-',
        c if c.is_control() => '-',
        c => c,
    }).collect();
    let s = s.trim().to_string();
    if s.is_empty() { "app".into() } else { s }
}

/// POSIX-shell single-quoted literal ('' has no escapes; a literal quote is
/// written '\''). The name/path come from an untrusted shared file.
#[cfg(target_os = "macos")]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).map(|c| match c {
        '&' => "&amp;".to_string(),
        '<' => "&lt;".to_string(),
        '>' => "&gt;".to_string(),
        c => c.to_string(),
    }).collect()
}

#[cfg(target_os = "macos")]
fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm)?;
    Ok(())
}
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn set_executable(_p: &Path) -> Result<()> { Ok(()) }

#[cfg(test)]
mod tests {
    use super::png_to_icns;

    /// A fake PNG header: signature + IHDR chunk with the given dimensions.
    fn fake_png(w: u32, h: u32) -> Vec<u8> {
        let mut p = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        p.extend_from_slice(&13u32.to_be_bytes());
        p.extend_from_slice(b"IHDR");
        p.extend_from_slice(&w.to_be_bytes());
        p.extend_from_slice(&h.to_be_bytes());
        p.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, color, ... (don't care)
        p
    }

    #[test]
    fn icns_container_shape() {
        let png = fake_png(256, 256);
        let icns = png_to_icns(&png);
        assert_eq!(&icns[0..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());
        assert_eq!(&icns[8..12], b"ic08"); // 256px type
        assert_eq!(u32::from_be_bytes(icns[12..16].try_into().unwrap()) as usize, 8 + png.len());
        assert_eq!(&icns[16..], &png[..]);
    }

    #[test]
    fn icns_type_by_size() {
        for (side, kind) in [(1024, b"ic10"), (512, b"ic09"), (128, b"ic07"), (64, b"ic12"), (32, b"ic11"), (16, b"ic11")] {
            let icns = png_to_icns(&fake_png(side, side));
            assert_eq!(&icns[8..12], kind, "side {side}");
        }
    }
}
