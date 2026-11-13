#!/usr/bin/env bash
# run-negative.sh — negative E2E: corrupt the DEST copy so the daemon's verify step
# fires "verify-mismatch", and assert the stick is NOT reformatted afterwards.
#
# The pipeline runs mounting->copying->verifying in ~1-2s, too fast to catch via
# /api/state sampling. Instead a background watcher (100ms loop) flips one byte of an
# already-copied file under the dest folder for as long as that folder exists. The
# daemon's post-copy checksum verify (rsync -c dry-run, src still mounted rw at that
# point) then sees a diff -> error + refuse-format. Because the watcher keeps flipping
# until terminal state, even a very fast copier gets caught.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"
BIN="$ROOT/target/release/usbkeeper"
ARCHIVE="$HERE/archive-neg"
PORT="${USBK_PORT:-8490}"
MARKER="/run/usbkeeper/allow-loop"
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"
log() { printf '[neg %s] %s\n' "$(date +%H:%M:%S.%N | cut -c1-12)" "$*"; }
FAILED=0
DAEMON_PID=""
WATCHER_PID=""
cleanup() {
  [ -n "$WATCHER_PID" ] && kill "$WATCHER_PID" 2>/dev/null
  [ -n "$DAEMON_PID" ] && { $SUDO kill "$DAEMON_PID" 2>/dev/null || true; }
}
trap cleanup EXIT

"$HERE/ensure-loop.sh" >/dev/null || { log "FATAL: loop unavailable"; exit 1; }
[ -x "$BIN" ] || { log "FATAL: binary missing at $BIN"; exit 3; }

start_daemon() {
  if curl -sf -m 3 "http://127.0.0.1:${PORT}/api/state" >/dev/null 2>&1; then
    log "reusing daemon on :$PORT"
  else
    $SUDO mkdir -p "$ROOT/logs"
    LOGF="$ROOT/logs/neg-daemon.$(date +%s).log"
    $SUDO bash -c "setsid '$BIN' --dest '$ARCHIVE' --port '$PORT' >'$LOGF' 2>&1 & echo \$! > /tmp/usbk-neg.pid"
    DAEMON_PID=$(cat /tmp/usbk-neg.pid); log "daemon pid $DAEMON_PID (log $LOGF)"
    sleep 2
  fi
}

attempt() {   # returns 0 when verify-mismatch fired AND no reformat happened
  $SUDO rm -rf "$ARCHIVE"; $SUDO mkdir -p "$ARCHIVE"; $SUDO chmod 0777 "$ARCHIVE"
  "$HERE/make-stick.sh" vfat 400 stick-neg || { log "make-stick failed"; return 1; }
  local LOOP MNT; LOOP=$(awk '$1=="stick-neg"{print $2}' "$HERE/sticks/registry.txt")
  MNT=$(awk '$1=="stick-neg"{print $4}' "$HERE/sticks/registry.txt")
  $SUDO umount "$MNT"                       # handoff: daemon mounts the raw loop itself
  local DEVNAME=${LOOP#/dev/}

  rm -f /tmp/usbk-neg-stop
  # background tamper-watcher: flip a random copied byte every 100ms while dest exists
  (
    local stop=0
    while [ ! -e /tmp/usbk-neg-stop ]; do
      dest=$($SUDO find "$ARCHIVE" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | head -1)
      if [ -n "$dest" ]; then
        victim=$($SUDO find "$dest" -type f ! -name SHA256MANIFEST -size +1k 2>/dev/null | head -1)
        [ -n "$victim" ] && $SUDO dd if=/dev/urandom of="$victim" bs=1 count=1 seek=$((RANDOM % 512)) conv=notrunc status=none 2>/dev/null
      fi
      sleep 0.1
    done
  ) &
  WATCHER_PID=$!

  $SUDO mkdir -p /run/usbkeeper; $SUDO touch "$MARKER"
  start_daemon

  local STATEF=/tmp/usbk-neg-state.json st="" err="" final=""
  local deadline=$(( $(date +%s) + 300 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    curl -sf -m 3 "http://127.0.0.1:${PORT}/api/state" -o "$STATEF" || { sleep 1; continue; }
    read -r st err < <(python3 - "$STATEF" "$DEVNAME" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); want=sys.argv[2]
for p in d.get("ports",[]):
    dev=p.get("device") or {}
    if (dev.get("dev") or p.get("port"))==want:
        print(str(dev.get("state","")).lower(), "|", (dev.get("error") or "").replace("\n"," ")[:160]); break
PY
)
    case "$st" in done|complete|completed|archived|ok|error|failed) final="$st"; break;; esac
    sleep 0.3
  done
  touch /tmp/usbk-neg-stop; wait "$WATCHER_PID" 2>/dev/null; WATCHER_PID=""
  log "final state=${final:-timeout} err='${err:-none}'"

  local TYPE; TYPE=$($SUDO blkid -o value -s TYPE "$HERE/sticks/stick-neg.img" 2>/dev/null || true)
  local ok=1
  case "$final" in
    error|failed) echo "$err" | grep -qi "verify-mismatch" \
      && log "OK: verify-mismatch raised" \
      || { log "FAIL: error but not verify-mismatch: $err"; ok=0; } ;;
    *) log "FAIL: terminal state '${final:-timeout}' instead of error"; ok=0 ;;
  esac
  if [ "$TYPE" = exfat ]; then log "FAIL: stick WAS reformatted despite error"; ok=0
  else log "OK: not reformatted after error (TYPE='${TYPE:-none}')"; fi
  "$HERE/teardown.sh" >/dev/null || ok=0
  return $((1 - ok))
}

if attempt || attempt; then echo "NEG PASS"; exit 0; else echo "NEG FAIL"; exit 1; fi
