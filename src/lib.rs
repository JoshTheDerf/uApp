//! uapp core, exposed as a library so the native Tauri shells — the PRIMARY
//! UApp experience, desktop and mobile (`--features gui`) — embed the exact
//! same engine + server + UI. The CLI in `main.rs` wraps the same server and
//! uses the system browser only as the fallback shell.

pub mod ai;
pub mod app;
#[cfg(not(target_arch = "wasm32"))]
pub mod cipher;
pub mod engine;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(not(target_arch = "wasm32"))]
pub mod install;
pub mod mcp;
#[cfg(not(target_arch = "wasm32"))]
pub mod permissions;
pub mod native;
pub mod net;
pub mod prefs;
#[cfg(not(target_arch = "wasm32"))]
pub mod registry;
pub mod rpc;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;
#[cfg(not(target_arch = "wasm32"))]
pub mod shortcut;
pub mod store;
pub mod template;
pub mod tools;
#[cfg(target_arch = "wasm32")]
pub mod wasm;

#[cfg(not(target_arch = "wasm32"))]
use anyhow::{Context, Result};
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
mod native_serve {
use super::*;

/// Called once the server is listening: `(url, port, reused)`. The CLI uses it
/// to open a browser or print JSON; an embedder uses it to load the URL into a
/// webview.
pub type OnReady = Box<dyn FnOnce(&str, u16, bool) + Send>;

/// Stable per-user-per-machine device id, e.g. "karen-a3f9k2".
/// UAPP_DEVICE overrides (useful for testing several "devices" on one box).
pub fn device_id() -> Result<String> {
    if let Ok(d) = std::env::var("UAPP_DEVICE") {
        if !d.is_empty() {
            return Ok(d);
        }
    }
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uapp");
    std::fs::create_dir_all(&dir)?;
    let f = dir.join("device");
    if let Ok(s) = std::fs::read_to_string(&f) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let user: String = whoami::username()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_lowercase();
    let rand: String = (0..6).map(|_| fastrand::lowercase()).collect();
    let id = format!("{}-{}", if user.is_empty() { "user".into() } else { user }, rand);
    std::fs::write(&f, &id)?;
    Ok(id)
}

/// If this machine already serves this file, the running server can be reused.
/// Returns its `(url, port)` if it is healthy.
fn reusable_server(path: &Path) -> Option<(String, u16)> {
    let (port, token) = registry::read_addr(path)?;
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    let Ok(resp) = ureq::get(&format!("http://127.0.0.1:{port}/health"))
        .timeout(std::time::Duration::from_millis(1500))
        .call()
    else {
        return None;
    };
    // A lingering server from an older build would otherwise be reused
    // silently — a fresh `uapp open` after an upgrade must run the new code.
    // …and a server that has a DIFFERENT file open (a crashed server's .addr
    // outlived it and the port was recycled) is not ours either.
    if resp.header("x-uapp-key").map(|k| k != registry::key(path)).unwrap_or(true) {
        return None;
    }
    let theirs = resp.header("x-uapp-version").unwrap_or("");
    if theirs != env!("CARGO_PKG_VERSION") {
        eprintln!(
            "uapp: a server of another version ({}) still has {} open — stop it (or wait for it to exit) so this build can open the file",
            if theirs.is_empty() { "unknown" } else { theirs },
            path.display()
        );
        return None;
    }
    Some((url, port))
}

/// Open a .uapp and serve it on 127.0.0.1. Blocks until the server exits.
/// `on_ready` fires once the listener is bound (or a reusable server is found).
/// `unsaved` marks a scratch app (opened with no file) so the shell shows a
/// "not saved yet — download to keep it" banner.
pub fn serve_opts(
    path: PathBuf,
    fixed_port: u16,
    unsaved: bool,
    passphrase: Option<String>,
    on_ready: OnReady,
) -> Result<()> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let device = device_id()?;
    let user = {
        let real = whoami::realname();
        if real.trim().is_empty() { whoami::username() } else { real }
    };

    let token: String = (0..32).map(|_| fastrand::alphanumeric()).collect();
    let rt = tokio::runtime::Runtime::new()?;
    let mut passphrase = passphrase;
    let mut on_ready = Some(on_ready);

    // Bind the app's socket up front. It's already listening, so a request sent
    // to it (the unlock page's redirect) just queues until we start serving —
    // no re-binding or polling needed.
    let listener = rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", fixed_port)))?;
    let port = listener.local_addr()?.port();
    let app_url = format!("http://127.0.0.1:{port}/?t={token}");

    // Encrypted file without a usable password? Show a small web unlock page —
    // the prompt the browser and Tauri window get instead of failing — and
    // block until it's entered. On success it redirects to the app server above.
    if store::is_encrypted_file(&path) {
        let have_valid = passphrase
            .as_deref()
            .map_or(false, |pw| crate::cipher::open_file(&path, pw).is_ok());
        if !have_valid {
            // Already open (and unlocked) on this machine? Redirect to it
            // instead of prompting a second time.
            if let Some((url, port)) = reusable_server(&path) {
                if let Some(or) = on_ready.take() {
                    or(&url, port, true);
                }
                return Ok(());
            }
            // Point the browser / native window at the app URL now: the same
            // socket serves the unlock page until it's unlocked, then the app.
            if let Some(or) = on_ready.take() {
                or(&app_url, port, false);
            }
            passphrase = Some(rt.block_on(server::run_unlock(&listener, &path))?);
        }
    }

    let opened =
        engine::Engine::open_with_passphrase(path.clone(), device.clone(), user.clone(), passphrase);
    let eng = match opened {
        Ok(e) => e,
        Err(e) => {
            // Locked (or a race): another instance has this file open — hand
            // the caller its window instead.
            let reuse = reusable_server(&path);
            if let Some(or) = on_ready.take() {
                // The caller hasn't been pointed anywhere yet: give it the
                // running server's URL, or surface the error.
                match &reuse {
                    Some((url, port)) => {
                        or(url, *port, true);
                        return Ok(());
                    }
                    None => return Err(e).with_context(|| format!("opening {}", path.display())),
                }
            }
            // on_ready already fired (the unlock page pointed the tab HERE) and
            // we can no longer serve on this socket: answer the waiting tab
            // with a redirect to the running server, or a readable error —
            // never a bare "connection refused".
            let (redirect, message) = match &reuse {
                Some((url, _)) => (Some(url.clone()), String::new()),
                None => (
                    None,
                    format!(
                        "<!doctype html><meta charset=utf-8><title>Couldn't open</title>                         <body style=\"font:15px system-ui;margin:3em;color:#333\">                         <h1>Couldn't open this app</h1><p>{}</p>",
                        e
                    ),
                ),
            };
            rt.block_on(server::run_notice(&listener, redirect, &message));
            return Ok(());
        }
    };
    let app = Arc::new(app::App::new(eng, token.clone(), unsaved));

    // Native window only: forward OS file drops to the shell (see native.rs +
    // App::native_drop). The webview never gets HTML5 drop events for them, so
    // this is the path that makes drag-and-drop work in the desktop app.
    {
        let app = app.clone();
        native::install_drop_sink(Box::new(move |info| app.native_drop(info)));
    }

    // Permission grants are filed per app, so the broker needs to know which
    // .uapp is open. It deliberately gets NOTHING else from the server: the
    // prompt itself is a native dialog installed by `gui.rs`, because anything
    // reachable from the page is reachable from untrusted app code.
    {
        let id_app = app.clone();
        permissions::install_app_id(Box::new(move || {
            id_app.engine.lock().unwrap().app_id.clone()
        }));
    }

    rt.block_on(async move {
        // The socket was bound up front (and may have just served the unlock
        // page); the app now takes it over — same port, no rebind.
        app.port.store(port, std::sync::atomic::Ordering::Relaxed);
        let url = app_url;

        // Advertise for double-open reuse (rewritten by app.save when the
        // backing file changes — see rpc.rs).
        {
            let eng = app.engine.lock().unwrap();
            registry::write_addr(&eng.path, port, &token);
        }

        // Background snapshot timer: refresh the rolling rescue snapshot while
        // writes keep happening (commits themselves are already durable).
        {
            let app = app.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    let app2 = app.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        app2.engine.lock().unwrap().maybe_snapshot();
                    })
                    .await;
                }
            });
        }

        // Exit when the last browser tab disconnects (or none ever connects).
        // The op log makes this always safe — nothing in memory is precious.
        {
            let app = app.clone();
            // "never" disables this entirely — used by the mobile GUI (gui.rs):
            // there the OS owns the process lifecycle, and Android suspends the
            // WebView (dropping every socket, freezing every timer) whenever
            // the app is backgrounded. Exiting on zero clients there would kill
            // an in-flight AI turn the moment the user switches apps for a
            // couple of minutes.
            let linger_never = std::env::var("UAPP_LINGER_SECS")
                .map(|v| v.eq_ignore_ascii_case("never") || v.eq_ignore_ascii_case("off"))
                .unwrap_or(false);
            let linger_ms: u64 = std::env::var("UAPP_LINGER_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(90)
                * 1000;
            if !linger_never {
                tokio::spawn(async move {
                let mut zero_since = Some(std::time::Instant::now());
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    tick.tick().await;
                    let n = app.clients.load(std::sync::atomic::Ordering::SeqCst);
                    if n > 0 {
                        zero_since = None;
                        continue;
                    }
                    let since = *zero_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed().as_millis() as u64 >= linger_ms
                        && !app.ai_busy_any()
                    {
                        eprintln!("uapp: last client disconnected — shutting down");
                        app.graceful_cleanup();
                        // A native window whose webview dropped the socket
                        // (frozen/suspended) may still be up — close it too.
                        crate::native::dispatch(crate::native::NativeReq::Close);
                        std::process::exit(0);
                    }
                }
                });
            }
        }

        if let Some(or) = on_ready {
            or(&url, port, false);
        }

        // The native desktop window's close (X button) runs the same cleanup
        // via this hook before the process exits with the window.
        crate::native::install_shutdown(Box::new({
            let app = app.clone();
            move || app.graceful_cleanup()
        }));

        let router = server::router(app.clone());
        let shutdown_app = app.clone();
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                shutdown_app.graceful_cleanup();
                eprintln!("uapp: shutting down");
            })
            .await?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Embedder entry (native shells): open `path` on a background thread and
/// return the URL to load into a webview once the server is listening. The
/// server runs until the process exits.
pub fn serve_background(path: PathBuf, fixed_port: u16, unsaved: bool) -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        let tx2 = tx.clone();
        let on_ready: OnReady = Box::new(move |url, _port, _reused| {
            let _ = tx2.send(Ok(url.to_string()));
        });
        if let Err(e) = serve_opts(path, fixed_port, unsaved, None, on_ready) {
            let _ = tx.send(Err(format!("{e:#}")));
        }
    });
    match rx.recv() {
        Ok(Ok(url)) => Ok(url),
        Ok(Err(e)) => anyhow::bail!("{e}"),
        Err(_) => anyhow::bail!("server thread exited before binding"),
    }
}

/// Serve one .uapp as a PUBLIC website (`uapp serve`). Blocks until interrupted.
///
/// The same router as the desktop server (`server.rs`), with `App::public`
/// set: anonymous requests see the site's pages and `/site.uapp`; `token`
/// unlocks the shell, the WebSocket and `PUT /site.uapp` (publishing an
/// edited copy back). None of the desktop lifecycle applies — no browser to
/// open, no exit when the last client leaves, no registry advertisement.
///
/// The file is opened read-write by the normal engine (so a publish can land
/// and so the crash-rescue snapshots exist), which also means it takes the
/// registry lock: the desktop app cannot have the same file open at once.
pub fn serve_public(
    path: PathBuf,
    bind: &str,
    port: u16,
    passphrase: Option<String>,
    token: Option<String>,
    opts: app::PublicOpts,
    chrome_dir: Option<PathBuf>,
) -> Result<()> {
    let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
    anyhow::ensure!(
        passphrase.is_some() || !store::is_encrypted_file(&path),
        "{} is encrypted — pass --password (or set UAPP_PASSWORD) to serve it",
        path.display()
    );
    let eng = engine::Engine::open_with_passphrase(path.clone(), device_id()?, "site".into(), passphrase)
        .with_context(|| format!("opening {}", path.display()))?;
    let generated = token.is_none();
    let token = token.unwrap_or_else(|| (0..32).map(|_| fastrand::alphanumeric()).collect());
    let mut app = app::App::new(eng, token.clone(), false);
    app.public = Some(opts);
    app.chrome = match &chrome_dir {
        Some(dir) => Some(server::load_chrome(dir)?),
        None => None,
    };
    let app = Arc::new(app);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        let addr = listener.local_addr()?;
        app.port.store(addr.port(), std::sync::atomic::Ordering::Relaxed);
        eprintln!("uapp: serving {} at http://{}/", path.display(), addr);
        if opts.archive {
            eprintln!("uapp: archive endpoint: http://{addr}/site.uapp (PUT with the token publishes)");
        }
        if generated {
            eprintln!("uapp: token (pass --token or set UAPP_TOKEN to fix it): {token}");
        }
        if std::fs::OpenOptions::new().write(true).open(&path).is_err() {
            eprintln!("uapp: {} is not writable — publishing (PUT /site.uapp) will fail", path.display());
        }
        match &chrome_dir {
            Some(dir) => eprintln!("uapp: editing chrome enabled (web build: {})", dir.display()),
            None => eprintln!("uapp: no editing chrome (pass --chrome <dist-web> to enable)"),
        }
        // Rolling rescue snapshot while publishes keep landing.
        {
            let app = app.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    let app2 = app.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        app2.engine.lock().unwrap().maybe_snapshot();
                    })
                    .await;
                }
            });
        }
        let shutdown_app = app.clone();
        axum::serve(listener, server::router(app))
            .with_graceful_shutdown(async move {
                let _ = tokio::signal::ctrl_c().await;
                shutdown_app.graceful_cleanup();
            })
            .await?;
        Ok::<_, anyhow::Error>(())
    })
}

}
#[cfg(not(target_arch = "wasm32"))]
pub use native_serve::*;
