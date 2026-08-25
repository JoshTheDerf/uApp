#!/usr/bin/env bash
# Build the desktop (Tauri) window + the server/CLI binary in release mode and
# install them for the current user:
#   ~/.local/bin/uapp          the CLI / server  (target/release/uapp-server)
#   ~/.local/bin/uapp-desktop  the native window (target/release/uapp)
# `uapp <file>` finds the window as a sibling binary (see delegate_to_desktop
# in src/main.rs), so both must live in the same directory. Existing binaries
# are kept as *.bak. Pass --no-build to only (re)install what is already built.
set -euo pipefail
cd "$(dirname "$0")/.."

DEST="${UAPP_INSTALL_DIR:-$HOME/.local/bin}"
if [[ "${1:-}" != "--no-build" ]]; then
  cargo build --release            # gui feature is on by default -> both bins
fi

mkdir -p "$DEST"
install_bin() {
  local src="$1" name="$2"
  if [[ -e "$DEST/$name" ]]; then cp -p "$DEST/$name" "$DEST/$name.bak"; fi
  # copy to a temp name and rename: a running instance keeps its old inode
  cp "$src" "$DEST/.$name.tmp" && mv -f "$DEST/.$name.tmp" "$DEST/$name"
  echo "installed $DEST/$name  ($(du -h "$src" | cut -f1))"
}
install_bin target/release/uapp-server uapp
install_bin target/release/uapp        uapp-desktop

case ":$PATH:" in *":$DEST:"*) ;; *) echo "note: $DEST is not on your PATH";; esac
echo "Running instances keep the old binary until restarted (uapp windows, uapp-desktop)."
