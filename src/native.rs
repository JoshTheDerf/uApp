//! A process-global bridge from the RPC layer to the native desktop shell
//! (Tauri). When the app runs inside the `uapp` window, `gui.rs`
//! installs a handler here; the RPC methods `win.*` and `app.save` call it. In
//! the plain browser CLI no handler is installed, so `is_native()` is false and
//! those methods are inert — and the shell hides the custom window controls
//! entirely (a browser tab has its own chrome).
//!
//! This lives in the core (no Tauri types) so the default, GUI-less build still
//! compiles; the handler is the only thing that touches Tauri, and it's boxed.

use std::sync::OnceLock;

/// A request from the web UI that only the native window can satisfy.
pub enum NativeReq {
    Minimize,
    /// Maximize if restored, restore if maximized.
    ToggleMaximize,
    Close,
    /// Begin an OS window-move (called on titlebar mousedown).
    StartDrag,
    /// Show a native "save as" dialog seeded with `default_name`; returns the
    /// chosen absolute path, or None if the user cancelled.
    SaveDialog { default_name: String },
    /// Show a native "open" dialog filtered to `.uapp` files; returns the
    /// chosen absolute path, or None if the user cancelled. The way in when
    /// the machine has no .uapp file association to double-click.
    OpenDialog,
}

type Handler = Box<dyn Fn(NativeReq) -> Option<String> + Send + Sync>;
static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Install the native handler (called once by the desktop shell at startup).
pub fn install(h: Handler) {
    let _ = HANDLER.set(h);
}

/// True when running inside the native desktop window.
pub fn is_native() -> bool {
    HANDLER.get().is_some()
}

/// Dispatch a request to the native shell. Returns None (and does nothing) in
/// the browser CLI; otherwise the handler's result.
pub fn dispatch(req: NativeReq) -> Option<String> {
    HANDLER.get().and_then(|h| h(req))
}

// ---- reverse direction: the native shell asking the SERVER to clean up ----
// The desktop window's close (X button / OS close) must run the same graceful
// shutdown as every other exit path — final rescue snapshot and registry
// addr-file removal — before the process dies with the window. The server
// installs its cleanup here at startup; gui.rs calls it on CloseRequested.

type ShutdownHook = Box<dyn Fn() + Send + Sync>;
static SHUTDOWN: OnceLock<ShutdownHook> = OnceLock::new();

/// Install the server's graceful-cleanup hook (called once at serve start).
pub fn install_shutdown(h: ShutdownHook) {
    let _ = SHUTDOWN.set(h);
}

/// Run the server's graceful cleanup, if a server is running. Safe to call
/// more than once (the cleanup itself is idempotent) and from any thread.
pub fn shutdown_server() {
    if let Some(h) = SHUTDOWN.get() {
        h();
    }
}

/// A file drag from the OS into the native window. The webview never sees these
/// as HTML5 drag events: the native runtime claims file drops (that's what
/// makes plain `drop` handlers dead in the desktop app), so the window hands
/// them here and the server replays them to the shell over the WebSocket.
pub enum DropPhase {
    /// Drag entered the window / moved inside it — used only to light up the
    /// drop target under the cursor.
    Hover,
    /// Dropped: `paths` are the OS paths, to be read server-side.
    Drop,
    /// Cancelled or left the window.
    Leave,
}

pub struct DropInfo {
    pub phase: DropPhase,
    pub paths: Vec<std::path::PathBuf>,
    /// Cursor position in CSS pixels relative to the webview's top-left, so the
    /// shell can find the element under it with elementFromPoint().
    pub x: f64,
    pub y: f64,
}

type DropSink = Box<dyn Fn(DropInfo) + Send + Sync>;
static DROP_SINK: OnceLock<DropSink> = OnceLock::new();

/// Register the server-side receiver for native file drops (called once when
/// the server starts). One app per process, so a `OnceLock` is enough.
pub fn install_drop_sink(sink: DropSink) {
    let _ = DROP_SINK.set(sink);
}


/// Hand a native drop to the server. No-op when nothing is listening.
pub fn deliver_drop(info: DropInfo) {
    if let Some(sink) = DROP_SINK.get() {
        sink(info);
    }
}
