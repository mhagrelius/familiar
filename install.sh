#!/usr/bin/env bash
#
# Install Familiar into the user's home directory. No root, no system paths —
# everything lands under ~/.local.
#
#   ./install.sh
#   PREFIX=/usr/local sudo ./install.sh
#
set -euo pipefail

APP_ID="us.hagreli.Familiar"

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
DATA_DIR="$PREFIX/share"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$*" >&2; }

say "Building (release)"
cargo build --release --locked

say "Installing to $PREFIX"
install -Dm755 target/release/familiar "$BIN_DIR/familiar"
install -Dm644 "data/$APP_ID.desktop" "$DATA_DIR/applications/$APP_ID.desktop"
install -Dm644 "data/$APP_ID.metainfo.xml" "$DATA_DIR/metainfo/$APP_ID.metainfo.xml"
install -Dm644 "data/icons/hicolor/scalable/apps/$APP_ID.svg" \
  "$DATA_DIR/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 "data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" \
  "$DATA_DIR/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"

# The desktop file declares DBusActivatable, so GNOME needs a matching D-Bus
# service file to launch the app on demand — from the dock's context menu, for
# instance.
install -Dm644 /dev/stdin "$DATA_DIR/dbus-1/services/$APP_ID.service" <<EOF
[D-BUS Service]
Name=$APP_ID
Exec=$BIN_DIR/familiar --gapplication-service
EOF

if command -v gtk4-update-icon-cache >/dev/null; then
  gtk4-update-icon-cache -qtf "$DATA_DIR/icons/hicolor" 2>/dev/null || true
elif command -v gtk-update-icon-cache >/dev/null; then
  gtk-update-icon-cache -qtf "$DATA_DIR/icons/hicolor" 2>/dev/null || true
fi
if command -v update-desktop-database >/dev/null; then
  update-desktop-database -q "$DATA_DIR/applications" 2>/dev/null || true
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH; add it to run 'familiar' from a terminal" ;;
esac

echo
say "Installed. Familiar needs a llama-server to talk to; it looks for one at"
say "http://127.0.0.1:8080 unless Preferences says otherwise."
say "Its memory is your notes — the same folder Brain uses — and it writes only"
say "under a heading it marks as its own."
