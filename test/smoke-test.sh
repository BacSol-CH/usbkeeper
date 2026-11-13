#!/usr/bin/env bash
# smoke-test.sh — fixture harness self-test WITHOUT the usbkeeper daemon.
#
# make-stick x2 -> simulate the copy ourselves (rsync into a temp destroot) ->
# check-stick against that destroot -> negative control (corrupt one file, must FAIL)
# -> teardown -> verify clean state. Exit nonzero on any unexpected result.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"
log() { printf '[smoke] %s\n' "$*"; }
rc=0

"$HERE/ensure-loop.sh" || { log "FATAL: loop not available"; exit 1; }
$SUDO rm -rf "$HERE/sticks" "$HERE/smoke-dest"   # start from scratch

log "== make stick-a (vfat 64M) =="
"$HERE/make-stick.sh" vfat 64 stick-a || { log "make-stick stick-a failed"; exit 1; }
log "== make stick-b (ext4 80M) =="
"$HERE/make-stick.sh" ext4 80 stick-b || { log "make-stick stick-b failed"; $SUDO "$HERE/teardown.sh"; exit 1; }

MNT_A=$(awk '$1=="stick-a"{print $4}' "$HERE/sticks/registry.txt")
MNT_B=$(awk '$1=="stick-b"{print $4}' "$HERE/sticks/registry.txt")

log "== simulated archive copy (rsync as stand-in for daemon) =="
$SUDO mkdir -p "$HERE/smoke-dest/stick-a-copy" "$HERE/smoke-dest/stick-b-copy"
$SUDO rsync -a --delete "$MNT_A/" "$HERE/smoke-dest/stick-a-copy/" || rc=1
$SUDO rsync -a --delete "$MNT_B/" "$HERE/smoke-dest/stick-b-copy/" || rc=1
$SUDO chmod -R a+rX "$HERE/smoke-dest"

log "== positive checks =="
"$HERE/check-stick.sh" stick-a "$HERE/smoke-dest" || { log "unexpected FAIL stick-a"; rc=1; }
"$HERE/check-stick.sh" stick-b "$HERE/smoke-dest" || { log "unexpected FAIL stick-b"; rc=1; }

log "== negative control: corrupt one copied file, expect FAIL =="
victim=$($SUDO find "$HERE/smoke-dest/stick-a-copy" -type f ! -name SHA256MANIFEST | head -1)
$SUDO sh -c "echo GARBAGE > '$victim'"
$SUDO chmod a+w "$victim" 2>/dev/null || true
if "$HERE/check-stick.sh" stick-a "$HERE/smoke-dest"; then
  log "NEGATIVE CONTROL BROKE: check passed despite corruption"; rc=1
else
  log "negative control OK (check correctly FAILED)"
fi

log "== teardown =="
"$HERE/teardown.sh" || { log "teardown errors"; rc=1; }

log "== post-teardown invariants =="
$SUDO test -z "$($SUDO losetup -j "$HERE/sticks" 2>/dev/null)" || { log "stale loops on our images"; rc=1; }
$SUDO test ! -e "$HERE/sticks/stick-a.img" || { log "stick-a.img survived"; rc=1; }
$SUDO test ! -e /run/usbkeeper/allow-loop || { log "marker file survived"; rc=1; }
[ -s "$HERE/sticks/registry.txt" ] && { log "registry not cleared"; rc=1; }
log "== idempotent teardown re-run =="
"$HERE/teardown.sh" || { log "second teardown should be a no-op success"; rc=1; }

$SUDO rm -rf "$HERE/smoke-dest"
if [ "$rc" = 0 ]; then echo "SMOKE PASS"; else echo "SMOKE FAIL"; fi
exit "$rc"
