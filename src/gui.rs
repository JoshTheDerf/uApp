//! Native UApp shell (Tauri v2), compiled only with `--features gui` — the
//! PRIMARY UApp experience (the browser shell is only the fallback). One
//! entry for desktop (Windows/macOS/Linux) and mobile (iOS/Android): it starts
//! the embedded `uapp` server and shows the UI in a native webview window (or
//! hands it to the system browser, per the machine-local `shell` preference /
//! a `--browser`/`--window` flag).
//!
//! The window is frameless (`decorations(false)`) — the titlebar, drag region
//! and min/max/close buttons live in the shell UI and are shown only when it
//! detects it's running natively (see `native.rs` + the shell's `win.*` RPCs).

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Native "Save…" dialog for scratch apps (opened with no file).
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let args: Vec<String> = std::env::args().collect();
            // A bare `uapp` scratch is delegated here with `--unsaved`, so the
            // shell shows the "not saved — Save…" banner.
            let unsaved = args.iter().any(|a| a == "--unsaved");
            // Mobile has no home/config dirs (the `dirs` crate returns None on
            // Android/iOS), so the core's "." fallbacks (device id, prefs, the
            // default document) would land on the read-only root. Anchor the
            // process in the app's private data dir, and point TMPDIR at its
            // cache dir.
            #[cfg(mobile)]
            {
                let p = app.path();
                if let Ok(dir) = p.app_data_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::env::set_current_dir(&dir);
                }
                if let Ok(cache) = p.app_cache_dir() {
                    let _ = std::fs::create_dir_all(&cache);
                    std::env::set_var("TMPDIR", &cache);
                }
                // Android/iOS suspend the webview when the app is backgrounded:
                // every socket drops and all JS timers freeze, so the shell's
                // WebSocket reads as "last tab closed". Disable the server's
                // exit-on-zero-clients watcher — here the OS owns the process
                // lifecycle, and the AI turn runs on a server thread that must
                // keep going while the page is suspended (see lib.rs).
                std::env::set_var("UAPP_LINGER_SECS", "never");
            }
            // Mobile: a .uapp opened from the file manager arrives as a VIEW
            // intent; MainActivity stages it under marker files in the app's
            // private dir (see gen/android's MainActivity.kt).
            #[cfg_attr(desktop, allow(unused_variables))]
            let mobile_data = {
                #[cfg(mobile)]
                {
                    app.path().app_data_dir().ok()
                }
                #[cfg(desktop)]
                {
                    None::<std::path::PathBuf>
                }
            };
            let (path, default_doc) = resolve_app_path(mobile_data.as_deref());
            // The per-user default document (bare launcher, no file) is a
            // scratch workspace: it lives in app/user data storage the user
            // never browses, so show the "Not saved" banner until it's saved
            // to (mobile: downloaded as) a real file.
            let unsaved = unsaved || default_doc;
            let url = crate::serve_background(path, 0, unsaved)
                .map_err(|e| format!("could not start the UApp server: {e:#}"))?;

            // Mobile always uses the native webview. Desktop: a CLI flag wins;
            // else, launching with no .uapp file (the app's own launcher icon
            // / a bare `uapp`) always opens natively — that's the
            // whole point of running this binary directly. The saved "Open
            // apps in" preference (default: native window) only applies when
            // a specific file was handed to us (CLI/file-association delegate).
            let flag = args.iter().find_map(|a| match a.as_str() {
                "--window" | "--tauri" | "--native" => Some(true),
                "--browser" | "--web" => Some(false),
                _ => None,
            });
            let has_file_arg = args.iter().skip(1).any(|a| a.ends_with(".uapp"));
            let native = cfg!(mobile)
                || flag.unwrap_or_else(|| !has_file_arg || crate::prefs::shell() == "native");
            if native {
                // Route the shell's window-control / save requests to this
                // window. Installed before the webview loads so the first
                // click always has somewhere to go.
                install_native_bridge(app.handle().clone());
                // (mobile builds never take the `mut` rebinding below)
                #[cfg_attr(mobile, allow(unused_mut))]
                let mut b = WebviewWindowBuilder::new(
                    app,
                    "main",
                    WebviewUrl::External(url.parse().map_err(|e| format!("bad url: {e}"))?),
                )
                .title("UApp")
                .inner_size(1180.0, 800.0);
                #[cfg(desktop)]
                {
                    b = b.decorations(false).min_inner_size(760.0, 480.0);
                }
                let win = b.build()?;
                install_navigation_handler(&win);
                install_drop_forwarding(&win);
            } else {
                let _ = open::that(&url);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the UApp desktop shell");
}

/// Forward OS file drops to the server, which relays them to the shell over
/// the WebSocket (see `native.rs`). The native runtime owns file drops on this
/// window — the webview never gets HTML5 `dragover`/`drop` events for them —
/// so without this, dragging a file into the desktop app does nothing.
fn install_drop_forwarding<R: tauri::Runtime>(win: &tauri::WebviewWindow<R>) {
    use crate::native::{deliver_drop, DropInfo, DropPhase};
    use tauri::DragDropEvent;
    // Tracked rather than queried inside the handler: asking the runtime for
    // the window's scale from a window-event callback can deadlock.
    let scale = std::sync::Arc::new(std::sync::Mutex::new(win.scale_factor().unwrap_or(1.0)));
    win.on_window_event(move |event| {
        if let tauri::WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
            *scale.lock().unwrap() = *scale_factor;
            return;
        }
        // Closing the window (X button, or the server's own Close dispatch)
        // must run the same graceful shutdown as every other exit path —
        // final rescue snapshot, registry addr removal — before Tauri exits
        // the process with the last window. Idempotent, so the quit-RPC path
        // (which cleans up first, then closes) is fine too.
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            crate::native::shutdown_server();
            return;
        }
        let tauri::WindowEvent::DragDrop(drag) = event else { return };
        // Positions arrive in physical pixels; the shell needs CSS pixels to
        // find the element under the cursor.
        let (phase, paths, pos) = match drag {
            DragDropEvent::Enter { paths, position } => {
                (DropPhase::Hover, paths.clone(), Some(*position))
            }
            DragDropEvent::Over { position } => (DropPhase::Hover, Vec::new(), Some(*position)),
            DragDropEvent::Drop { paths, position } => {
                (DropPhase::Drop, paths.clone(), Some(*position))
            }
            DragDropEvent::Leave => (DropPhase::Leave, Vec::new(), None),
            // The enum is #[non_exhaustive]; anything new is not a drop we know.
            _ => return,
        };
        let s = *scale.lock().unwrap();
        let (x, y) = pos.map(|p| (p.x / s, p.y / s)).unwrap_or((0.0, 0.0));
        deliver_drop(DropInfo { phase, paths, x, y });
    });
}

/// Install a handler that intercepts navigation requests (window.open,
/// <a target="_blank">, etc.) and opens external URLs in the system browser
/// instead of within the app's webview.
///
/// Note: Tauri v2 doesn't expose a direct navigation handler API on the
/// webview. The primary mechanism is the JavaScript-level interception in the
/// shell (see shell/js/main.js). This stub is kept for future Tauri API
/// additions or platform-specific workarounds.
fn install_navigation_handler<R: tauri::Runtime>(_win: &tauri::WebviewWindow<R>) {
    // The actual navigation interception happens via JavaScript in the shell,
    // which calls the browser.open RPC method for external URLs.
}

/// Install the process-global bridge the web UI's `win.*` / `app.save` RPCs
/// call. The closure captures the app handle so it can drive the main window
/// and show native dialogs; it lives for the process lifetime.
fn install_native_bridge(handle: tauri::AppHandle) {
    use crate::native::NativeReq;
    crate::native::install(Box::new(move |req| {
        let win = handle.get_webview_window("main");
        match req {
            // Min/max/drag are desktop-window concepts — the methods don't
            // exist on mobile targets, and the shell hides the controls there
            // (see the `desktop` flag in rpc.rs), so they no-op.
            NativeReq::Minimize => {
                #[cfg(desktop)]
                if let Some(w) = win {
                    let _ = w.minimize();
                }
                None
            }
            NativeReq::ToggleMaximize => {
                #[cfg(desktop)]
                if let Some(w) = win {
                    if w.is_maximized().unwrap_or(false) {
                        let _ = w.unmaximize();
                    } else {
                        let _ = w.maximize();
                    }
                }
                None
            }
            NativeReq::Close => {
                if let Some(w) = win {
                    let _ = w.close();
                }
                None
            }
            NativeReq::StartDrag => {
                #[cfg(desktop)]
                if let Some(w) = win {
                    let _ = w.start_dragging();
                }
                None
            }
            NativeReq::SaveDialog { default_name } => save_dialog(&handle, &default_name),
        }
    }));
}

/// Native "save as" dialog; returns the chosen absolute path (or None if the
/// user cancelled). Blocks the calling (RPC worker) thread until they decide.
fn save_dialog(handle: &tauri::AppHandle, default_name: &str) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    handle
        .dialog()
        .file()
        .set_file_name(default_name)
        .add_filter("UApp app", &["uapp"])
        .blocking_save_file()
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().to_string())
}

/// Which .uapp to open. Desktop: a path from the OS file association or the
/// command line. Mobile: a document staged by MainActivity from an "open with"
/// VIEW intent (marker files in the app's private dir), moved into the
/// per-user documents dir and opened. Otherwise a per-user default document —
/// the second tuple field is true for that fallback case.
#[cfg_attr(desktop, allow(unused_variables))]
fn resolve_app_path(mobile_data: Option<&std::path::Path>) -> (std::path::PathBuf, bool) {
    #[cfg(mobile)]
    if let Some(data_root) = mobile_data {
        // MainActivity writes the markers into Context.dataDir, which is what
        // app_data_dir() resolves to on Android today — but also check its
        // files/ subdir (Context.filesDir) so a change in either side's idea
        // of "the data dir" degrades gracefully instead of silently ignoring
        // "open with" (which is exactly how the original bug presented).
        let candidates = [data_root.to_path_buf(), data_root.join("files")];
        for dir in candidates.iter().filter(|d| d.exists()) {
        // Launched from a pinned launcher shortcut (shell → Install button):
        // the marker holds the absolute path of the exact .uapp to open.
        let marker = dir.join("shortcut_open.path");
        if let Ok(p) = std::fs::read_to_string(&marker) {
            let _ = std::fs::remove_file(&marker);
            let p = std::path::PathBuf::from(p.trim());
            if p.is_absolute() && p.exists() {
                return (p, false);
            }
        }
        let pending = dir.join("pending_open.uapp");
        if pending.exists() {
            let name = std::fs::read_to_string(dir.join("pending_open.name"))
                .ok()
                .and_then(|s| {
                    // The display name originates from the sending app's
                    // intent — strip path separators/control chars so it
                    // can't escape the documents dir when joined below.
                    let s: String = s
                        .trim()
                        .chars()
                        .map(|c| if c == '/' || c == '\\' || c.is_control() { '-' } else { c })
                        .collect();
                    let s = s.trim_matches('.').trim().to_string();
                    if s.is_empty() {
                        None
                    } else if s.ends_with(".uapp") {
                        Some(s)
                    } else {
                        Some(format!("{s}.uapp"))
                    }
                })
                .unwrap_or_else(|| "Shared App.uapp".into());
            // The documents dir stays anchored at the data root even when the
            // marker was found in the files/ fallback location.
            let docs = data_root.join("UApp");
            let _ = std::fs::create_dir_all(&docs);
            let dest = docs.join(&name);
            if std::fs::rename(&pending, &dest).is_err() {
                if std::fs::copy(&pending, &dest).is_ok() {
                    let _ = std::fs::remove_file(&pending);
                }
            }
            let _ = std::fs::remove_file(dir.join("pending_open.name"));
            return (dest, false);
        }
        }
    }
    #[cfg(desktop)]
    {
        if let Some(arg) = std::env::args().skip(1).find(|a| a.ends_with(".uapp")) {
            return (std::path::PathBuf::from(arg), false);
        }
    }
    let dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("UApp");
    let _ = std::fs::create_dir_all(&dir);
    (dir.join("My App.uapp"), true)
}
