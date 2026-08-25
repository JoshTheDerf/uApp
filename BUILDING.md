# Building uApp

The quick way is [`just`](https://github.com/casey/just) (`justfile` at the repo root):

```sh
just            # list recipes
just build      # release build of both binaries
just test       # server debug build + Rust tests + node end-to-end tests
just web        # browser build -> dist-web/
just install    # build + install ~/.local/bin/uapp and uapp-desktop
just serve ../thederf-com/site.uapp   # public-site mode locally, with editing chrome
just deploy-site / just deploy-content <file.uapp>   # thederf.com (see deploy doc)
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

To build and install both binaries for your user (`~/.local/bin/uapp` = CLI/server,
`~/.local/bin/uapp-desktop` = native window; previous ones kept as `.bak`):

```sh
scripts/install-local.sh            # cargo build --release, then install
scripts/install-local.sh --no-build # install what is already built
```

### Run tests

```sh
cargo build --bin uapp-server --no-default-features
for t in tests/test_*.mjs; do
  UAPP_BIN="$PWD/target/debug/uapp-server" node "$t" || exit 1
done
```

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

## Developer notes

A `uapp-server` binary exists for server/CLI usage (future work). It is not part of the distributed builds and can be built explicitly with:

```sh
cargo build --bin uapp-server --no-default-features
```

This binary is not documented for end users and may change without notice.
