#!/usr/bin/env bash
#
# Remove what install.sh installed. Never touches a vault: the notes are
# ordinary files in a folder you chose, and they are not Familiar's to delete.
#
set -euo pipefail

APP_ID="us.hagreli.Familiar"
PREFIX="${PREFIX:-$HOME/.local}"
DATA_DIR="$PREFIX/share"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

say "Removing Familiar from $PREFIX"
rm -f "$PREFIX/bin/familiar"
rm -f "$DATA_DIR/applications/$APP_ID.desktop"
rm -f "$DATA_DIR/metainfo/$APP_ID.metainfo.xml"
rm -f "$DATA_DIR/icons/hicolor/scalable/apps/$APP_ID.svg"
rm -f "$DATA_DIR/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg"
rm -f "$DATA_DIR/dbus-1/services/$APP_ID.service"

if command -v update-desktop-database >/dev/null; then
  update-desktop-database -q "$DATA_DIR/applications" 2>/dev/null || true
fi

echo
say "Done. Your notes and ~/.config/familiar/config.json were left alone."
say "Remove the config with: rm -r ~/.config/familiar"
