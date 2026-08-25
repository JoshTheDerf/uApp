# uApp build recipes — `just --list` shows them. The shell scripts under
# scripts/ stay the single source of truth for the multi-step jobs; this file
# is the front door.

set shell := ["bash", "-euo", "pipefail", "-c"]

# where `just install` puts the binaries
install_dir := env("UAPP_INSTALL_DIR", env("HOME") + "/.local/bin")
# public-site deployment target (see ../thederf-com/deploy/uapp-serve.md)
deploy_host := env("UAPP_DEPLOY_HOST", "home")
deploy_dir  := env("UAPP_DEPLOY_DIR", "/server/thederf/uApp/")
compose_dir := env("UAPP_COMPOSE_DIR", "/server/thederf/compose")

default:
    @just --list

# release build of both binaries (native window `uapp` + CLI/server `uapp-server`)
build:
    cargo build --release

# CLI/server only, no Tauri/webview deps (what CI, Docker and the tests use)
build-server:
    cargo build --release --bin uapp-server --no-default-features

# debug build of the server + the whole test suite (Rust + node end-to-end)
test: 
    cargo build --bin uapp-server --no-default-features
    cargo test
    for t in tests/test_*.mjs; do UAPP_BIN="$PWD/target/debug/uapp-server" node "$t" || exit 1; done

# Rust tests only
test-rust:
    cargo test

# the browser build (wasm engine + shell + site chrome) into dist-web/
web:
    scripts/build-web.sh

# build + install ~/.local/bin/uapp and ~/.local/bin/uapp-desktop (old ones kept as .bak)
install: build
    UAPP_INSTALL_DIR="{{install_dir}}" scripts/install-local.sh --no-build

# native window with live reload
dev:
    cargo tauri dev

# open a .uapp headless and print its URL as JSON
open file:
    cargo run --release --bin uapp-server --no-default-features -- open "{{file}}" --headless

# serve a .uapp as a public website locally, with the editing chrome (needs `just web` once)
serve file port="8080":
    cargo run --release --bin uapp-server --no-default-features -- serve "{{file}}" --port {{port}} --coi --publish-data --chrome dist-web

# rsync this repo to the deploy host and rebuild the uapp-site container
deploy-site:
    rsync -a --delete --exclude target --exclude .git --exclude node_modules "{{justfile_directory()}}/" "{{deploy_host}}:{{deploy_dir}}"
    ssh "{{deploy_host}}" 'cd {{compose_dir}} && timeout 1500 docker compose up -d --build uapp-site 2>&1 | grep -v level=warning | tail -2'

# push a new site.uapp to the deploy host and restart the container (it holds the old inode otherwise)
deploy-content file:
    rsync -a "{{file}}" "{{deploy_host}}:/server/thederf/thederf-com/site.uapp"
    ssh "{{deploy_host}}" 'cd {{compose_dir}} && docker compose restart uapp-site >/dev/null 2>&1 && echo restarted'

# remove build output
clean:
    cargo clean
