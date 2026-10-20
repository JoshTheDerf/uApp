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

# The build id: a hash of everything above. It is stamped into the files
# that talk to each other (page, workers, glue), the runtime URLs and
# boot.js's own module imports (?v=), so a page from one deploy never runs
# with another deploy's wasm or glue, and a mismatch is detected and shown
# instead (see BUILD in uapp-boot/env.js).
build=$(cd dist-web && find . -type f ! -path './examples/*' -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-12)
grep -rlZ --include='*.js' __UAPP_BUILD__ dist-web | xargs -0 -r sed -i "s/__UAPP_BUILD__/$build/g"
# The generated glue imports ours by a bare path; pin it to this build too
# (the import statements only: the same string is also the wasm's import
# module name, which has to stay as it is), and say which build it is.
sed -i "/^import /s#\(['\"]\)\./uapp_glue\.js\1#\1./uapp_glue.js?v=$build\1#g" dist-web/uapp_wasm.js
echo "export const WASM_BUILD = \"$build\";" >> dist-web/uapp_wasm.js
printf '{"build":"%s"}\n' "$build" > dist-web/uapp-build.json

# Every entry of the list must be in the bundle, generated ones included.
missing=0
while read -r dest; do
  [[ -e "dist-web/$dest" ]] || { echo "error: $manifest lists $dest, but the build did not produce it" >&2; missing=1; }
done < <(sed -n 's/^  "\([^"]*\)": .*,$/\1/p' "$manifest")
[[ $missing == 0 ]] || exit 1

echo "dist-web/ ready (build $build):"
du -sh dist-web
ls dist-web
