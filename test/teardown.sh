#!/usr/bin/env bash
# teardown.sh — destroy all usbkeeper test fixtures recorded in sticks/registry.txt.
#
# Idempotent. SAFETY GUARD: only loop devices that are (a) listed in our registry AND
# (b) confirmed by `losetup -j <our image path>` are ever detached; only images under
# test/sticks/ and mounts under test/sticks/mnt/ are removed. Real USB devices or
# foreign loops are never touched.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STICKS="$HERE/sticks"
REG="$STICKS/registry.txt"
MARKER="/run/usbkeeper/allow-loop"
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"

log() { printf '[teardown] %s\n' "$*"; }

if [ ! -f "$REG" ]; then
  log "no registry at $REG — nothing to do"
  exit 0
fi

rc=0
tmp=$(mktemp); : > "$tmp"
while IFS=$'\t' read -r NAME LOOP IMG MNT FS; do
  [ -n "${NAME:-}" ] || continue
  # double-check the loop really backs OUR image before detaching (guard against
  # a recycled /dev/loopN now pointing at someone else's file)
  if [ -n "$LOOP" ] && ! $SUDO losetup -j "$IMG" 2>/dev/null | grep -q "^$LOOP:"; then
    log "$NAME: $LOOP no longer backs $IMG — skipping detach (foreign/recycled)"
  else
    if [ -n "$MNT" ] && $SUDO mountpoint -q "$MNT" 2>/dev/null; then
      $SUDO umount "$MNT" && log "$NAME: unmounted $MNT" || { log "$NAME: UOMOUNT FAILED ($MNT, busy?)"; rc=1; }
    fi
    if [ -n "$LOOP" ] && $SUDO losetup -l "$LOOP" >/dev/null 2>&1; then
      $SUDO losetup -d "$LOOP" && log "$NAME: detached $LOOP" || { log "$NAME: DETACH FAILED $LOOP"; rc=1; }
    fi
  fi
  if [ -n "$IMG" ] && [[ "$IMG" == "$STICKS/"* ]] && $SUDO test -f "$IMG"; then
    $SUDO rm -f "$IMG" && log "$NAME: removed $IMG" || { log "$NAME: RM FAILED $IMG"; rc=1; }
  fi
  [ -n "$MNT" ] && [[ "$MNT" == "$STICKS/mnt/"* ]] && $SUDO rmdir "$MNT" 2>/dev/null
done < "$REG"
rm -f "$tmp"

if [ "$rc" = 0 ]; then
  : > "$REG"
  log "registry cleared"
else
  log "errors occurred — registry kept for retry"
fi

# remove daemon test-mode marker (only ours, exact path)
if $SUDO test -f "$MARKER"; then
  $SUDO rm -f "$MARKER" && log "removed marker $MARKER"
fi
exit "$rc"
