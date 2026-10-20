# uApp build recipes — `just --list` shows them. The shell scripts under
# scripts/ stay the single source of truth for the multi-step jobs; this file
# is the front door.

set shell := ["bash", "-euo", "pipefail", "-c"]

# where `just install` puts the binary
install_dir := env("UAPP_INSTALL_DIR", env("HOME") + "/.local/bin")

default:
    @just --list

# release build of `uapp` with the native window
build:
    cargo build --release

# release build of `uapp` without the window (what servers and Docker use)
build-server:
    cargo build --release --no-default-features

# Rust tests, then the node end-to-end suites (against a build without the window)
test:
    cargo test --no-default-features
    scripts/test-e2e.sh

# Rust tests only
test-rust:
    cargo test --no-default-features

# formatting check and clippy, for both builds
lint:
    cargo fmt --check
    cargo clippy --all-targets --no-default-features -- -D warnings
    cargo clippy --all-targets -- -D warnings

# the browser build (wasm engine + shell + site chrome) into dist-web/
web:
    scripts/build-web.sh

# build + install ~/.local/bin/uapp (the old one kept as .bak)
install: build
    UAPP_INSTALL_DIR="{{install_dir}}" scripts/install-local.sh --no-build

# native window with live reload
dev:
    cargo tauri dev

# open a .uapp headless and print its URL as JSON
open file:
    cargo run --release --no-default-features -- open "{{file}}" --headless

# serve a .uapp as a public website locally, with the editing chrome (run `just web` first)
serve file port="8080":
    @test -f dist-web/index.html || { echo "dist-web/ is missing or incomplete; run 'just web' first" >&2; exit 1; }
    cargo run --release --no-default-features -- serve "{{file}}" --port {{port}} --coi --publish-data --chrome dist-web

# remove build output
clean:
    cargo clean
