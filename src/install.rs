//! Desktop integration (Linux/XDG): MIME type for .uapp, a .desktop entry so
//! double-click opens the launcher, and a file in ~/Templates so the file
//! manager's right-click "New Document" menu can create a blank .uapp.
//! Everything points at the running `uapp` binary.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The Windows installer and the macOS app bundle register .uapp themselves.
fn ensure_xdg() -> Result<()> {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        bail!(
            "`uapp install` sets up Linux desktops; on this system the UApp installer \
             registers .uapp files"
        );
    }
    Ok(())
}

/// The per-user XDG data dir (`~/.local/share`) and home.
fn dirs_xdg() -> Result<(PathBuf, PathBuf)> {
    let home = dirs::home_dir().context("no home dir")?;
    let data = dirs::data_dir().unwrap_or_else(|| home.join(".local/share"));
    Ok((home, data))
}

/// Icon sizes written as PNG fallbacks beside the SVGs.
const ICON_SIZES: [&str; 5] = ["16x16", "32x32", "48x48", "128x128", "256x256"];

pub fn install() -> Result<()> {
    ensure_xdg()?;
    let exe = std::env::current_exe().context("resolve current executable")?;
    let (home, data) = dirs_xdg()?;

    // 1. Icons (embedded in the binary): launcher icon "uapp" + file-type
    //    icon "application-x-uapp", as scalable SVG plus PNG fallbacks.
    let icon_root = data.join("icons/hicolor");
    std::fs::create_dir_all(icon_root.join("scalable/apps"))?;
    std::fs::create_dir_all(icon_root.join("scalable/mimetypes"))?;
    std::fs::write(
        icon_root.join("scalable/apps/uapp.svg"),
        include_str!("../assets/icon.svg"),
    )?;
    std::fs::write(
        icon_root.join("scalable/mimetypes/application-x-uapp.svg"),
        include_str!("../assets/icon-file.svg"),
    )?;
    let pngs: [(&str, &[u8], &[u8]); 5] = [
        (
            "16x16",
            include_bytes!("../assets/uapp-16.png"),
            include_bytes!("../assets/uapp-file-16.png"),
        ),
        (
            "32x32",
            include_bytes!("../assets/uapp-32.png"),
            include_bytes!("../assets/uapp-file-32.png"),
        ),
        (
            "48x48",
            include_bytes!("../assets/uapp-48.png"),
            include_bytes!("../assets/uapp-file-48.png"),
        ),
        (
            "128x128",
            include_bytes!("../assets/uapp-128.png"),
            include_bytes!("../assets/uapp-file-128.png"),
        ),
        (
            "256x256",
            include_bytes!("../assets/uapp-256.png"),
            include_bytes!("../assets/uapp-file-256.png"),
        ),
    ];
    for (size, app_png, file_png) in pngs {
        let d = icon_root.join(size);
        std::fs::create_dir_all(d.join("apps"))?;
        std::fs::create_dir_all(d.join("mimetypes"))?;
        std::fs::write(d.join("apps/uapp.png"), app_png)?;
        std::fs::write(d.join("mimetypes/application-x-uapp.png"), file_png)?;
    }
    let _ = Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(&icon_root)
        .status();

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
    let _ = Command::new("update-mime-database")
        .arg(data.join("mime"))
        .status();

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
    //     native window with no file (the per-user default document). Only
    //     the window build has one to open.
    let launcher = cfg!(feature = "gui");
    if launcher {
        std::fs::write(
            apps_dir.join("UApp.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=UApp\nComment=Single-file shareable apps\n\
                 Exec={}\nTerminal=false\nIcon=uapp\nCategories=Office;Utility;\n",
                exe.display()
            ),
        )?;
    }
    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();
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
    println!(
        "  Icons: uapp + application-x-uapp ({})",
        icon_root.display()
    );
    println!("  MIME type application/x-uapp (*.uapp)");
    println!(
        "  Desktop entry: {} (double-click .uapp files now opens uapp)",
        apps_dir.join("uapp.desktop").display()
    );
    if launcher {
        println!(
            "  Launcher: {} (opens the UApp window)",
            apps_dir.join("UApp.desktop").display()
        );
    }
    println!(
        "  File-manager template: {} (right-click → New Document → New App)",
        tpl.display()
    );
    Ok(())
}

/// Undo [`install`]: remove the icons, MIME type, desktop entries and the
/// file-manager template (the template only while it's still blank).
pub fn uninstall() -> Result<()> {
    ensure_xdg()?;
    let (home, data) = dirs_xdg()?;
    let mut removed: Vec<PathBuf> = Vec::new();
    let mut remove = |p: PathBuf| {
        if std::fs::remove_file(&p).is_ok() {
            removed.push(p);
        }
    };
    let icon_root = data.join("icons/hicolor");
    remove(icon_root.join("scalable/apps/uapp.svg"));
    remove(icon_root.join("scalable/mimetypes/application-x-uapp.svg"));
    for size in ICON_SIZES {
        remove(icon_root.join(size).join("apps/uapp.png"));
        remove(
            icon_root
                .join(size)
                .join("mimetypes/application-x-uapp.png"),
        );
    }
    remove(data.join("mime/packages/uapp.xml"));
    let apps_dir = data.join("applications");
    remove(apps_dir.join("uapp.desktop"));
    remove(apps_dir.join("UApp.desktop"));
    let tpl = home.join("Templates").join("New App.uapp");
    let kept_template = tpl.exists() && !is_blank(&tpl);
    if !kept_template {
        remove(tpl.clone());
    }

    let _ = Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(&icon_root)
        .status();
    let _ = Command::new("update-mime-database")
        .arg(data.join("mime"))
        .status();
    let _ = Command::new("update-desktop-database")
        .arg(&apps_dir)
        .status();

    if removed.is_empty() {
        println!("Nothing to remove (uapp install hasn't been run for this user).");
    } else {
        println!("Removed:");
        for p in &removed {
            println!("  {}", p.display());
        }
    }
    if kept_template {
        println!("Kept {} (it has been edited)", tpl.display());
    }
    Ok(())
}

/// Is the file at `path` still the blank app `install` put there?
fn is_blank(path: &Path) -> bool {
    use rusqlite::{Connection, OpenFlags};
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(|c| crate::store::is_blank_app(&c))
        .unwrap_or(false)
}
