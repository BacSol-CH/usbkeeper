#!/usr/bin/env bash
# check-stick.sh NAME DESTROOT — verify an archived stick copy against its SHA256MANIFEST.
#
# Finds the DESTROOT subfolder belonging to NAME (name-pattern match, deepest candidate
# wins), reads the manifest that make-stick.sh wrote at the tree root (searched in the
# dest folder and in the still-mounted fixture), and checks every entry: exists + sha256.
# Prints PASS/FAIL and a diff count. Exit 0 on PASS.
set -euo pipefail

NAME="${1:?usage: check-stick.sh NAME DESTROOT}"
DESTROOT="${2:?usage: check-stick.sh NAME DESTROOT}"
[ -d "$DESTROOT" ] || { echo "[check-stick $NAME] FATAL: DESTROOT not a directory: $DESTROOT"; exit 2; }

log() { printf '[check-stick %s] %s\n' "$NAME" "$*"; }

# --- locate the dest subfolder for this stick --------------------------------
DEST=""
# direct hit: DESTROOT itself is the archived folder (e.g. API dest hint)
if [ -f "$DESTROOT/SHA256MANIFEST" ]; then
  DEST="$DESTROOT"
fi
if [ -z "$DEST" ]; then
  CAND=$(find "$DESTROOT" -maxdepth 3 -type d \( -iname "*${NAME}*" \) 2>/dev/null | LC_ALL=C sort)
  if [ -z "$CAND" ]; then
    log "FAIL: no subfolder matching '$NAME' under $DESTROOT (and no manifest at root)"
    exit 1
  fi
  # prefer the deepest dir that actually contains (or leads to) a SHA256MANIFEST
  while IFS= read -r c; do
    if find "$c" -maxdepth 2 -name SHA256MANIFEST -print -quit 2>/dev/null | grep -q .; then
      DEST="$c"; break
    fi
  done < <(echo "$CAND" | tac)
  [ -n "$DEST" ] || DEST=$(echo "$CAND" | head -1)
fi
log "dest folder: $DEST"

MAN=$(find "$DEST" -maxdepth 2 -name SHA256MANIFEST -print -quit 2>/dev/null)
if [ -z "$MAN" ]; then
  # fall back to the live fixture mount recorded in registry.txt
  HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  FIXMNT=$(awk -v n="$NAME" '$1==n{print $4}' "$HERE/sticks/registry.txt" 2>/dev/null | tail -1)
  [ -n "$FIXMNT" ] && [ -f "$FIXMNT/SHA256MANIFEST" ] && MAN="$FIXMNT/SHA256MANIFEST"
fi
[ -n "$MAN" ] || { log "FAIL: SHA256MANIFEST not found in dest or fixture mount"; exit 1; }
log "manifest: $MAN"

# --- verify -------------------------------------------------------------------
TOTAL=0; MISSING=0; MISMATCH=0; BAD=0
while IFS= read -r line; do
  [ "${#line}" -ge 67 ] || continue
  hash="${line:0:64}"; rel="${line:66}"
  TOTAL=$((TOTAL+1))
  f="$DEST/$rel"
  if [ ! -f "$f" ]; then
    MISSING=$((MISSING+1)); log "MISSING: $rel"; continue
  fi
  actual=$(sha256sum "$f" | cut -d' ' -f1)
  if [ "$actual" != "$hash" ]; then
    MISMATCH=$((MISMATCH+1)); log "HASH-MISMATCH: $rel (want ${hash:0:12}… got ${actual:0:12}…)"
  fi
done < <(cat "$MAN")

DIFF=$((MISSING+MISMATCH))
log "checked $TOTAL entries — missing=$MISSING mismatch=$MISMATCH"
if [ "$TOTAL" -eq 0 ]; then
  log "FAIL: manifest empty"
  exit 1
elif [ "$DIFF" -eq 0 ]; then
  echo "PASS $NAME ($TOTAL files verified in $DEST)"
else
  echo "FAIL $NAME (diff count: $DIFF / $TOTAL)"
  exit 1
fi
