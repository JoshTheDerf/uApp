# Building uApp

The quick way is [`just`](https://github.com/casey/just) (`justfile` at the repo root):

```sh
just            # list recipes
just build      # release build of uapp (with the native window)
just test       # Rust tests + node end-to-end tests (no-window build)
just web        # browser build -> dist-web/
just lint       # cargo fmt --check + clippy for both builds
just install    # build + install ~/.local/bin/uapp
just serve path/to/site.uapp          # public-site mode locally, with editing chrome
```

The sections below describe what those recipes run.

## Desktop app

The distributed uApp binary is a native desktop application built with Tauri.

### Prerequisites

- Rust (stable toolchain)
- A C compiler (gcc/clang)
- Node.js ≥ 22 (for tests)
- Tauri CLI: `cargo install tauri-cli --version '^2'`
- Desktop deps: https://tauri.app/start/prerequisites/

### Build

```sh
cargo tauri dev              # dev build
cargo tauri build            # release build
```

Linux needs `webkit2gtk` / `libsoup`.

`uapp` is one binary: the native window plus every CLI subcommand (see
[docs/CLI.md](docs/CLI.md)). To build and install it for your user as
`~/.local/bin/uapp` (the previous one kept as `uapp.bak`):

```sh
scripts/install-local.sh            # cargo build --release, then install
scripts/install-local.sh --no-build # install what is already built
```

### Run tests

```sh
cargo test --no-default-features   # Rust unit + integration tests
scripts/test-e2e.sh                # node suites against target/debug/uapp
```

`scripts/test-e2e.sh` builds `uapp` without the window first (a gui
`cargo build` writes the same `target/debug/uapp`). Set `UAPP_BIN` to test
another binary, or name suites to run only those (`scripts/test-e2e.sh ai`).

## Mobile apps

iOS and Android use the same Tauri build with `--features gui` (enabled by default).

### Prerequisites

- iOS: macOS + Xcode
- Android: Android Studio + SDK + NDK, JDK 17–21

### Build

```sh
cargo tauri android build
cargo tauri ios build
```

## Browser demo (WASM)

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.127
./scripts/build-web.sh
```

Limitations: no encryption, no crash snapshots.

## Server build

Built with `--no-default-features`, `uapp` has no Tauri or webview
dependencies and opens apps in the system browser. That's the build for
servers, Docker (`Dockerfile.serve`) and the tests:

```sh
cargo build --release --no-default-features
```

The demo launcher for the browser build comes from a cargo example:

```sh
cargo run --release --no-default-features --example make-demo-apps -- dist-web
```
