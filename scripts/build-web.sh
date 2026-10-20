#!/usr/bin/env bash
# Build the fully client-side browser demo into dist-web/.
# Everything (SQLite, the app engine, the AI tool loop) runs in the visitor's
# tab: a wasm worker + service worker; no server code at all. The output is a
# static directory — host it anywhere (a subpath like /uapp/demo/ works).
#
# Needs: the wasm32-unknown-unknown target, clang (sqlite-wasm-rs compiles
# SQLite), and wasm-bindgen-cli at the version Cargo.lock pins. wasm-opt
# (binaryen) is used when installed.
set -euo pipefail
cd "$(dirname "$0")/.."

# The CLI and the crate must agree exactly: a mismatch fails at runtime in
# the browser with an unhelpful import error, not here.
want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
if ! command -v wasm-bindgen >/dev/null; then
  echo "error: wasm-bindgen not found. Install it with: cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi
have=$(wasm-bindgen --version | awk '{ print $2 }')
if [[ "$have" != "$want" ]]; then
  echo "error: wasm-bindgen CLI is $have but Cargo.lock pins the crate at $want." >&2
  echo "       Install the matching CLI: cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi

# Start clean: a renamed or deleted source must not live on in the bundle
# (load_chrome serves the whole directory and hashes it into the version).
rm -rf dist-web

cargo build --locked --lib --release --target wasm32-unknown-unknown --no-default-features
wasm-bindgen --target web --out-dir dist-web --out-name uapp_wasm \
  target/wasm32-unknown-unknown/release/uapp.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -Oz dist-web/uapp_wasm_bg.wasm -o dist-web/uapp_wasm_bg.opt.wasm \
    && mv dist-web/uapp_wasm_bg.opt.wasm dist-web/uapp_wasm_bg.wasm \
    || { rm -f dist-web/uapp_wasm_bg.opt.wasm; echo "note: wasm-opt failed, keeping the unoptimized wasm"; }
else
  echo "note: wasm-opt not found, skipping it (install binaryen for a smaller wasm)"
fi

# The bundle's own files, from the one list sw.js reads too.
manifest=web/uapp-bundle.js
while IFS=$'\t' read -r dest src; do
  if [[ "$dest" == */ ]]; then
    mkdir -p "dist-web/$dest"
    cp -r "$src." "dist-web/$dest"
  else
    mkdir -p "$(dirname "dist-web/$dest")"
    cp "$src" "dist-web/$dest"
  fi
done < <(sed -n 's/^  "\([^"]*\)": "\([^"]*\)",$/\1\t\2/p' "$manifest")

# Demo .uapp files are generated fresh — never ship a real local .uapp (they
# can carry API keys and user data). Samples land in examples/ (committed)
# and are served from dist-web/examples/; the launcher's own SQLite carries
# the sample catalog.
cargo run --locked --bin make-demo-apps --release -- dist-web
mkdir -p dist-web/examples
cp examples/*.uapp dist-web/examples/
rm -f dist-web/sample.uapp dist-web/sample-expenses.uapp dist-web/samples.json

# Every entry of the list must be in the bundle, generated ones included.
missing=0
while read -r dest; do
  [[ -e "dist-web/$dest" ]] || { echo "error: $manifest lists $dest, but the build did not produce it" >&2; missing=1; }
done < <(sed -n 's/^  "\([^"]*\)": .*,$/\1/p' "$manifest")
[[ $missing == 0 ]] || exit 1

echo "dist-web/ ready:"
du -sh dist-web
ls dist-web
