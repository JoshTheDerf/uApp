#!/usr/bin/env bash
# Build the fully client-side browser demo into dist-web/.
# Everything (SQLite, the app engine, the AI tool loop) runs in the visitor's
# tab: a wasm worker + service worker; no server code at all. The output is a
# static directory — host it anywhere (a subpath like /uapp/demo/ works).
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --lib --release --target wasm32-unknown-unknown --no-default-features
wasm-bindgen --target web --out-dir dist-web --out-name uapp_wasm \
  target/wasm32-unknown-unknown/release/uapp.wasm

mkdir -p dist-web/shell/lang
cp src/shell/js/*.js dist-web/shell/
cp src/shell/js/lang/*.js dist-web/shell/lang/
cp src/shell/shell.css dist-web/shell.css
cp src/shell/uapp.js src/shell/icons.js src/shell/scratch.html dist-web/
cp assets/uapp-256.png dist-web/icon-256.png 2>/dev/null || true
cp web/boot.js web/worker.js web/sw.js web/uapp_glue.js web/site-chrome.js dist-web/
# Demo .uapp files are generated fresh — never ship a real local .uapp (they
# can carry API keys and user data). Samples land in examples/ (committed)
# and are served from dist-web/examples/; the launcher's own SQLite carries
# the sample catalog.
cargo run --bin make-demo-apps --release -- dist-web
mkdir -p dist-web/examples
cp examples/*.uapp dist-web/examples/
rm -f dist-web/sample.uapp dist-web/sample-expenses.uapp dist-web/samples.json

# index.html: use the web-specific version (already has relative paths and boot.js)
cp web/index.html dist-web/index.html

echo "dist-web/ ready:"
du -sh dist-web
ls dist-web