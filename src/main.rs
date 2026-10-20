//! The `uapp` binary: the native window (default `gui` feature) and the CLI in
//! one. `uapp` and `uapp FILE.uapp` open the window in-process; a build made
//! with `--no-default-features` (servers, Docker, tests) opens the system
//! browser instead. Every subcommand works the same in both.

// Windowless on Windows release builds, so no console flashes up behind the
// native window. A subcommand run from a terminal attaches to that terminal's
// console instead (see `attach_console`).
#![cfg_attr(
    all(windows, feature = "gui", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use uapp::{device_id, install, serve_opts, store, OnReady};

#[derive(Parser)]
#[command(
    name = "uapp",
    version,
    about = "Single-file shareable apps: app + data + AI in one .uapp file",
    after_help = "With no command, `uapp` opens your scratch app. `uapp FILE.uapp` is short \
                  for `uapp open FILE.uapp`."
)]
struct Cli {
    /// Log more (-v debug, -vv trace). UAPP_LOG overrides both flags
    #[arg(short, long, action = clap::ArgAction::Count, global = true, help_heading = "Global options")]
    verbose: u8,
    /// Log less (-q warnings only, -qq errors only)
    #[arg(short, long, action = clap::ArgAction::Count, global = true, conflicts_with = "verbose",
        help_heading = "Global options")]
    quiet: u8,
    #[command(flatten, next_help_heading = "Options for opening the scratch app")]
    open: OpenOpts,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open an app (creates it if the file is empty or missing)
    Open {
        /// The .uapp file (none: your scratch app)
        file: Option<PathBuf>,
        #[command(flatten)]
        opts: OpenOpts,
    },
    /// Create a blank app without opening it
    New {
        /// Where to create it
        file: PathBuf,
        /// Create it encrypted (asks for a master password)
        #[arg(long)]
        encrypt: bool,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Update an app's code from a newer copy of it, keeping its data
    #[command(
        long_about = "Update an app's code from a newer copy of it, keeping its data.\n\n\
                      The app's files under app/ and its schema come from TEMPLATE; rows, \
                      uploads, chat and the app's identity stay as they are. A snapshot is \
                      taken first, so the update can be undone. This is what dropping a \
                      .uapp onto an open window does."
    )]
    Update {
        /// The app to update
        app: PathBuf,
        /// The .uapp to take the code from
        template: PathBuf,
        /// Print what would change, as JSON, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Keep app files the template no longer has (removed by default)
        #[arg(long)]
        keep_stale: bool,
        /// Master password of an encrypted TEMPLATE (visible to other users in `ps`)
        #[arg(long, value_name = "PW", conflicts_with = "template_password_file")]
        template_password: Option<String>,
        /// Read the TEMPLATE's master password from the first line of a file
        #[arg(long, value_name = "PATH")]
        template_password_file: Option<PathBuf>,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Write a copy of an app to a new file
    Export {
        /// The app to copy
        app: PathBuf,
        /// Where to write the copy
        #[arg(short, long, value_name = "OUT")]
        output: PathBuf,
        /// Export a template instead: the app's code and empty tables, without
        /// rows, data/, chat, history or saved keys
        #[arg(long)]
        template: bool,
        /// Replace OUT if it exists
        #[arg(short, long)]
        force: bool,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Show what's in an app file (name, id, format, tables, files)
    Info {
        /// The .uapp file
        app: PathBuf,
        /// Print JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Encrypt an app with a master password
    Encrypt {
        /// The .uapp file
        file: PathBuf,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Remove an app's encryption
    Decrypt {
        /// The .uapp file
        file: PathBuf,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Change an app's master password
    #[command(long_about = "Change an app's master password.\n\n\
                      The new password comes from UAPP_NEW_PASSWORD, else the second line of \
                      stdin (with --password-stdin), else a prompt.")]
    Passwd {
        /// The .uapp file
        file: PathBuf,
        #[command(flatten)]
        password: PasswordArgs,
    },
    /// Serve an app as a public website, or the browser build on its own
    #[command(
        long_about = "Serve an app as a public website, or the browser build on its own.\n\n\
                      With FILE, anyone can read the site's pages; the token unlocks the \
                      editing shell, the WebSocket and publishing (PUT /site.uapp).\n\n\
                      Without FILE, --chrome is required: the browser build is served alone \
                      and opens whatever its URL names (?open=<url>, ?app=<id>, or --open)."
    )]
    Serve(ServeArgs),
    /// Serve a folder of .uapp files over HTTP (list, fetch, publish)
    #[command(long_about = "Serve a folder of .uapp files over HTTP.\n\n\
                      GET / lists them as JSON and GET /<name>.uapp fetches one (ETag, \
                      If-None-Match). PUT /<name>.uapp publishes one (needs the token; \
                      If-Match gives 409 on a conflict) and is stored without chat, history \
                      or config. DELETE /<name>.uapp removes one (needs the token).")]
    Library(LibraryArgs),
    /// Register .uapp files with your desktop and file manager (Linux)
    Install,
    /// Remove what `uapp install` set up
    Uninstall,
    /// Print a shell completion script
    #[command(
        after_help = "Example (bash): uapp completions bash > ~/.local/share/bash-completion/completions/uapp"
    )]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// How to open an app.
#[derive(Args, Clone, Default)]
struct OpenOpts {
    /// Don't show any UI; print the URL as a JSON line and keep serving
    #[arg(long)]
    headless: bool,
    /// Open in the system browser instead of the UApp window
    #[arg(long, alias = "web", conflicts_with = "window")]
    browser: bool,
    /// Open in the UApp window even if Settings says browser
    #[arg(long, aliases = ["tauri", "native"], hide = true)]
    window: bool,
    /// Serve on this port (default: any free one)
    #[arg(long, value_name = "N", default_value_t = 0, hide_default_value = true)]
    port: u16,
    #[command(flatten)]
    password: PasswordArgs,
}

/// Where a master password comes from. Without any of these: UAPP_PASSWORD,
/// else a hidden prompt on a terminal.
#[derive(Args, Clone, Default)]
struct PasswordArgs {
    /// Master password (visible to other users in `ps`; prefer --password-stdin, --password-file or UAPP_PASSWORD)
    #[arg(long, value_name = "PW", conflicts_with_all = ["password_stdin", "password_file"])]
    password: Option<String>,
    /// Read the master password from the first line of stdin
    #[arg(long, conflicts_with = "password_file")]
    password_stdin: bool,
    /// Read the master password from the first line of a file
    #[arg(long, value_name = "PATH")]
    password_file: Option<PathBuf>,
}

#[derive(Args)]
struct ServeArgs {
    /// The .uapp to serve as a website (none: serve only the browser build)
    file: Option<PathBuf>,
    /// Interface to bind (0.0.0.0 accepts outside connections)
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1")]
    bind: String,
    /// Port to listen on
    #[arg(long, value_name = "N", default_value_t = 8080)]
    port: u16,
    /// Secret that unlocks the shell, the WebSocket and publishing (default:
    /// a random one, printed at start)
    #[arg(long, value_name = "T", env = "UAPP_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Send COOP/COEP so pages are cross-origin isolated (needed by the editing
    /// chrome's run_js bridge; breaks cross-origin iframe embeds such as YouTube)
    #[arg(long)]
    coi: bool,
    /// Without FILE: don't send COOP/COEP (sent by default there, since only
    /// the shell is served)
    #[arg(long)]
    no_coi: bool,
    /// Don't expose /site.uapp
    #[arg(long)]
    no_archive: bool,
    /// Include data/ (content sources) in /site.uapp so visitors can re-run
    /// the site's build. Everything in the archive is public
    #[arg(long)]
    publish_data: bool,
    /// Let browsers cache non-HTML site files this long (only if your build
    /// fingerprints asset names)
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    asset_max_age: u32,
    /// Serve the browser build from this directory (dist-web), which gives
    /// pages an "Edit this site" button that edits a local copy
    #[arg(long, value_name = "DIR")]
    chrome: Option<PathBuf>,
    /// Without FILE: what a bare visit opens, e.g. /uapp/apps/launcher.uapp on
    /// a library
    #[arg(long, value_name = "URL")]
    open: Option<String>,
    #[command(flatten)]
    password: PasswordArgs,
}

#[derive(Args)]
struct LibraryArgs {
    /// Folder of .uapp files (created if missing)
    dir: PathBuf,
    /// Interface to bind (0.0.0.0 accepts outside connections)
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1")]
    bind: String,
    /// Port to listen on
    #[arg(long, value_name = "N", default_value_t = 8090)]
    port: u16,
    /// Secret that unlocks PUT and DELETE (default: a random one, printed at start)
    #[arg(long, value_name = "T", env = "UAPP_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Largest upload accepted, in MB
    #[arg(long, value_name = "N", default_value_t = 256)]
    max_mb: usize,
}

fn main() {
    attach_console();
    let cli = Cli::parse_from(rewrite_args(std::env::args_os().collect()));
    let verbosity = cli.verbose as i8 - cli.quiet as i8;
    let window = opens_window(&cli);
    uapp::logging::init(verbosity, window);
    if let Err(e) = run(cli, window) {
        tracing::debug!("{e:?}");
        eprintln!("uapp: {e:#}");
        std::process::exit(1);
    }
}

/// `uapp FILE` means `uapp open FILE`: put the `open` back in when the first
/// word looks like a file (ends in .uapp, or exists). Any other word is left
/// for clap, so a typo'd command gets "unrecognized subcommand" and a
/// suggestion rather than a new file named after the typo.
fn rewrite_args(mut args: Vec<OsString>) -> Vec<OsString> {
    // Top-level options (OpenOpts, PasswordArgs) that take a separate value.
    const TAKES_VALUE: [&str; 3] = ["--port", "--password", "--password-file"];
    let mut i = 1;
    while i < args.len() {
        let a = args[i].to_string_lossy().into_owned();
        if a == "--" {
            if i + 1 < args.len() {
                args.insert(i, "open".into());
            }
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            i += if TAKES_VALUE.contains(&a.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        let cmd = Cli::command();
        let known = a == "help"
            || cmd
                .get_subcommands()
                .any(|s| s.get_name() == a || s.get_all_aliases().any(|al| al == a));
        if !known && (a.to_lowercase().ends_with(".uapp") || Path::new(&a).exists()) {
            args.insert(i, "open".into());
        }
        break;
    }
    args
}

/// Does this invocation open the native window? (Then logs also go to a file,
/// since a Windows window build has no console.)
fn opens_window(cli: &Cli) -> bool {
    match &cli.cmd {
        None => wants_window(&cli.open, false),
        Some(Cmd::Open { file, opts }) => wants_window(opts, file.is_some()),
        _ => false,
    }
}

/// Window or browser. A launch with no file always gets the window; a file
/// follows the "Open apps in" setting unless a flag says otherwise. Builds
/// without the gui feature have no window.
fn wants_window(o: &OpenOpts, has_file: bool) -> bool {
    if !cfg!(feature = "gui") || o.headless || o.browser {
        return false;
    }
    o.window || !has_file || uapp::prefs::shell() == "native"
}

/// A GUI-subsystem exe starts without a console; attach to the terminal it
/// was run from (if any) so CLI output shows up there. Does nothing when
/// there is no parent console (a double-click) or the handles are redirected.
#[cfg(all(windows, feature = "gui"))]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: plain Win32 call with no pointers; failure just means no console.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(windows, feature = "gui")))]
fn attach_console() {}

fn run(cli: Cli, window: bool) -> Result<()> {
    let Some(cmd) = cli.cmd else {
        return open(None, cli.open, window);
    };
    // The top-level open options belong to a bare `uapp`; anything else would
    // be silently ignored. (clap's args_conflicts_with_subcommands would also
    // refuse the global -v/-q.)
    let o = &cli.open;
    if o.headless || o.browser || o.window || o.port != 0 || o.password.given() {
        usage_error("put the options after the command (e.g. `uapp open FILE --headless`)");
    }
    match cmd {
        Cmd::Open { file, opts } => open(file, opts, window),
        Cmd::New {
            file,
            encrypt,
            password,
        } => new(&file, encrypt, password),
        Cmd::Update {
            app,
            template,
            dry_run,
            keep_stale,
            template_password,
            template_password_file,
            password,
        } => {
            let template_password = match template_password_file {
                Some(p) => Some(read_first_line(&p)?),
                None => template_password,
            };
            update(
                &app,
                &template,
                dry_run,
                keep_stale,
                template_password,
                password,
            )
        }
        Cmd::Export {
            app,
            output,
            template,
            force,
            password,
        } => export(&app, &output, template, force, password),
        Cmd::Info {
            app,
            json,
            password,
        } => info(&app, json, password),
        Cmd::Encrypt { file, password } => encrypt(&file, password),
        Cmd::Decrypt { file, password } => decrypt(&file, password),
        Cmd::Passwd { file, password } => passwd(&file, password),
        Cmd::Serve(a) => serve(a),
        Cmd::Library(a) => library(a),
        Cmd::Install => install::install(),
        Cmd::Uninstall => install::uninstall(),
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "uapp", &mut std::io::stdout());
            Ok(())
        }
    }
}

// ---- passwords ----------------------------------------------------------------

/// Lines read from stdin by --password-stdin (read once: `passwd` takes the
/// new password from the second line).
fn stdin_lines() -> Result<&'static [String]> {
    static LINES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    if let Some(l) = LINES.get() {
        return Ok(l);
    }
    let mut s = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).context("reading stdin")?;
    let lines = s
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    Ok(LINES.get_or_init(|| lines))
}

fn read_first_line(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("reading the password file {}", path.display()))?;
    let line = s.split('\n').next().unwrap_or("");
    Ok(line.strip_suffix('\r').unwrap_or(line).to_string())
}

fn nonempty(p: String) -> Result<String> {
    if p.is_empty() {
        bail!("empty password");
    }
    Ok(p)
}

impl PasswordArgs {
    /// A password given explicitly (flag, stdin or file), if any.
    fn explicit(&self) -> Result<Option<String>> {
        if let Some(p) = &self.password {
            return nonempty(p.clone()).map(Some);
        }
        if self.password_stdin {
            let first = stdin_lines()?.first().cloned().unwrap_or_default();
            return nonempty(first).map(Some);
        }
        if let Some(path) = &self.password_file {
            return nonempty(read_first_line(path)?).map(Some);
        }
        Ok(None)
    }

    fn given(&self) -> bool {
        self.password.is_some() || self.password_stdin || self.password_file.is_some()
    }

    /// Explicit, else UAPP_PASSWORD.
    fn provided(&self) -> Result<Option<String>> {
        Ok(self.explicit()?.or_else(|| env_password("UAPP_PASSWORD")))
    }

    /// Provided, else a hidden prompt (only on a terminal). `confirm` asks
    /// twice, for setting a new password.
    fn resolve(&self, confirm: bool) -> Result<String> {
        if let Some(p) = self.provided()? {
            return Ok(p);
        }
        prompt("master password", confirm)
    }
}

fn env_password(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|p| !p.is_empty())
}

fn prompt(what: &str, confirm: bool) -> Result<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!(
            "no {what} given and no terminal to ask on; use --password-stdin, \
             --password-file or UAPP_PASSWORD"
        );
    }
    let p = nonempty(rpassword::prompt_password(format!("{what}: "))?)?;
    if confirm {
        let again = rpassword::prompt_password(format!("{what} (again): "))?;
        if p != again {
            bail!("passwords do not match");
        }
    }
    Ok(p)
}

/// Commands that work on an existing file say so up front, before asking for
/// a password.
fn ensure_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} doesn't exist", path.display());
    }
    if path.is_dir() {
        bail!("{} is a directory", path.display());
    }
    Ok(())
}

// ---- open ----------------------------------------------------------------------

fn open(file: Option<PathBuf>, o: OpenOpts, window: bool) -> Result<()> {
    if o.window && !cfg!(feature = "gui") {
        tracing::warn!("this build has no window; opening in the browser");
    }
    #[cfg(feature = "gui")]
    if window {
        // Encrypted with no password given: the window shows the unlock page.
        let passphrase = match &file {
            Some(f) if store::is_encrypted_file(f) => o.password.provided()?,
            _ => None,
        };
        uapp::gui::run_with(uapp::gui::Launch {
            path: file,
            passphrase,
            port: o.port,
        });
        return Ok(());
    }
    #[cfg(not(feature = "gui"))]
    let _ = window;

    let Some(f) = file else {
        // No file: a blank scratch app in a temp file, flagged "unsaved" so the
        // shell prompts to download it before it's lost. A unique temp subdir
        // lets the file be named "Untitled" (its stem becomes the app name and
        // the download filename).
        let dir = std::env::temp_dir().join(format!(
            "uapp-scratch-{}",
            (0..10)
                .map(|_| fastrand::alphanumeric())
                .collect::<String>()
        ));
        std::fs::create_dir_all(&dir)?;
        return run_serve(dir.join("Untitled.uapp"), o.headless, o.port, true, None);
    };
    let pw = resolve_open_password(&f, &o.password, o.headless)?;
    run_serve(f, o.headless, o.port, false, pw)
}

/// For `open`: if the file is encrypted, resolve the master password.
/// Interactive launches return `None` and let the server show its web unlock
/// prompt. Headless has no UI to prompt in, so it requires the password up
/// front and verifies it, failing clearly on a wrong or missing one. A
/// plaintext file just opens.
fn resolve_open_password(path: &Path, pw: &PasswordArgs, headless: bool) -> Result<Option<String>> {
    if !store::is_encrypted_file(path) {
        return Ok(None);
    }
    match pw.provided()? {
        Some(p) => {
            // Verify now so a wrong password fails clearly here rather than
            // surfacing deep in the open path (headless especially).
            if headless {
                uapp::cipher::open_file(path, &p)
                    .with_context(|| format!("opening {}", path.display()))?;
            }
            Ok(Some(p))
        }
        None if headless => bail!(
            "{} is encrypted; give the password with --password-stdin, --password-file or \
             UAPP_PASSWORD (headless mode can't show the unlock prompt)",
            path.display()
        ),
        None => Ok(None),
    }
}

/// Serve in this process, then open the browser (or print the JSON line).
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

// ---- file commands -------------------------------------------------------------

fn new(file: &Path, encrypt: bool, pw: PasswordArgs) -> Result<()> {
    let name = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "app".into());
    if encrypt || pw.given() {
        let p = pw.resolve(true)?;
        store::bootstrap(file, &name, Some(&p))?;
        println!("created {} (encrypted)", file.display());
    } else {
        store::bootstrap(file, &name, None)?;
        println!("created {}", file.display());
    }
    Ok(())
}

fn encrypt(file: &Path, pw: PasswordArgs) -> Result<()> {
    ensure_exists(file)?;
    if std::fs::metadata(file)?.len() == 0 {
        bail!(
            "{} is empty; use `uapp new --encrypt` to create it",
            file.display()
        );
    }
    if !store::looks_like_sqlite(file) {
        // Already encrypted (verify with the password) or not a .uapp at all.
        let p = pw.resolve(false)?;
        uapp::cipher::open_file(file, &p)?;
        bail!(
            "{} is already encrypted (use `uapp passwd` to change the password)",
            file.display()
        );
    }
    let p = pw.resolve(true)?;
    uapp::cipher::encrypt_file_in_place(file, &p)?;
    println!("{} is now encrypted (AES-256, SQLCipher)", file.display());
    Ok(())
}

fn decrypt(file: &Path, pw: PasswordArgs) -> Result<()> {
    ensure_exists(file)?;
    if !store::is_encrypted_file(file) {
        bail!("{} is not encrypted", file.display());
    }
    let p = pw.resolve(false)?;
    // Verify the password up front (clear error), then transform.
    uapp::cipher::open_file(file, &p)?;
    uapp::cipher::decrypt_file_in_place(file, &p)?;
    println!("{} is now plaintext", file.display());
    Ok(())
}

fn passwd(file: &Path, pw: PasswordArgs) -> Result<()> {
    ensure_exists(file)?;
    if !store::is_encrypted_file(file) {
        bail!("{} is not encrypted (nothing to change)", file.display());
    }
    let old = pw.resolve(false)?;
    // Verify before asking for the new one.
    uapp::cipher::open_file(file, &old)?;
    let from_stdin = || -> Result<Option<String>> {
        if !pw.password_stdin {
            return Ok(None);
        }
        Ok(stdin_lines()?.get(1).filter(|l| !l.is_empty()).cloned())
    };
    let new = match env_password("UAPP_NEW_PASSWORD") {
        Some(p) => p,
        None => match from_stdin()? {
            Some(p) => p,
            None => {
                use std::io::IsTerminal;
                if std::io::stdin().is_terminal() {
                    prompt("new master password", true)?
                } else if pw.given() {
                    // Older scripts passed the old password as a flag and the
                    // new one in UAPP_PASSWORD.
                    env_password("UAPP_PASSWORD")
                        .ok_or_else(|| anyhow!("no new password given; set UAPP_NEW_PASSWORD"))?
                } else {
                    bail!("no new password given; set UAPP_NEW_PASSWORD")
                }
            }
        },
    };
    uapp::cipher::rekey_file(file, &old, &new)?;
    println!("master password changed");
    Ok(())
}

/// Open an existing .uapp read-only, without taking the per-file lock (so it
/// works while the app is open in a window). Returns the connection and the
/// master password it was opened with.
fn open_readonly(file: &Path, pw: &PasswordArgs) -> Result<(rusqlite::Connection, Option<String>)> {
    ensure_exists(file)?;
    if std::fs::metadata(file)?.len() == 0 {
        bail!(
            "{} is empty (it becomes a new app the first time it's opened)",
            file.display()
        );
    }
    let (conn, key) = if store::is_encrypted_file(file) {
        let p = pw.resolve(false)?;
        (uapp::cipher::open_file_readonly(file, &p)?, Some(p))
    } else {
        let c =
            rusqlite::Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("opening {}", file.display()))?;
        (c, None)
    };
    store::check_format(&conn).with_context(|| format!("reading {}", file.display()))?;
    Ok((conn, key))
}

fn info(file: &Path, as_json: bool, pw: PasswordArgs) -> Result<()> {
    let (conn, key) = open_readonly(file, &pw)?;
    let meta = |k: &str| store::meta_get(&conn, k).ok().flatten();
    // The header holds the format; files encrypted by older builds lost it
    // there, and uapp_meta repeats it.
    let format = store::check_format(&conn)?.max(
        meta("format_version")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
    );
    let size = std::fs::metadata(file)?.len();
    let has_sqlar: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='sqlar'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    let (files, app_files, data_files): (i64, i64, i64) = if has_sqlar {
        conn.query_row(
            "SELECT count(*), coalesce(sum(name LIKE 'app/%'),0), \
             coalesce(sum(name LIKE 'data/%'),0) FROM sqlar",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    } else {
        (0, 0, 0)
    };
    // The app's own tables (uApp's bookkeeping tables are left out).
    let names: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite\\_%' \
             ESCAPE '\\' AND name NOT LIKE 'uapp\\_%' ESCAPE '\\' AND name <> 'sqlar' \
             ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut tables = Vec::new();
    for name in names {
        let q = format!("SELECT count(*) FROM \"{}\"", name.replace('"', "\"\""));
        let rows: Option<i64> = conn.query_row(&q, [], |r| r.get(0)).ok();
        tables.push((name, rows));
    }
    let name = meta("name");
    let app_id = meta("app_id");
    if as_json {
        let v = json!({
            "path": file.display().to_string(),
            "name": name,
            "app_id": app_id,
            "format_version": format,
            "encrypted": key.is_some(),
            "size": size,
            "files": {"total": files, "app": app_files, "data": data_files},
            "tables": tables.iter().map(|(n, r)| json!({"name": n, "rows": r})).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let dash = || "-".to_string();
    println!("name:       {}", name.unwrap_or_else(dash));
    println!("app id:     {}", app_id.unwrap_or_else(dash));
    println!("format:     {format}");
    println!("encrypted:  {}", if key.is_some() { "yes" } else { "no" });
    println!("size:       {} ({size} bytes)", human_size(size));
    println!("files:      {files} ({app_files} in app/, {data_files} in data/)");
    if tables.is_empty() {
        println!("tables:     none");
    } else {
        println!("tables:");
        let w = tables.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        for (n, rows) in &tables {
            match rows {
                Some(r) => println!("  {n:<w$}  {r} row{}", if *r == 1 { "" } else { "s" }),
                None => println!("  {n:<w$}  (unreadable)"),
            }
        }
    }
    Ok(())
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn export(file: &Path, out: &Path, template: bool, force: bool, pw: PasswordArgs) -> Result<()> {
    ensure_exists(file)?;
    if uapp::registry::canonical(out) == uapp::registry::canonical(file) {
        bail!("the output is the app itself; pick another file");
    }
    if out.exists() && !force {
        bail!(
            "{} already exists (pass --force to replace it)",
            out.display()
        );
    }
    let (conn, key) = open_readonly(file, &pw)?;
    let bytes = if template {
        let name = store::meta_get(&conn, "name")?.unwrap_or_else(|| {
            file.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "app".into())
        });
        store::export_template(&conn, &name)?
    } else {
        store::export_full(&conn, key.as_deref())?
    };
    // Temp file + rename, so a failed write never leaves half a file at OUT.
    let tmp = out.with_extension(format!(
        "uapp.part-{}",
        (0..6).map(|_| fastrand::alphanumeric()).collect::<String>()
    ));
    let written = (|| -> Result<()> {
        std::fs::write(&tmp, &bytes)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, out)?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", out.display()));
    }
    println!(
        "wrote {} ({}{})",
        out.display(),
        human_size(bytes.len() as u64),
        match (template, key.is_some()) {
            (true, _) => ", template",
            (false, true) => ", encrypted with the same password",
            (false, false) => "",
        }
    );
    Ok(())
}

/// Drop a newer version of an app onto an existing one: the app's code
/// (files under app/) and its schema are taken from the template; the rows,
/// uploads, chat and app identity stay exactly as they are. This is the same
/// operation the shell performs when a .uapp is dragged onto the window.
fn update(
    target: &Path,
    template: &Path,
    dry_run: bool,
    keep_stale: bool,
    template_password: Option<String>,
    pw: PasswordArgs,
) -> Result<()> {
    ensure_exists(target)?;
    ensure_exists(template)?;
    let bytes =
        std::fs::read(template).with_context(|| format!("reading {}", template.display()))?;
    let pw = resolve_open_password(target, &pw, true)?;
    let device = device_id().unwrap_or_else(|_| "cli".into());
    let user = {
        let real = whoami::realname();
        if real.trim().is_empty() {
            whoami::username()
        } else {
            real
        }
    };
    // A normal open: it takes the registry lock (so this refuses to run while
    // the app is open elsewhere) and snapshots first, which is the undo path
    // if an update turns out wrong.
    let mut eng =
        uapp::engine::Engine::open_with_passphrase(target.to_path_buf(), device, user, pw)
            .with_context(|| {
                format!(
                    "opening {} (if it's open in a window, close it first, or drop the \
                     template onto that window)",
                    target.display()
                )
            })?;
    if dry_run {
        let src = uapp::template::Source::from_bytes(&bytes, template_password.as_deref())?;
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
        a["filesWritten"],
        a["filesRemoved"],
        a["tablesCreated"],
        a["columnsAdded"],
        a["objectsUpdated"],
    );
    for w in result["warnings"].as_array().cloned().unwrap_or_default() {
        eprintln!("uapp: note: {}", w.as_str().unwrap_or_default());
    }
    Ok(())
}

/// Standard base64 (the op payload carries the template's bytes).
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---- servers -------------------------------------------------------------------

/// A usage error found after parsing (exit code 2, like clap's own).
fn usage_error(msg: impl std::fmt::Display) -> ! {
    Cli::command()
        .error(clap::error::ErrorKind::ArgumentConflict, msg)
        .exit()
}

fn serve(a: ServeArgs) -> Result<()> {
    let token = a.token.filter(|t| !t.is_empty());
    let Some(file) = a.file else {
        // No file: host the browser build alone (webhost.rs). What opens is in
        // the URL, so this process needs no archive and no token.
        for (set, flag) in [
            (a.coi, "--coi"),
            (a.no_archive, "--no-archive"),
            (a.publish_data, "--publish-data"),
            (a.asset_max_age != 0, "--asset-max-age"),
            (a.password.given(), "a password"),
        ] {
            if set {
                usage_error(format!("{flag} only applies when serving a FILE"));
            }
        }
        let Some(dir) = a.chrome else {
            usage_error("serve without a FILE needs --chrome <DIR> (the browser build to host)");
        };
        return uapp::serve_web(&a.bind, a.port, &dir, !a.no_coi, a.open);
    };
    for (set, flag) in [(a.no_coi, "--no-coi"), (a.open.is_some(), "--open")] {
        if set {
            usage_error(format!("{flag} only applies when serving without a FILE"));
        }
    }
    ensure_exists(&file)?;
    // Only ask for a password when the file actually needs one; a plaintext
    // site should serve with no prompt at all.
    let pw = if store::is_encrypted_file(&file) {
        Some(a.password.resolve(false)?)
    } else {
        None
    };
    uapp::serve_public(
        file,
        &a.bind,
        a.port,
        pw,
        token,
        uapp::app::PublicOpts {
            coi: a.coi,
            archive: !a.no_archive,
            export_data: a.publish_data,
            asset_max_age: a.asset_max_age,
        },
        a.chrome,
    )
}

fn library(a: LibraryArgs) -> Result<()> {
    use std::sync::Arc;
    let dir = if a.dir.is_absolute() {
        a.dir
    } else {
        std::env::current_dir()?.join(a.dir)
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let token = a.token.filter(|t| !t.is_empty());
    let generated = token.is_none();
    if let Some(t) = &token {
        uapp::secrets::check_token(t).context("--token / UAPP_TOKEN")?;
    }
    let token = token.unwrap_or_else(uapp::secrets::new_token);
    let lib = Arc::new(uapp::library::Library {
        dir: dir.clone(),
        token: token.clone(),
        max_bytes: a.max_mb * 1024 * 1024,
    });
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let stop = uapp::shutdown_signal();
        let listener = tokio::net::TcpListener::bind((a.bind.as_str(), a.port)).await?;
        let addr = listener.local_addr()?;
        eprintln!(
            "uapp: serving the library {} at http://{addr}/",
            dir.display()
        );
        if generated {
            eprintln!("uapp: token (pass --token or set UAPP_TOKEN to fix it): {token}");
        }
        if std::fs::metadata(&dir)
            .map(|m| m.permissions().readonly())
            .unwrap_or(true)
        {
            tracing::warn!("{} is not writable, so PUT will fail", dir.display());
        }
        axum::serve(listener, uapp::library::router(lib))
            .with_graceful_shutdown(async {
                stop.await;
                tracing::info!("shutting down");
            })
            .await?;
        Ok::<_, anyhow::Error>(())
    })
}
