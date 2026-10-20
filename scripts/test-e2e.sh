#!/usr/bin/env bash
# Run the node end-to-end suites (tests/test_*.mjs) against `uapp` built
# without the gui feature (the suites only use --headless). Each suite gets its
# own scratch dir and its own HOME, so suites can run in parallel and never
# touch your real ~/.config/uapp or ~/.local/share/uapp.
#
#   scripts/test-e2e.sh                 # all suites, 4 at a time
#   scripts/test-e2e.sh ai approval     # just tests/test_ai.mjs and test_approval.mjs
#   JOBS=1 scripts/test-e2e.sh          # one at a time
#
# Set UAPP_BIN to test a binary other than target/debug/uapp. Unless UAPP_BIN
# is set, the script rebuilds that one without default features first: a gui
# `cargo build` writes the same path.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -z "${UAPP_BIN:-}" ]]; then
  cargo build --quiet --bin uapp --no-default-features
  UAPP_BIN="$PWD/target/debug/uapp"
fi
export UAPP_BIN

if [[ $# -gt 0 ]]; then
  suites=()
  for name in "$@"; do suites+=("tests/test_${name#test_}.mjs"); done
else
  suites=(tests/test_*.mjs)
fi

root="$(mktemp -d "${TMPDIR:-/tmp}/uapp-e2e.XXXXXX")"
trap 'rm -rf "$root"' EXIT

run_one() {
  local t="$1" name dir
  name="$(basename "$t" .mjs)"
  dir="$root/$name"
  mkdir -p "$dir/home"
  if (cd "$PWD" && env -u XDG_CONFIG_HOME -u XDG_DATA_HOME -u XDG_STATE_HOME \
        HOME="$dir/home" TESTDIR="$dir/work" UAPP_BIN="$UAPP_BIN" \
        timeout 600 node "$t") >"$dir/log" 2>&1; then
    echo "PASS $name"
  else
    echo "FAIL $name (exit $?)"
    sed 's/^/    /' "$dir/log" | tail -40
    return 1
  fi
}
export -f run_one
export root

printf '%s\n' "${suites[@]}" | xargs -P "${JOBS:-4}" -I{} bash -c 'run_one "$@"' _ {}
