#!/usr/bin/env bash
# run.sh — start the usbkeeper daemon for real USB sticks (root).
# Usage: ./run.sh [extra usbkeeper args]   e.g. ./run.sh --port 9000
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/release/usbkeeper"
DEST="${USBK_DEST:-/var/lib/usb-archive}"
PORT="${USBK_PORT:-8488}"

# --- preflight ---------------------------------------------------------------
sudo -v   # fail early with a friendly prompt instead of mid-pipeline errors

if [ ! -x "$BIN" ]; then
  echo "usbkeeper binary missing; building release..."
  (cd "$HERE" && cargo build --release)
fi

for t in rsync blkid wipefs mount umount du; do
  command -v "$t" >/dev/null || { echo "PREFLIGHT FAIL: required tool '$t' not found"; exit 1; }
done
if ! command -v mkfs.exfat >/dev/null; then
  echo "PREFLIGHT FAIL: mkfs.exfat missing — install with: sudo pacman -S exfatprogs"
  exit 1
fi
command -v ntfs-3g >/dev/null || echo "note: ntfs-3g absent — NTFS sticks will be refused (state error), everything else works"

sudo mkdir -p "$DEST" /run/usbkeeper
sudo chmod 755 "$DEST"

echo "starting usbkeeper: dest=$DEST port=$PORT (real USB sticks only; no allow-loop)"
exec sudo -E env PATH="$PATH" "$BIN" --dest "$DEST" --port "$PORT" --ui-root "$HERE/web" "$@"
