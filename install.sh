#!/bin/bash
# Install the binary, launcher entry and icon for this user (~/.local).
# Usage: ./install.sh [BINARY]   (default: target/release/omacctv, else dist/omacctv)
set -euo pipefail
cd "$(dirname "$0")"
bin=${1:-}
if [ -z "$bin" ]; then
  for b in target/release/omacctv dist/omacctv; do [ -x "$b" ] && { bin=$b; break; }; done
fi
[ -n "$bin" ] && [ -x "$bin" ] || { echo "no binary: run cargo build --release first" >&2; exit 1; }
install -Dm755 "$bin" ~/.local/bin/omacctv
install -Dm644 data/lu.kloot.OmaCCTV.desktop ~/.local/share/applications/lu.kloot.OmaCCTV.desktop
install -Dm644 data/lu.kloot.OmaCCTV.svg ~/.local/share/icons/hicolor/scalable/apps/lu.kloot.OmaCCTV.svg
sed -i "s|^Exec=.*|Exec=$HOME/.local/bin/omacctv|" ~/.local/share/applications/lu.kloot.OmaCCTV.desktop
update-desktop-database ~/.local/share/applications 2>/dev/null || true
gtk4-update-icon-cache -q -t -f ~/.local/share/icons/hicolor 2>/dev/null || true
command -v ffmpeg >/dev/null || echo "note: ffmpeg is not installed; OmaCCTV needs it (and ffprobe for omacctv add)"
echo "installed ~/.local/bin/omacctv; set it up with: omacctv scan, omacctv discover --write, omacctv check"
