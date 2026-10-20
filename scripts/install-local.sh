#!/usr/bin/env bash
# Build `uapp` (native window + CLI in one binary) in release mode and install
# it for the current user as ~/.local/bin/uapp. The existing one is kept as
# uapp.bak. A `uapp-desktop` left by older versions of this script is removed.
# Pass --no-build to only (re)install what is already built.
set -euo pipefail
cd "$(dirname "$0")/.."

DEST="${UAPP_INSTALL_DIR:-$HOME/.local/bin}"
if [[ "${1:-}" != "--no-build" ]]; then
  cargo build --release            # gui feature is on by default
fi

mkdir -p "$DEST"
src=target/release/uapp
if [[ -e "$DEST/uapp" ]]; then cp -p "$DEST/uapp" "$DEST/uapp.bak"; fi
# copy to a temp name and rename: a running instance keeps its old inode
cp "$src" "$DEST/.uapp.tmp" && mv -f "$DEST/.uapp.tmp" "$DEST/uapp"
echo "installed $DEST/uapp  ($(du -h "$src" | cut -f1))"

for old in uapp-desktop uapp-desktop.bak; do
  if [[ -e "$DEST/$old" ]]; then rm -f "$DEST/$old" && echo "removed $DEST/$old (no longer used)"; fi
done
# Launchers made by `uapp install` or the window's Install button point at the
# binary that made them; move any that pointed at uapp-desktop over.
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
for f in "$apps"/UApp.desktop "$apps"/uapp-*.desktop; do
  if [[ -f "$f" ]] && grep -q "$DEST/uapp-desktop" "$f"; then
    sed -i "s#$DEST/uapp-desktop#$DEST/uapp#g" "$f" && echo "updated $f"
  fi
done

case ":$PATH:" in *":$DEST:"*) ;; *) echo "note: $DEST is not on your PATH";; esac
echo "Running uapp windows keep the old binary until they're restarted."
