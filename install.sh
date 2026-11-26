#!/bin/sh
# Builds Aqua Fetch and installs it for the current user (no sudo).
set -eu
cd "$(dirname "$0")"
"${CARGO:-$HOME/.cargo/bin/cargo}" build --release
install -Dm755 target/release/aqua-fetch "$HOME/.local/bin/aqua-fetch"
install -Dm644 assets/icon.png "$HOME/.local/share/icons/hicolor/256x256/apps/aqua-fetch.png"
# Absolute Exec path, so the menu entry works even when ~/.local/bin is not on PATH.
sed "s|^Exec=.*|Exec=$HOME/.local/bin/aqua-fetch|" aqua-fetch.desktop > aqua-fetch.desktop.tmp
install -Dm644 aqua-fetch.desktop.tmp "$HOME/.local/share/applications/aqua-fetch.desktop"
rm aqua-fetch.desktop.tmp
echo "Installed. Start it from the menu (Internet > Aqua Fetch) or run: $HOME/.local/bin/aqua-fetch"
