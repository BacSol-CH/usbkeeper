#!/usr/bin/env bash
# run-e2e.sh — full usbkeeper end-to-end: fixtures -> daemon archive -> verify -> teardown.
#
# Flow:
#   1. ensure loop driver (test/ensure-loop.sh)
#   2. make 3 sticks: vfat 300M, ext4 500M, vfat 250M, then HANDOFF: umount the fixture
#      mounts (loop devices stay attached) — the daemon mounts /dev/loopN itself under
#      /run/usbkeeper/loopN/fs and errors with "already mounted" if we keep ours.
#   3. touch /run/usbkeeper/allow-loop (daemon test-mode marker)
#   4. start the daemon binary if not already running: --dest test/archive --port 8489
#   5. poll GET /api/state until OUR sticks reach done|error (timeout 600s), log timeline
#      (API schema: {"ports":[{"port":"loopN","device":{"dev":..,"state":..,"dest":..,"error":..}}]})
#   6. check-stick.sh per stick against the archive destroot (dest hint from API if set)
#   7. assert each underlying loop image was reformatted exfat afterwards (blkid TYPE=exfat)
#   8. teardown.sh; kill daemon if we started it
# Exit nonzero on any failure with clear log lines.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"
BIN="$ROOT/target/release/usbkeeper"
ARCHIVE="$HERE/archive"
PORT="${USBK_PORT:-8489}"
MARKER="/run/usbkeeper/allow-loop"
TIMEOUT="${USBK_E2E_TIMEOUT:-600}"
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"

log() { printf '[e2e %s] %s\n' "$(date +%H:%M:%S)" "$*"; }
fail() { log "FAIL: $*"; FAILED=1; }
FAILED=0
DAEMON_PID=""

cleanup() {
  if [ -n "$DAEMON_PID" ]; then
    log "stopping daemon pid $DAEMON_PID"
    $SUDO kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# --- 0. prereqs ---------------------------------------------------------------
"$HERE/ensure-loop.sh" || fail "loop driver unavailable"
if [ ! -x "$BIN" ]; then
  log "FATAL: daemon binary not built yet: $BIN"
  log "re-run ./run-e2e.sh after building: cargo build --release (target/release/usbkeeper)"
  exit 3
fi
[ "$FAILED" = 0 ] || { log "prereq failures — aborting"; exit 1; }

# --- 1. fixtures ---------------------------------------------------------------
$SUDO rm -rf "$ARCHIVE"; $SUDO mkdir -p "$ARCHIVE"
$SUDO chmod 0777 "$ARCHIVE"   # daemon (root) writes; checker (user) reads
for spec in "vfat 300 stick-vfat-300" "ext4 500 stick-ext4-500" "vfat 250 stick-vfat-250"; do
  # shellcheck disable=SC2086
  "$HERE/make-stick.sh" $spec || fail "make-stick failed for: $spec"
done
[ "$FAILED" = 0 ] || { log "fixture creation failed — teardown then abort"; "$HERE/teardown.sh"; exit 1; }

# handoff: the daemon mounts the raw loop itself — drop our fixture mounts
while IFS=$'\t' read -r NAME LOOP IMG MNT FS; do
  [ -n "${NAME:-}" ] || continue
  if $SUDO mountpoint -q "$MNT"; then
    $SUDO umount "$MNT" && log "handoff: unmounted $MNT (daemon will mount $LOOP)" \
      || fail "could not unmount fixture $MNT for $NAME"
  fi
done < "$HERE/sticks/registry.txt"

OUR_DEVS=$(cut -f2 "$HERE/sticks/registry.txt" | sed 's|/dev/||' | paste -sd, -)
log "watching devices: $OUR_DEVS"

# --- 2. marker + daemon --------------------------------------------------------
$SUDO mkdir -p /run/usbkeeper
$SUDO touch "$MARKER"
log "marker touched: $MARKER"

if curl -sf -m 3 "http://127.0.0.1:${PORT}/api/state" >/dev/null 2>&1; then
  log "daemon already listening on :$PORT (reusing)"
else
  log "starting daemon: $BIN --dest $ARCHIVE --port $PORT"
  $SUDO mkdir -p "$ROOT/logs"
  LOGF="$ROOT/logs/e2e-daemon.$(date +%s).log"
  $SUDO bash -c "setsid '$BIN' --dest '$ARCHIVE' --port '$PORT' >'$LOGF' 2>&1 & echo \$! > /tmp/usbk-e2e.pid"
  DAEMON_PID=$(cat /tmp/usbk-e2e.pid 2>/dev/null || true)
  log "daemon pid ${DAEMON_PID:-?} (log: $LOGF)"
  sleep 2
fi

# --- 3. poll /api/state --------------------------------------------------------
STATEF=/tmp/usbk-e2e-state.json
TL=/tmp/usbk-e2e-timeline.log
: > "$TL"
DEADLINE=$(( $(date +%s) + TIMEOUT ))
DONE_N=0; ERR_N=0; LAST=""
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  if curl -sf -m 5 "http://127.0.0.1:${PORT}/api/state" -o "$STATEF"; then
    PARSED=$(python3 - "$STATEF" "$OUR_DEVS" <<'PY'
import json, sys
statef, devs = sys.argv[1], set(sys.argv[2].split(","))
try:
    d = json.load(open(statef))
except Exception as e:
    print("PARSE_ERR"); sys.exit(0)
out = []
for p in d.get("ports", []):
    dev = p.get("device") or {}
    name = dev.get("dev") or p.get("port") or ""
    if name not in devs:
        continue
    st = str(dev.get("state", "?")).lower()
    err = (dev.get("error") or "").replace("\n", " ")[:120]
    dest = dev.get("dest") or ""
    pct = dev.get("pct")
    out.append((name, st, dest, str(pct) if pct is not None else "-", err))
for row in out:
    print("\t".join(row))
PY
)
    snap=$(echo "$PARSED" | awk -F'\t' 'NF>1{printf "%s=%s(%s) ",$1,$2,$4}' | xargs)
    [ "$snap" != "$LAST" ] && { echo "$(date +%H:%M:%S) $snap" >> "$TL"; LAST="$snap"; }
    log "state: ${snap:-no rows for our devices}"
    DONE_N=$(echo "$PARSED" | awk -F'\t' 'NF>1 && $2~/^(done|complete|completed|archived|ok)$/' | wc -l)
    ERR_N=$(echo "$PARSED"  | awk -F'\t' 'NF>1 && $2~/^(error|failed)$/' | wc -l)
    if [ $((DONE_N + ERR_N)) -ge 3 ]; then break; fi
  else
    log "poll: API not reachable yet"
  fi
  sleep 5
done
log "final states:"; cat "$TL" | sed 's/^/[e2e tl] /'
echo "$PARSED" > /tmp/usbk-e2e-final.tsv
[ "$DONE_N" -ge 3 ] || fail "did not reach 3x done within ${TIMEOUT}s (done=$DONE_N err=$ERR_N)"
[ "$ERR_N" -eq 0 ]  || fail "devices in error state: $(awk -F'\t' '$2~/^(error|failed)$/{print $1": "$5}' /tmp/usbk-e2e-final.tsv | paste -sd'; ' -)"

# --- 4. content verification ----------------------------------------------------
while IFS=$'\t' read -r DEV ST DEST PCT ERR; do
  [ -n "${DEV:-}" ] || continue
  NAME=$(awk -v d="/dev/$DEV" '$2==d{print $1}' "$HERE/sticks/registry.txt")
  [ -n "$NAME" ] || { log "no registry entry for $DEV"; continue; }
  if [ -n "$DEST" ] && [ "$DEST" != "-" ] && $SUDO test -d "$DEST"; then
    "$HERE/check-stick.sh" "$NAME" "$DEST" || fail "check-stick $NAME (dest=$DEST)"
  else
    "$HERE/check-stick.sh" "$NAME" "$ARCHIVE" || fail "check-stick $NAME (archive root)"
  fi
done < /tmp/usbk-e2e-final.tsv

# --- 5. reformat assertion: images should now be exfat --------------------------
# release any daemon-side mount of our loops first (read-only check, does not detach)
while IFS=$'\t' read -r NAME LOOP IMG MNT FS; do
  [ -n "${NAME:-}" ] || continue
  for t in $($SUDO findmnt -ln -o TARGET --source "$LOOP" 2>/dev/null); do
    case "$t" in /run/usbkeeper/*) $SUDO umount "$t" 2>/dev/null || true;; esac
  done
done < "$HERE/sticks/registry.txt"
sync
while IFS=$'\t' read -r NAME LOOP IMG MNT FS; do
  [ -n "${NAME:-}" ] || continue
  TYPE=$($SUDO blkid -o value -s TYPE "$IMG" 2>/dev/null || true)
  if [ "$TYPE" = exfat ]; then
    log "$NAME: image reformatted to exfat ✔"
  else
    fail "$NAME: expected blkid TYPE=exfat on $IMG, got '${TYPE:-none}'"
  fi
done < "$HERE/sticks/registry.txt"

# --- 6. teardown ------------------------------------------------------------------
"$HERE/teardown.sh" || fail "teardown reported errors"

if [ "$FAILED" = 0 ]; then
  echo "E2E PASS"
  exit 0
else
  echo "E2E FAIL (see [e2e] FAIL lines above)"
  exit 1
fi
