// Server binary (uapp-server) — for future work, not distributed.
// Built as `uapp-server` via `--bin uapp-server --no-default-features`.
// The primary distributed binary is the native Tauri desktop app (uapp).
use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use uapp::{device_id, install, serve_opts, store, OnReady};

/// How to present the UI for a launch. `--window`/`--tauri`/`--native` force
/// the native UApp desktop window (the primary path, and the default);
/// `--browser`/`--web` force the system browser (the fallback shell).
#[derive(Clone, Copy, PartialEq)]
enum ShellMode {
    Browser,
    Native,
}

fn usage() -> ! {
    eprintln!(
        "uapp {}\n\nUsage:\n  uapp <file.uapp>            open an app (creates it if empty/missing)\n  uapp open <file.uapp>       same, explicit\n  uapp new <file.uapp>        create a blank app without opening it\n  uapp encrypt <file.uapp>    encrypt a (plaintext) app with a master password\n  uapp decrypt <file.uapp>    remove encryption (needs the master password)\n  uapp passwd <file.uapp>     change the master password\n  uapp install                register .uapp with your desktop + file manager\n  uapp serve <file.uapp>      serve it as a PUBLIC website\n  uapp serve --chrome <dir>   serve the browser build with NO file: the page\n                              opens ?open=<url> / ?app=<id>, or --open\n  uapp update <app> <tpl>     update <app>'s code from a template .uapp,\n                              keeping its data\n\nServe options (public website mode):\n  --bind <addr>               interface to bind (default 127.0.0.1; use\n                              0.0.0.0 to accept outside connections)\n  --port <n>                  port (default 8080 in serve mode)\n  --coi                       send COOP/COEP so pages are cross-origin\n                              isolated. Needed for the editing chrome's\n                              run_js bridge; breaks cross-origin iframe\n                              embeds (YouTube etc.)\n  --no-archive                don't expose /site.uapp\n  --asset-max-age <secs>      let browsers cache non-HTML site files that long\n                              (default 0 = always revalidate; raise it only if\n                              your build fingerprints asset names)\n  --publish-data              include data/ (content sources) in /site.uapp\n                              so visitors can re-run the site's build\n                              pipeline. Everything in the archive is PUBLIC\n  --chrome <dist-web>         serve the browser build from this directory so\n                              pages get an \"Edit this site\" affordance that\n                              boots the wasm engine on a local copy\n  --open <url>                (no file) what a bare visit opens, e.g. the\n                              launcher .uapp on a uapp-library\n  --no-coi                    (no file) don't send COOP/COEP; on by default\n                              there, since only the shell is served\n  --token <t>                 the secret that unlocks the shell, the WebSocket\n                              and publishing (PUT /site.uapp) on the public\n                              origin; else UAPP_TOKEN, else a random one is\n                              printed at start\n\nOptions:\n  --headless                  don't open a browser; print the URL as JSON\n  --port <n>                  bind a fixed port (default: random)\n  --window                    force the native UApp window (the primary path\n                              and the default when the UApp app is installed)\n  --browser                   force the system browser (the fallback shell)\n  --password <pw>             master password for an encrypted app (else\n                              UAPP_PASSWORD, else a hidden prompt). Opening a\n                              plaintext app with a password encrypts it.\n  --encrypt                   with `new`: create the app encrypted\n  --dry-run                   with `update`: print what would change, as JSON\n  --keep-stale                with `update`: keep app files the template\n                              no longer has (they are removed by default)\n  --template-password <pw>    master password of an ENCRYPTED source .uapp\n",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}

/// Resolve a master password: --password flag, else UAPP_PASSWORD, else a
/// hidden prompt (only possible on a tty; headless callers must use one of
/// the other two). `confirm` asks twice (for setting a new password).
fn get_password(flag: Option<String>, confirm: bool) -> Result<String> {
    if let Some(p) = flag {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("UAPP_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("no terminal available for a password prompt — pass --password or set UAPP_PASSWORD");
    }
    let p = rpassword::prompt_password("master password: ")?;
    if p.is_empty() {
        anyhow::bail!("empty password");
    }
    if confirm {
        let p2 = rpassword::prompt_password("master password (again): ")?;
        if p != p2 {
            anyhow::bail!("passwords do not match");
        }
    }
    Ok(p)
}

/// Standard base64 (the op payload carries the template's bytes).
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("uapp: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut headless = false;
    let mut port: u16 = 0;
    let mut shell: Option<ShellMode> = None;
    let mut password_flag: Option<String> = None;
    let mut encrypt_new = false;
    let mut dry_run = false;
    let mut keep_stale = false;
    let mut template_password: Option<String> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut coi = false;
    let mut archive = true;
    let mut publish_data = false;
    let mut chrome_dir: Option<PathBuf> = None;
    let mut open_url: Option<String> = None;
    let mut no_coi = false;
    let mut token: Option<String> = std::env::var("UAPP_TOKEN").ok().filter(|t| !t.is_empty());
    let mut asset_max_age: u32 = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => headless = true,
            "--browser" | "--web" => shell = Some(ShellMode::Browser),
            "--window" | "--tauri" | "--native" => shell = Some(ShellMode::Native),
            "--port" => {
                port = it.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| {
                    eprintln!("uapp: --port needs a number");
                    usage()
                });
            }
            "--asset-max-age" => {
                asset_max_age = it.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| {
                    eprintln!("uapp: --asset-max-age needs a number of seconds");
                    usage()
                });
            }
            "--password" => {
                password_flag = Some(it.next().cloned().unwrap_or_else(|| usage()));
            }
            "--encrypt" => encrypt_new = true,
            "--bind" => {
                bind = it.next().cloned().unwrap_or_else(|| usage());
            }
            "--coi" => coi = true,
            "--no-coi" => no_coi = true,
            "--open" => {
                open_url = Some(it.next().cloned().unwrap_or_else(|| usage()));
            }
            "--no-archive" => archive = false,
            "--publish-data" => publish_data = true,
            "--chrome" => {
                chrome_dir = Some(PathBuf::from(it.next().cloned().unwrap_or_else(|| usage())));
            }
            "--token" => {
                token = Some(it.next().cloned().unwrap_or_else(|| usage()));
            }
            "--dry-run" => dry_run = true,
            "--keep-stale" => keep_stale = true,
            "--template-password" => {
                template_password = Some(it.next().cloned().unwrap_or_else(|| usage()));
            }
            "-h" | "--help" => usage(),
            "-V" | "--version" => {
                println!("uapp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            // A typo'd flag must not silently become a positional argument.
            other if other.starts_with('-') && other != "-" => {
                eprintln!("uapp: unknown option {other}");
                usage()
            }
            _ => positional.push(a.clone()),
        }
    }
    let open_path = |positional: &[String], idx: usize| -> PathBuf {
        positional.get(idx).map(PathBuf::from).unwrap_or_else(|| usage())
    };
    match positional.first().map(|s| s.as_str()) {
        // No file: host the browser build alone (webhost.rs). What opens is
        // in the URL, so this process needs no archive and no token.
        Some("serve") if positional.len() == 1 => {
            let Some(dir) = chrome_dir else {
                anyhow::bail!("serve without a file needs --chrome <dist-web> (the browser build to host)");
            };
            uapp::serve_web(&bind, if port == 0 { 8080 } else { port }, &dir, !no_coi, open_url)
        }
        Some("serve") => {
            let f = open_path(&positional, 1);
            // Only ask for a password when the file actually needs one; a
            // plaintext site should serve with no prompt at all.
            let pw = if store::is_encrypted_file(&f) {
                Some(get_password(password_flag, false)?)
            } else {
                None
            };
            uapp::serve_public(
                f,
                &bind,
                if port == 0 { 8080 } else { port },
                pw,
                token,
                uapp::app::PublicOpts {
                    coi,
                    archive,
                    export_data: publish_data,
                    asset_max_age,
                },
                chrome_dir,
            )
        }
        Some("install") => install::install(),
        Some("new") => {
            let f = open_path(&positional, 1);
            let name = f
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "app".into());
            if encrypt_new || password_flag.is_some() {
                let pw = get_password(password_flag, true)?;
                store::bootstrap(&f, &name, Some(&pw))?;
                println!("created {} (encrypted)", f.display());
            } else {
                store::bootstrap(&f, &name, None)?;
                println!("created {}", f.display());
            }
            Ok(())
        }
        Some("encrypt") => {
            let f = open_path(&positional, 1);
            let len = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
            if len == 0 {
                anyhow::bail!("{} is empty — use `uapp new --encrypt` to create it", f.display());
            }
            if !store::looks_like_sqlite(&f) {
                // Already encrypted (verify with the password) or foreign.
                let pw = get_password(password_flag, false)?;
                uapp::cipher::open_file(&f, &pw)?;
                anyhow::bail!("{} is already encrypted (use `uapp passwd` to change it)", f.display());
            }
            let pw = get_password(password_flag, true)?;
            uapp::cipher::encrypt_file_in_place(&f, &pw)?;
            println!("{} is now encrypted (AES-256, SQLCipher)", f.display());
            Ok(())
        }
        Some("decrypt") => {
            let f = open_path(&positional, 1);
            if store::looks_like_sqlite(&f) {
                anyhow::bail!("{} is not encrypted", f.display());
            }
            let pw = get_password(password_flag, false)?;
            // Verify the password up front (clear error), then transform.
            uapp::cipher::open_file(&f, &pw)?;
            uapp::cipher::decrypt_file_in_place(&f, &pw)?;
            println!("{} is now plaintext", f.display());
            Ok(())
        }
        Some("passwd") => {
            let f = open_path(&positional, 1);
            if store::looks_like_sqlite(&f) {
                anyhow::bail!("{} is not encrypted (nothing to change)", f.display());
            }
            let old = get_password(password_flag, false)?;
            // Verify before prompting for the new one.
            uapp::cipher::open_file(&f, &old)?;
            use std::io::IsTerminal;
            let new = if std::io::stdin().is_terminal() {
                let new = rpassword::prompt_password("new master password: ")?;
                if new.is_empty() {
                    anyhow::bail!("empty password");
                }
                let again = rpassword::prompt_password("new master password (again): ")?;
                if new != again {
                    anyhow::bail!("passwords do not match");
                }
                new
            } else {
                // Headless: the new password comes from the environment.
                std::env::var("UAPP_PASSWORD")
                    .ok()
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("no terminal available — set the new password via UAPP_PASSWORD and the old one via --password")
                    })?
            };
            uapp::cipher::rekey_file(&f, &old, &new)?;
            println!("master password changed");
            Ok(())
        }
        // Drop a newer version of an app onto an existing one: the app's code
        // (files under app/) and its schema are taken from the template; the
        // rows, uploads, chat and app identity in <app> stay exactly as they
        // are. This is the same operation the shell performs when a .uapp is
        // dragged onto the window.
        Some("update") => {
            let target = open_path(&positional, 1);
            let template = open_path(&positional, 2);
            let bytes = std::fs::read(&template)
                .with_context(|| format!("reading {}", template.display()))?;
            let pw = resolve_open_password(&target, password_flag, true)?;
            let device = device_id().unwrap_or_else(|_| "cli".into());
            let user = {
                let real = whoami::realname();
                if real.trim().is_empty() { whoami::username() } else { real }
            };
            // A normal open: it takes the registry lock (so this refuses to run
            // while the app is open elsewhere) and snapshots first, which is
            // the undo path if an update turns out wrong.
            let mut eng = uapp::engine::Engine::open_with_passphrase(
                target.clone(), device, user, pw,
            )
            .with_context(|| {
                format!(
                    "opening {} (if it's open in a window, close it first — or just \
                     drag the template onto that window)",
                    target.display()
                )
            })?;
            if dry_run {
                let src = uapp::template::Source::from_bytes(
                    &bytes, template_password.as_deref())?;
                let plan = uapp::template::plan(&eng.db, src.conn())?;
                println!("{}", serde_json::to_string_pretty(&plan)?);
                return Ok(());
            }
            let mut payload = json!({
                "b64": base64_encode(&bytes),
                "remove_stale": !keep_stale,
            });
            if let Some(p) = &template_password {
                payload["password"] = json!(p);
            }
            let (result, _) = eng.local_op("template_update", payload)?;
            eng.snapshot_if_dirty();
            let a = &result["applied"];
            println!(
                "{}: {} file(s) written, {} removed, {} table(s) created, {} column(s) added, \
                 {} view/index/trigger(s) updated",
                target.display(),
                a["filesWritten"], a["filesRemoved"], a["tablesCreated"],
                a["columnsAdded"], a["objectsUpdated"],
            );
            for w in result["warnings"].as_array().cloned().unwrap_or_default() {
                eprintln!("uapp: note: {}", w.as_str().unwrap_or_default());
            }
            Ok(())
        }
        Some("open") => {
            let f = open_path(&positional, 1);
            let pw = resolve_open_password(&f, password_flag, headless)?;
            launch(f, headless, port, false, shell, pw)
        }
        Some(other) if !other.starts_with('-') => {
            let f = PathBuf::from(other);
            let pw = resolve_open_password(&f, password_flag, headless)?;
            launch(f, headless, port, false, shell, pw)
        }
        // No file given: open a blank scratch app in a temp file, flagged
        // "unsaved" so the shell prompts to download it before it's lost.
        None => {
            // A unique temp subdir so the file can be named "Untitled" (its
            // stem becomes the app name and the download filename).
            let dir = std::env::temp_dir().join(format!(
                "uapp-scratch-{}",
                (0..10).map(|_| fastrand::alphanumeric()).collect::<String>()
            ));
            std::fs::create_dir_all(&dir)?;
            launch(dir.join("Untitled.uapp"), headless, port, true, shell, None)
        }
        _ => usage(),
    }
}

/// For `open`: if the file is encrypted, resolve the master password.
/// Interactive launches (browser / native window) return `None` and let the
/// server show its web unlock prompt. Headless has no UI to prompt in, so it
/// requires the password up front (`--password` / `UAPP_PASSWORD`) and verifies
/// it, failing clearly on a wrong/missing one. A plaintext file just opens.
fn resolve_open_password(
    path: &Path,
    flag: Option<String>,
    headless: bool,
) -> Result<Option<String>> {
    if !store::is_encrypted_file(path) {
        return Ok(None);
    }
    let provided = flag.or_else(|| std::env::var("UAPP_PASSWORD").ok().filter(|p| !p.is_empty()));
    match provided {
        Some(pw) => {
            // Verify now so a wrong password fails clearly here rather than
            // surfacing deep in the open path (headless especially).
            if headless {
                uapp::cipher::open_file(path, &pw)
                    .with_context(|| format!("opening {}", path.display()))?;
            }
            Ok(Some(pw))
        }
        None if headless => anyhow::bail!(
            "{} is encrypted — pass --password or set UAPP_PASSWORD \
             (headless mode can't show the unlock prompt)",
            path.display()
        ),
        None => Ok(None), // interactive: the server serves the web unlock page
    }
}

/// Decide how to present the UI, then serve. The native UApp (Tauri) window
/// is the primary path and the default delegation target; the browser is only
/// the fallback (explicit `--browser`, or no UApp desktop app installed).
fn launch(
    path: PathBuf,
    headless: bool,
    port: u16,
    unsaved: bool,
    shell: Option<ShellMode>,
    passphrase: Option<String>,
) -> Result<()> {
    // With no explicit --browser/--window, fall back to this machine's saved
    // preference (Settings → "Open apps in", default: native window) for
    // file-association and terminal opens alike. An explicit flag still always
    // wins, and when no UApp desktop app can be found the browser is the
    // fallback (with a note). A bare `uapp` scratch also opens natively; the
    // `unsaved` flag rides along so the native window shows the "Save…" banner.
    let shell = shell.or_else(|| {
        if uapp::prefs::shell() == "native" {
            Some(ShellMode::Native)
        } else {
            None
        }
    });
    // Headless is a server-only mode (no UI launch), so the shell choice is moot.
    if !headless && shell == Some(ShellMode::Native) {
        match delegate_to_desktop(&path, unsaved) {
            Ok(true) => return Ok(()),
            Ok(false) => eprintln!(
                "uapp: no UApp desktop app found (install it, or set UAPP_DESKTOP_BIN) — opening in your browser instead"
            ),
            Err(e) => eprintln!("uapp: couldn't launch the UApp desktop app ({e}) — opening in your browser instead"),
        }
    }
    run_serve(path, headless, port, unsaved, passphrase)
}

/// Try to hand a .uapp to the native UApp (Tauri) desktop app. Looks at
/// $UAPP_DESKTOP_BIN, then next to this executable, then the PATH. `unsaved`
/// forwards the scratch flag so the native window shows the "Save…" banner.
/// Returns Ok(true) if one was launched.
fn delegate_to_desktop(path: &Path, unsaved: bool) -> Result<bool> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("UAPP_DESKTOP_BIN") {
        if !p.is_empty() {
            candidates.push(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for n in ["uapp", "uapp.exe", "UApp", "UApp.exe"] {
                candidates.push(dir.join(n));
            }
        }
    }
    // Bare names fall through to a PATH lookup.
    for n in ["uapp", "UApp"] {
        candidates.push(PathBuf::from(n));
    }
    for c in candidates {
        let mut cmd = std::process::Command::new(&c);
        cmd.arg(path);
        if unsaved {
            cmd.arg("--unsaved");
        }
        if cmd.spawn().is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Build the right "ready" behaviour for the CLI and hand off to the library.
fn run_serve(
    path: PathBuf,
    headless: bool,
    port: u16,
    unsaved: bool,
    passphrase: Option<String>,
) -> Result<()> {
    let on_ready: OnReady = if headless {
        let device = device_id().unwrap_or_default();
        Box::new(move |url, port, reused| {
            println!(
                "{}",
                json!({"url": url, "port": port, "device": device, "reused": reused})
            );
        })
    } else {
        Box::new(|url, _port, _reused| {
            eprintln!("uapp: serving at {url}");
            // Detached so xdg-open/Explorer don't block for the browser's life.
            let _ = open::that_detached(url);
        })
    };
    serve_opts(path, port, unsaved, passphrase, on_ready)
}
