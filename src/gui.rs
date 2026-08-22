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
                install_permission_handling(&win);
                install_permission_prompt(app.handle().clone());
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

/// Install the NATIVE permission dialog.
///
/// This is the whole security boundary, so it is worth being explicit about why
/// it is not shell UI. The shell and the app iframe share one origin: app code
/// can reach `window.parent`, and a dynamic `import()` from the parent realm
/// hands it the same live module instances (and therefore the same privileged
/// socket) the shell uses. A prompt drawn by the page can be answered by the
/// page. So the decision is made in an OS dialog instead — the same reason a
/// browser paints permission prompts in its chrome and not in the document.
///
/// Runs on a worker thread (see `permissions::decide`), which is what the
/// `blocking_*` dialog API requires; calling it on the UI thread would deadlock
/// against the window it is modal to.
fn install_permission_prompt(handle: tauri::AppHandle) {
    crate::permissions::install_prompt(Box::new(move |ask| {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        let t = ask.prompt_text();
        // `mut` is only taken up by the desktop-only `parent` call below.
        #[allow(unused_mut)]
        let mut d = handle
            .dialog()
            .message(&t.body)
            .title(&t.title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::YesNoCancelCustom(
                t.always_allow.to_string(),
                t.allow_once.to_string(),
                t.deny.to_string(),
            ));
        // Modal to the app window, so it can't be lost behind it and answered
        // later against a request the user has forgotten the context of.
        // Desktop only: the mobile dialog API has no `parent` — Android and iOS
        // show the dialog over the activity/scene regardless.
        #[cfg(desktop)]
        if let Some(w) = handle.get_webview_window("main") {
            d = d.parent(&w);
        }
        // Custom button labels come back as `Custom(label)` — on Linux for ALL
        // THREE buttons, so matching Yes/No here would silently deny every
        // request no matter what was clicked. Match the labels we just handed
        // in, and keep Yes/No as a fallback for platforms that report those.
        use tauri_plugin_dialog::MessageDialogResult as R;
        let result = d.blocking_show_with_result();

        // Hand focus back to the app window after the dialog. This is a
        // courtesy, not a fix: waiting for focus to land before delivering the
        // grant does NOT rescue pointer lock (see the inline path in the
        // permission handler), it just delays every other permission.
        if let Some(w) = handle.get_webview_window("main") {
            let _ = w.set_focus();
        }

        match &result {
            R::Custom(s) if s.as_str() == t.always_allow => (true, true),
            R::Custom(s) if s.as_str() == t.allow_once => (true, false),
            R::Custom(s) if s.as_str() == t.deny => (false, false),
            R::Yes => (true, true),
            R::No => (true, false),
            // Dismissing the dialog is a denial, the safe direction. Anything
            // genuinely unrecognised denies too, but says so — a silent
            // fall-through here is what made every button mean "deny" once.
            R::Cancel => (false, false),
            other => {
                eprintln!(
                    "uapp: unrecognised permission dialog result {other:?} — denying. \
                     This is a bug: the button labels and the result mapping disagree."
                );
                (false, false)
            }
        }
    }));
}

/// Make the gated `navigator.*` APIs work in the native window, and put the
/// user in charge of them.
///
/// Every platform webview answers a permission request differently, so this is
/// where they're brought to the same behaviour — a per-app Allow/Deny prompt
/// brokered by [`crate::permissions`]:
///
/// * **Linux (WebKitGTK)** — anything the embedder doesn't answer is *denied*,
///   and the page is told "denied" as if the user had said so, which is why an
///   unhandled feature looks like a settings problem rather than a missing
///   handler. Capture is also compiled in but switched off: without
///   `enable-media-stream` there is no `navigator.mediaDevices` at all.
/// * **Windows (WebView2)** — wry answers only clipboard reads, leaving the
///   rest to WebView2's own prompt. We take them over so decisions are
///   remembered per .uapp rather than per WebView2 profile.
/// * **macOS/iOS (WKWebView)** — wry's UI delegate grants capture
///   unconditionally, so the only gate is the OS-level TCC prompt, whose usage
///   descriptions live in `installer/macos/Info.plist`. A per-app gate would
///   mean replacing wry's delegate wholesale, taking the file panel and JS
///   dialogs with it, so it's left to the OS for now.
/// * **Android** — wry's `RustWebChromeClient` already forwards capture and
///   geolocation to the runtime permission dialog; it only needs the manifest
///   entries (see `gen/android/app/src/main/AndroidManifest.xml`).
#[cfg(target_os = "linux")]
fn install_permission_handling<R: tauri::Runtime>(win: &tauri::WebviewWindow<R>) {
    use crate::permissions::{Ask, Feature};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use webkit2gtk::glib;
    use webkit2gtk::glib::prelude::*;
    use webkit2gtk::glib::translate::ToGlibPtr;
    use webkit2gtk::{
        DeviceInfoPermissionRequest, GeolocationPermissionRequest,
        NotificationPermissionRequest, PermissionRequest, PermissionRequestExt,
        PointerLockPermissionRequest, SettingsExt, UserMediaPermissionRequest,
        UserMediaPermissionRequestExt, WebViewExt, WebsiteDataAccessPermissionRequest,
        WebsiteDataAccessPermissionRequestExt,
    };

    // Requests waiting on the user. WebKit hands them to us on the GTK main
    // thread and they are not `Send`, so they stay parked here and are only
    // ever touched again from a main-thread idle callback.
    thread_local! {
        static OPEN: RefCell<HashMap<u64, PermissionRequest>> = RefCell::new(HashMap::new());
    }
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    let res = win.with_webview(|wv| {
        let webview = wv.inner();
        if let Some(settings) = WebViewExt::settings(&webview) {
            // Without this getUserMedia is absent, not merely denied.
            settings.set_enable_media_stream(true);
            // Lets apps feature-detect codecs before asking for a device.
            settings.set_enable_media_capabilities(true);
            // NOT enabling javascript-can-access-clipboard, deliberately.
            // Clipboard reads are refused while it is off even after the
            // permission request is allowed, so `clipboard.readText()` fails
            // with NotAllowedError despite a grant — annoying, but correct.
            // Turning it on was tested and is NOT safe: WebKitGTK 2.52 then
            // reads the clipboard with no WebKitClipboardPermissionRequest at
            // all, so the switch bypasses the prompt rather than deferring to
            // it. A clipboard that silently reads beats no clipboard only if
            // you don't care who is reading. Revisit if upstream starts
            // raising the request in both configurations.
        }
        webview.connect_permission_request(move |_, req| {
            // Device labels are not a question of their own: browsers expose
            // them once a capture permission exists, so answer from the
            // standing grant and never raise a dialog for enumerateDevices.
            if req.clone().downcast::<DeviceInfoPermissionRequest>().is_ok() {
                if crate::permissions::any_allowed(&[Feature::Camera, Feature::Microphone]) {
                    req.allow();
                } else {
                    req.deny();
                }
                return true;
            }

            let ask = if let Ok(media) = req.clone().downcast::<UserMediaPermissionRequest>() {
                // Screen capture arrives as the SAME request type as the
                // webcam. Without splitting them, a remembered "camera: allow"
                // would hand an app the whole screen — so ask the C API, which
                // has the getter the Rust binding is still missing.
                let display = unsafe {
                    webkit2gtk_sys::webkit_user_media_permission_is_for_display_device(
                        media.to_glib_none().0,
                    ) != 0
                };
                if display {
                    Ask::one(Feature::Screen)
                } else {
                    let mut f = Vec::new();
                    if media.is_for_video_device() {
                        f.push(Feature::Camera);
                    }
                    if media.is_for_audio_device() {
                        f.push(Feature::Microphone);
                    }
                    Ask::new(f)
                }
            } else if req.clone().downcast::<GeolocationPermissionRequest>().is_ok() {
                Ask::one(Feature::Location)
            } else if req.clone().downcast::<NotificationPermissionRequest>().is_ok() {
                Ask::one(Feature::Notifications)
            } else if req.clone().downcast::<PointerLockPermissionRequest>().is_ok() {
                Ask::one(Feature::PointerLock)
            } else if let Ok(sa) = req
                .clone()
                .downcast::<WebsiteDataAccessPermissionRequest>()
            {
                // An embedded third-party frame asking for its own cookies.
                // "This app wants storage access" would be meaningless, so the
                // prompt names both sides, and the decision is remembered
                // against the requesting domain rather than the app as a whole.
                let requesting = sa.requesting_domain().map(|d| d.to_string()).unwrap_or_default();
                let current = sa.current_domain().map(|d| d.to_string()).unwrap_or_default();
                if requesting.is_empty() {
                    return false; // nothing to name, nothing to scope: deny
                }
                Ask::one(Feature::StorageAccess).scoped(
                    requesting.clone(),
                    if current.is_empty() {
                        requesting
                    } else {
                        format!("{requesting} (embedded in {current})")
                    },
                )
            } else if req.type_().name() == "WebKitClipboardPermissionRequest" {
                // Added in WebKitGTK 2.52 and not in the Rust bindings yet, so
                // it's matched by type name; the base interface is all that's
                // needed to answer it.
                Ask::one(Feature::ClipboardRead)
            } else {
                // DRM key systems, missing-codec installs, XR sessions: keep
                // WebKit's default, which is to deny.
                return false;
            };
            if ask.is_empty() {
                return false;
            }

            // Pointer lock is checked against the focused window at the
            // instant it is granted, and any dialog we show takes that focus —
            // so a deferred answer can never succeed, however carefully focus
            // is handed back afterwards. Answer it inline from what we already
            // know; when we don't know yet, refuse THIS attempt and ask out of
            // band, so the next click goes through with no dialog at all.
            // (Refusing and re-asking on every attempt is what made it look
            // like the prompt was stuck in a loop.)
            if ask.features == [Feature::PointerLock] {
                match crate::permissions::known(&ask) {
                    Some(true) => req.allow(),
                    Some(false) => req.deny(),
                    None => {
                        req.deny();
                        crate::permissions::ask_in_background(ask);
                    }
                }
                return true;
            }

            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            OPEN.with(|m| m.borrow_mut().insert(id, req.clone()));
            // Deciding may need to paint a prompt in this very webview, so it
            // cannot happen on this thread: answer later, from an idle
            // callback, and tell WebKit we've taken the request.
            std::thread::spawn(move || {
                let allow = crate::permissions::decide(ask);
                glib::idle_add_once(move || {
                    OPEN.with(|m| {
                        if let Some(r) = m.borrow_mut().remove(&id) {
                            if allow {
                                r.allow();
                            } else {
                                r.deny();
                            }
                        }
                    });
                });
            });
            true
        });
    });
    if let Err(e) = res {
        eprintln!("uapp: could not install permission handling: {e}");
    }
}

/// Windows: take the gated permissions over from WebView2's built-in prompt so
/// decisions are remembered per .uapp. See the Linux arm for the rationale.
///
/// WebView2 exposes no permission kind for `getDisplayMedia`, pointer lock or
/// storage access, so those three are not brokered here — they keep whatever
/// WebView2 (i.e. Chromium) does by default, which for pointer lock is to allow
/// after a user gesture and show its own "press Esc to exit" notice.
#[cfg(target_os = "windows")]
fn install_permission_handling<R: tauri::Runtime>(win: &tauri::WebviewWindow<R>) {
    use crate::permissions::{Ask, Feature};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PERMISSION_KIND_CAMERA, COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ,
        COREWEBVIEW2_PERMISSION_KIND_GEOLOCATION, COREWEBVIEW2_PERMISSION_KIND_MICROPHONE,
        COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
        COREWEBVIEW2_PERMISSION_STATE_DENY,
    };
    use webview2_com::PermissionRequestedEventHandler;

    // Answers for the requests waiting on the user, each one holding the COM
    // args + deferral it will complete. WebView2 is apartment-threaded and its
    // interfaces are not `Send`, so — exactly as on Linux — they stay parked
    // here on the UI thread and are only ever touched again from a main-thread
    // callback. Boxing the answer as a closure keeps the COM types out of the
    // map's signature.
    thread_local! {
        static OPEN: RefCell<HashMap<u64, Box<dyn FnOnce(bool)>>> = RefCell::new(HashMap::new());
    }
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    let handle = win.app_handle().clone();
    let res = win.with_webview(move |wv| unsafe {
        let Ok(core) = wv.controller().CoreWebView2() else {
            return;
        };
        let mut token = Default::default();
        let _ = core.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = Default::default();
                args.PermissionKind(&mut kind)?;
                let feature = if kind == COREWEBVIEW2_PERMISSION_KIND_CAMERA {
                    Feature::Camera
                } else if kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE {
                    Feature::Microphone
                } else if kind == COREWEBVIEW2_PERMISSION_KIND_GEOLOCATION {
                    Feature::Location
                } else if kind == COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS {
                    Feature::Notifications
                } else if kind == COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ {
                    Feature::ClipboardRead
                } else {
                    return Ok(()); // not ours; leave WebView2's default
                };
                // WebView2 lets us answer after the handler returns, which is
                // what keeps the prompt (painted in this same webview) from
                // deadlocking against the request.
                let deferral = args.GetDeferral()?;
                let args = args.clone();
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                OPEN.with(|m| {
                    m.borrow_mut().insert(
                        id,
                        Box::new(move |allow| {
                            let _ = args.SetState(if allow {
                                COREWEBVIEW2_PERMISSION_STATE_ALLOW
                            } else {
                                COREWEBVIEW2_PERMISSION_STATE_DENY
                            });
                            let _ = deferral.Complete();
                        }),
                    );
                });
                // Deciding paints a modal dialog, so it cannot happen on this
                // thread; the answer then has to hop back, because the objects
                // it completes belong to this one.
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let allow = crate::permissions::decide(Ask::one(feature));
                    let _ = handle.run_on_main_thread(move || {
                        if let Some(answer) = OPEN.with(|m| m.borrow_mut().remove(&id)) {
                            answer(allow);
                        }
                    });
                });
                Ok(())
            })),
            &mut token,
        );
    });
    if let Err(e) = res {
        eprintln!("uapp: could not install permission handling: {e}");
    }
}

/// macOS/iOS/Android: the platform already routes these requests (see the Linux
/// arm's doc comment) — nothing to install here.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn install_permission_handling<R: tauri::Runtime>(_win: &tauri::WebviewWindow<R>) {}

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
            // Native, for the same reason the permission dialog is: the page
            // asking to forget these grants is the page they are about.
            NativeReq::ConfirmResetPermissions => {
                use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
                let ok = handle
                    .dialog()
                    .message("Forget this app's camera, microphone, location and other permission decisions?")
                    .title("Reset permissions")
                    .buttons(MessageDialogButtons::OkCancel)
                    .blocking_show();
                Some(if ok { "yes".into() } else { "no".into() })
            }
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
