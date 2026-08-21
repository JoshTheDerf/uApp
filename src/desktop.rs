// The native UApp desktop app (Tauri) — the PRIMARY distributed binary.
// Built as `uapp` with the default `gui` feature.
// Windowless on Windows release so no console appears behind the native window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    uapp::gui::run();
}
