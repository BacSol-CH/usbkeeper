#!/usr/bin/env bash
# ensure-loop.sh — make the loop driver usable on the host WITHOUT a reboot.
#
# Problem: the host runs kernel K1 but only the K2 module tree is
# installed (kernel upgraded, no reboot since). modprobe therefore finds nothing,
# CONFIG_BLK_DEV_LOOP=m and LOOP_MIN_COUNT=0 => /dev/loop* absent.
# Fix: extract loop.ko (+ exfat.ko for exfat fixtures) from the cached pacman
# package that MATCHES the running kernel and insmod it. After a reboot into the
# current kernel this script becomes a plain `modprobe loop` passthrough.
set -uo pipefail
SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo -n"
log() { printf '[ensure-loop] %s\n' "$*"; }

$SUDO modprobe loop 2>/dev/null || true
if ! grep -qw loop /proc/devices; then
  RUN=$(uname -r)
  PKG=$(ls -1 /var/cache/pacman/pkg/linux-"${RUN/-/.}"*.pkg.tar.zst 2>/dev/null | head -1)
  [ -n "$PKG" ] || { log "FATAL: no cached linux pkg matching running kernel $RUN — reboot required"; exit 1; }
  W=/tmp/usbk-kx.$$
  $SUDO rm -rf "$W"; $SUDO mkdir -p "$W"
  log "extracting loop.ko from $PKG"
  $SUDO tar --zstd -xf "$PKG" -C "$W" "usr/lib/modules/$RUN/kernel/drivers/block/loop.ko.zst" ||
    { log "FATAL: loop.ko not in $PKG"; $SUDO rm -rf "$W"; exit 1; }
  $SUDO zstd -df "$W/usr/lib/modules/$RUN/kernel/drivers/block/loop.ko.zst" -o "$W/loop.ko"
  sync
  $SUDO insmod "$W/loop.ko" max_loop=16 || { log "FATAL: insmod loop failed"; $SUDO rm -rf "$W"; exit 1; }
  $SUDO rm -rf "$W"
  log "loop.ko insmod'ed (matched to running kernel from pacman cache)"
fi
grep -qw loop /proc/devices || { log "FATAL: loop still missing"; exit 1; }
[ -e /dev/loop-control ] || { log "FATAL: /dev/loop-control missing"; exit 1; }

# optional: exfat kernel support (mkfs.exfat + blkid work without it; mount -t exfat needs it)
$SUDO modprobe exfat 2>/dev/null || true
if ! grep -qw exfat /proc/filesystems; then
  RUN=$(uname -r)
  PKG=$(ls -1 /var/cache/pacman/pkg/linux-"${RUN/-/.}"*.pkg.tar.zst 2>/dev/null | head -1)
  if [ -n "$PKG" ]; then
    W=/tmp/usbk-kx.$$
    $SUDO rm -rf "$W"; $SUDO mkdir -p "$W"
    if $SUDO tar --zstd -xf "$PKG" -C "$W" \
        "usr/lib/modules/$RUN/kernel/fs/exfat/exfat.ko.zst" 2>/dev/null; then
      $SUDO zstd -df "$W/usr/lib/modules/$RUN/kernel/fs/exfat/exfat.ko.zst" -o "$W/exfat.ko"
      sync
      if ! $SUDO insmod "$W/exfat.ko" 2>/dev/null; then
        # unresolved 'fat' symbol: load fat.ko first, then retry
        if $SUDO tar --zstd -xf "$PKG" -C "$W" \
            "usr/lib/modules/$RUN/kernel/fs/fat/fat.ko.zst" 2>/dev/null &&
           $SUDO zstd -df "$W/usr/lib/modules/$RUN/kernel/fs/fat/fat.ko.zst" -o "$W/fat.ko" &&
           sync && $SUDO insmod "$W/fat.ko" 2>/dev/null; then
          $SUDO insmod "$W/exfat.ko" || log "warn: exfat insmod failed"
        else
          log "warn: fat dependency unavailable"
        fi
      fi
    fi
    $SUDO rm -rf "$W"
  fi
fi
if grep -qw exfat /proc/filesystems; then
  log "OK: loop ready; exfat mountable"
else
  log "OK: loop ready; exfat module NOT available (mount -t exfat will fail; mkfs.exfat+blkid still OK)"
fi
