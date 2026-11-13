#!/usr/bin/env bash
# make-stick.sh FS SIZE_M [NAME] — fabricate a USB-stick stand-in on the host.
#
# Creates a sparse disk image, attaches it via losetup, mkfs's it (vfat|exfat|ext4),
# mounts it rw as root and seeds a deterministic ~200-file tree plus SHA256MANIFEST.
# NAME->loopdev mapping is appended to test/sticks/registry.txt (teardown.sh consumes it).
#
# PREREQ: the loop driver must be loaded. On this host the running kernel may lag the
# installed module tree (kernel upgraded without reboot); see test/ensure-loop.sh which
# insmods loop (+exfat/fat) from the matching cached pacman package.
# NOTE: the usbkeeper DAEMON only accepts loop-backed "sticks" when the test-mode marker
# file /run/usbkeeper/allow-loop exists. run-e2e.sh touches it; make-stick does not
# (fixture creation itself is daemon-independent).
#
# Usage: sudo ./make-stick.sh vfat 300 stick1   (run as root or with passwordless sudo)
set -euo pipefail

FS="${1:?usage: make-stick.sh FS SIZE_M [NAME]}"
SIZE_M="${2:?usage: make-stick.sh FS SIZE_M [NAME]}"
NAME="${3:-stick-$(date +%s%N | tail -c 7)}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STICKS="$HERE/sticks"
MNTROOT="$STICKS/mnt"
REG="$STICKS/registry.txt"
IMG="$STICKS/$NAME.img"
MNT="$MNTROOT/$NAME"
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"

log() { printf '[make-stick %s] %s\n' "$NAME" "$*"; }
die() { printf '[make-stick %s] FATAL: %s\n' "$NAME" "$*" >&2; exit 1; }

case "$FS" in
  vfat)  MKCMD=(mkfs.vfat -F32) ;;
  exfat) MKCMD=(mkfs.exfat) ;;
  ext4)  MKCMD=(mkfs.ext4 -q -F) ;;
  *) die "unknown FS '$FS' (vfat|exfat|ext4)" ;;
esac
[[ "$SIZE_M" =~ ^[0-9]+$ ]] && [ "$SIZE_M" -ge 32 ] || die "SIZE_M must be an integer >= 32"

# --- prereq checks -----------------------------------------------------------
$SUDO modprobe loop 2>/dev/null || true   # works once modules match running kernel
grep -qw loop /proc/devices || die "loop driver not loaded; run test/ensure-loop.sh first"
if [ "$FS" = exfat ]; then
  $SUDO modprobe exfat 2>/dev/null || true
  grep -qw exfat /proc/filesystems || die "exfat fs not available (module for running kernel missing); run test/ensure-loop.sh"
fi

mkdir -p "$STICKS" "$MNTROOT"
touch "$REG"
[ -e "$IMG" ] && die "image already exists: $IMG (run teardown.sh first or pick another NAME)"
if awk -v n="$NAME" '$1==n{found=1} END{exit !found}' "$REG"; then
  die "'$NAME' already in registry (stale entry? clean $REG or run teardown.sh)"
fi

# --- create + attach + format ------------------------------------------------
log "creating ${SIZE_M}M $FS image at $IMG"
$SUDO truncate -s "${SIZE_M}M" "$IMG"
LOOP=$($SUDO losetup -f --show -P "$IMG") || die "losetup failed"
[ -n "$LOOP" ] || die "losetup returned empty device"
log "attached $LOOP"

seed_ok=0
cleanup_fail() {
  [ "$seed_ok" = 1 ] && return 0
  $SUDO umount "$MNT" 2>/dev/null || true
  $SUDO losetup -d "$LOOP" 2>/dev/null || true
  $SUDO rm -f "$IMG" || true
  log "rolled back partial fixture"
}
trap cleanup_fail EXIT

$SUDO "${MKCMD[@]}" "$LOOP" >/dev/null
log "formatted ($($SUDO blkid -o value -s TYPE "$LOOP" 2>/dev/null || echo '?'))"
$SUDO mkdir -p "$MNT"
$SUDO mount "$LOOP" "$MNT"
log "mounted rw at $MNT"

# --- deterministic seed (~200 files, mixed sizes, spaces/UTF8/deep dirs) ----
# PRNG stream seeded from NAME so every stick is reproducible per name.
# Total real bytes is budgeted at ~55% of the image size so any SIZE_M works
# (the sparse bigfile does not count against the budget).
$SUDO python3 - "$NAME" "$MNT" "$SIZE_M" <<'PYEOF'
import os, random, sys

name, mnt, size_m = sys.argv[1], sys.argv[2], int(sys.argv[3])
rng = random.Random("usbkeeper-seed-v1:" + name)
budget = int(size_m * 1024 * 1024 * 0.55)

utf8_words = ["Aentenschoen", "Buecherverzeichnis", "Gmuende", "Kaedee", "Oeffnungszeiten",
              "Urnersee", "Weichweizen", "Zuerich", "Ölwechsel", "Ärztebrief"]
dirs = []
def mk(path):
    p = os.path.join(mnt, path)
    os.makedirs(p, exist_ok=True)
    dirs.append(p)

for top in ["Dokumente", "Photos", "Musik", "Backups", "Misc"]:
    mk(top)
for i in range(14):
    d = os.path.join(rng.choice(["Dokumente", "Photos", "Backups"]),
                     f"Ordner{i}", rng.choice(["tief", "tiefer", "am tiefsten"]), f"L{i%4}")
    mk(d)
mk(os.path.join("Misc", "a b c"))          # space in dirname
mk(os.path.join("Dokumente", "Verträge"))   # UTF8 dirname

plan = []   # (relpath, size, kind)
# one sparse-ish bigfile (declared 20M max, few real blocks — outside the budget)
if size_m >= 25:
    plan.append((f"{name}-bigfile.sparse", min(20 * 1024 * 1024, (size_m - 20) * 1024 * 1024), "sparse"))
# UTF8/space names at root
for w in utf8_words[:6]:
    plan.append((f"{w}.pdf", 0, "fill"))
plan.append(("Notizen mit  Leerzeichen.txt", 1024, "fixed"))

# ~192 distributed files; sizes drawn as weights over the remaining budget
while len(plan) < 200:
    d = rng.choice(dirs)
    rel = os.path.relpath(d, mnt)
    fname = rng.choice(["file", "dok", "scan", "foto", "bericht"]) + \
            f"{rng.randint(0,999):03d}" + rng.choice([".txt", ".pdf", ".jpg", ".bin", ".docx"])
    plan.append((os.path.join(rel, fname), 0, "fill"))

fill_n = sum(1 for _, _, k in plan if k == "fill")
weights = [rng.random() ** 2 for _ in range(fill_n)]  # small-biased
wsum = sum(weights) or 1.0
avail = budget - sum(s for _, s, k in plan if k != "fill") - fill_n * 1024
wi = iter(weights)
sizes = [(max(1024, int(avail * next(wi) / wsum)) if k == "fill" else s)
         for (_p, s, k) in plan]

count = 0
for (rel, _s, kind), sz in zip(plan, sizes):
    p = os.path.join(mnt, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    if kind == "sparse":
        with open(p, "wb") as f:
            f.write(b"SparseBigFileHeader\n")
            f.truncate(sz)
    else:
        with open(p, "wb") as f:
            written = 0
            while written < sz:
                chunk = os.urandom(min(1 << 20, sz - written))
                f.write(chunk); written += len(chunk)
    count += 1
print(f"seeded {count} files (~{sum(sz for (p,s2,k),sz in zip(plan,sizes) if k!='sparse')//1024}KB real) into {mnt}")
PYEOF

# --- manifest: sha256 of everything EXCLUDING SHA256MANIFEST itself ---------
LIST=/tmp/usbk-mf.$$.list
$SUDO find "$MNT" -type f ! -name SHA256MANIFEST -printf '%P\0' > "$LIST"
$SUDO sh -c 'cd "$1" && xargs -0 -r sha256sum < "$2" | LC_ALL=C sort -k2 > "$1/SHA256MANIFEST"' _ "$MNT" "$LIST"
rm -f "$LIST"
NF=$($SUDO grep -c . "$MNT/SHA256MANIFEST")
log "manifest written: $NF entries at $MNT/SHA256MANIFEST"

# --- registry ----------------------------------------------------------------
$SUDO chown -R "$(id -u):$(id -g)" "$MNT" 2>/dev/null || $SUDO chmod -R a+rX "$MNT" 2>/dev/null || true
printf '%s\t%s\t%s\t%s\t%s\n' "$NAME" "$LOOP" "$IMG" "$MNT" "$FS" >> "$REG"
seed_ok=1
trap - EXIT
log "DONE: $NAME ($FS, ${SIZE_M}M, $LOOP, mounted at $MNT)"
