//! Desktop integration (Linux/XDG): MIME type for .uapp, a .desktop entry so
//! double-click opens the launcher, and a file in ~/Templates so the file
//! manager's right-click "New Document" menu can create a blank .uapp.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub fn install() -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let home = dirs::home_dir().context("no home dir")?;
    let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));

    // 1. Icons (embedded in the binary): launcher icon "uapp" + file-type
    //    icon "application-x-uapp", as scalable SVG plus PNG fallbacks.
    let icon_root = data.join("icons/hicolor");
    std::fs::create_dir_all(icon_root.join("scalable/apps"))?;
    std::fs::create_dir_all(icon_root.join("scalable/mimetypes"))?;
    std::fs::write(icon_root.join("scalable/apps/uapp.svg"), include_str!("../assets/icon.svg"))?;
    std::fs::write(
        icon_root.join("scalable/mimetypes/application-x-uapp.svg"),
        include_str!("../assets/icon-file.svg"),
    )?;
    let pngs: [(&str, &[u8], &[u8]); 5] = [
        ("16x16", include_bytes!("../assets/uapp-16.png"), include_bytes!("../assets/uapp-file-16.png")),
        ("32x32", include_bytes!("../assets/uapp-32.png"), include_bytes!("../assets/uapp-file-32.png")),
        ("48x48", include_bytes!("../assets/uapp-48.png"), include_bytes!("../assets/uapp-file-48.png")),
        ("128x128", include_bytes!("../assets/uapp-128.png"), include_bytes!("../assets/uapp-file-128.png")),
        ("256x256", include_bytes!("../assets/uapp-256.png"), include_bytes!("../assets/uapp-file-256.png")),
    ];
    for (size, app_png, file_png) in pngs {
        let d = icon_root.join(size);
        std::fs::create_dir_all(d.join("apps"))?;
        std::fs::create_dir_all(d.join("mimetypes"))?;
        std::fs::write(d.join("apps/uapp.png"), app_png)?;
        std::fs::write(d.join("mimetypes/application-x-uapp.png"), file_png)?;
    }
    let _ = Command::new("gtk-update-icon-cache").args(["-f", "-t"]).arg(&icon_root).status();

    // 2. MIME type
    let mime_dir = data.join("mime/packages");
    std::fs::create_dir_all(&mime_dir)?;
    std::fs::write(
        mime_dir.join("uapp.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-uapp">
    <comment>uapp shareable application</comment>
    <glob pattern="*.uapp"/>
    <icon name="application-x-uapp"/>
  </mime-type>
</mime-info>
"#,
    )?;
    let _ = Command::new("update-mime-database").arg(data.join("mime")).status();

    // 3. Desktop entry
    let apps_dir = data.join("applications");
    std::fs::create_dir_all(&apps_dir)?;
    std::fs::write(
        apps_dir.join("uapp.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=uapp\nComment=Open shareable .uapp applications\n\
             Exec={} open %f\nTerminal=false\nMimeType=application/x-uapp;\n\
             Icon=uapp\nCategories=Office;Utility;\n",
            exe.display()
        ),
    )?;
    // 3b. The bare "UApp" launcher: an applications-menu entry that opens the
    //     native desktop app with no file (the per-user default document).
    //     Installed when a uapp binary can be found — same lookup
    //     order as the CLI's --window delegation (main.rs).
    let desktop_bin = find_desktop_bin(&exe);
    if let Some(bin) = &desktop_bin {
        std::fs::write(
            apps_dir.join("UApp.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=UApp\nComment=Single-file shareable apps\n\
                 Exec={}\nTerminal=false\nIcon=uapp\nCategories=Office;Utility;\n",
                bin.display()
            ),
        )?;
    }
    let _ = Command::new("update-desktop-database").arg(&apps_dir).status();
    let _ = Command::new("xdg-mime")
        .args(["default", "uapp.desktop", "application/x-uapp"])
        .status();

    // 4. Template for the file manager's "New Document" menu.
    //    A real bootstrapped file, so it opens instantly; a 0-byte copy the
    //    file manager might make also works because open() bootstraps empties.
    let templates = home.join("Templates");
    std::fs::create_dir_all(&templates)?;
    let tpl: PathBuf = templates.join("New App.uapp");
    if tpl.exists() {
        std::fs::remove_file(&tpl)?;
    }
    crate::store::bootstrap(&tpl, "New App", None)?;

    println!("Installed:");
    println!("  Icons: uapp + application-x-uapp ({})", icon_root.display());
    println!("  MIME type application/x-uapp (*.uapp)");
    println!("  Desktop entry: {} (double-click .uapp files now opens uapp)", apps_dir.join("uapp.desktop").display());
    match &desktop_bin {
        Some(bin) => println!("  Launcher: {} (bare UApp window → {})", apps_dir.join("UApp.desktop").display(), bin.display()),
        None => println!("  (no uapp binary found — bare UApp launcher not installed)"),
    }
    println!("  File-manager template: {} (right-click → New Document → New App)", tpl.display());
    Ok(())
}

/// Locate the native UApp (Tauri) binary: $UAPP_DESKTOP_BIN, then next to
/// this executable, then the PATH — the same order the CLI's --window
/// delegation uses (main.rs).
fn find_desktop_bin(exe: &std::path::Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UAPP_DESKTOP_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(dir) = exe.parent() {
        for n in ["uapp", "uapp.exe", "UApp", "UApp.exe"] {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for n in ["uapp", "UApp"] {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}
