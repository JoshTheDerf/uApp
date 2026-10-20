# Building uApp

Everything builds one binary, `uapp`. With the default `gui` feature it has the native window (Tauri), and with `--no-default-features` it opens apps in your browser instead, which is the build for servers, Docker and the tests. Both have every command in [docs/CLI.md](docs/CLI.md).

The quick way is [`just`](https://github.com/casey/just):

```sh
just                  # list recipes
just build            # release build with the window (target/release/uapp)
just build-server     # release build without the window
just install          # just build, then install it as ~/.local/bin/uapp
just web              # the browser build, into dist-web/
just serve site.uapp  # serve a .uapp as a public site locally, with the editor (run `just web` first)
just dev              # the window with live reload (cargo tauri dev)
```

Testing and linting are in [CONTRIBUTING.md](CONTRIBUTING.md).

## Prerequisites

- Rust 1.88 or newer
- A C compiler, Perl and make (SQLCipher and OpenSSL are compiled from source). On Windows you also need [NASM](https://www.nasm.us/) on your `PATH`.
- For the window build on Linux: `libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev libxdo-dev libssl-dev` (on Debian and Ubuntu). Other platforms are covered by [Tauri's prerequisites](https://tauri.app/start/prerequisites/).
- For bundles and mobile builds: the Tauri CLI, `cargo install tauri-cli --version '^2' --locked`

## Desktop

```sh
cargo build --release    # target/release/uapp
cargo tauri build        # the same, plus installable bundles (.deb, .AppImage, .app, .dmg)
```

To install your build as `~/.local/bin/uapp` (the previous one is kept as `uapp.bak`):

```sh
scripts/install-local.sh            # cargo build --release, then install
scripts/install-local.sh --no-build # install what's already built
```

Then `uapp install` registers `.uapp` files with your Linux desktop.

## Android and iOS

They use the same Tauri build.

- Android: Android Studio with the SDK and NDK, and JDK 17 to 21. CI uses NDK 27.1.12297006 and symlinks the NDK's `llvm-*` tools to the old binutils names that the OpenSSL build expects (see the `android` job in `.github/workflows/ci.yml`).
- iOS: macOS and Xcode (untested).

```sh
cargo tauri android build
cargo tauri ios build
```

## Browser build (WASM)

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.127 --locked
scripts/build-web.sh
```

You also need clang (SQLite is compiled to wasm). The script stops if your wasm-bindgen CLI doesn't match the version in `Cargo.lock`, runs `wasm-opt -Oz` when binaryen is installed, and builds the demo launcher with the `make-demo-apps` example. The bundle's files are listed in `web/uapp-bundle.js` (the service worker reads the same list), so a new file goes there.

The browser build can't open encrypted files and doesn't take snapshots.

## Server

```sh
cargo build --release --no-default-features
```

This build has no Tauri or webview dependencies. `Dockerfile.serve` builds it into an image for `uapp serve` and `uapp library`, and its comments show how to run each.
