fn main() {
    // tauri-build (below) sets the `desktop`/`mobile` cfgs on gui builds; the
    // core also reads them (rpc "desktop" flag) on default builds, where
    // they'd otherwise warn as unexpected.
    println!("cargo:rustc-check-cfg=cfg(desktop, mobile)");
    // Only the Tauri (gui) build needs codegen from tauri.conf.json; the
    // default CLI build does nothing here.
    #[cfg(feature = "gui")]
    tauri_build::build();
}
