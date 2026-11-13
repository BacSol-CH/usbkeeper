# usbkeeper test fixtures & E2E harness

Bash scripts that fabricate realistic USB-stick stand-ins (loop devices + seeded file
trees + sha256 manifests), verify archive copies, and tear everything down.
Status as of 2026-11-12: `run-e2e.sh` fully green against binary 20:19:42;
`run-negative.sh` green (verify-mismatch fires, stick NOT reformatted).

## Files
- `make-stick.sh FS SIZE_M [NAME]` — create `sticks/NAME.img`, `losetup -f --show -P`,
  mkfs (vfat|exfat|ext4), mount rw as root at `sticks/mnt/NAME`, seed a deterministic
  ~200-file tree (real bytes budgeted at ~55% of SIZE_M so any size works; names with
  spaces + UTF8 like `Aentenschoen.pdf`, deep dirs, one sparse bigfile), write
  `SHA256MANIFEST` at tree root (all files except itself, relative paths, sorted),
  append NAME→loopdev to `sticks/registry.txt`.
- `check-stick.sh NAME DESTROOT` — locate the archive folder (direct manifest hit, then
  `*NAME*` dir glob, then fixture-mount fallback), verify every entry exists + sha256;
  PASS/FAIL + diff count.
- `teardown.sh` — umount + `losetup -d` + rm images for registry entries only.
  Guard: detaches a loop only when `losetup -j <our image>` confirms it backs our file
  (survives foreign/recycled devices). Removes `/run/usbkeeper/allow-loop`. Idempotent.
- `ensure-loop.sh` — loads loop (+exfat) WITHOUT reboot: extracts loop.ko/exfat.ko from
  the cached pacman package matching the RUNNING kernel and insmods. Needed because
  the host ran K1 with only K2 modules installed (upgraded, no reboot).
  Uses `max_loop=16` deliberately: static nodes are required (no udev rules create loop
  nodes dynamically here, LOOP_MIN_COUNT=0). Consequence: phantom `/dev/loopN` nodes
  show up in the daemon's scan — the E2E ignores non-registry devices by design.
- `smoke-test.sh` — self-test without the daemon (make x2 → rsync sim → check →
  negative control → teardown → idempotency). Was green on 2026-11-12.
- `run-e2e.sh` — the single E2E gate: 3 sticks (vfat 300M, ext4 500M, vfat 250M),
  HANDOFF-UMOUNT of fixture mounts (the daemon mounts raw loops itself at
  `/run/usbkeeper/loopN/fs` and errors "already mounted" otherwise), touch marker,
  start `../target/release/usbkeeper --dest archive --port 8489` if not up, poll
  `/api/state` until all OUR devices are terminal (timeout 600s), timeline to
  `/tmp/usbk-e2e-timeline.log`, `check-stick.sh` per stick (dest hint taken from API),
  assert `blkid TYPE=exfat` on every image, teardown. Exit nonzero on any failure.
- `run-negative.sh` — corrupts the dest copy via a 100 ms background byte-flipper
  while the daemon copies; asserts final state=error with "verify-mismatch" and that
  the image was NOT reformatted. The copy+verify window is ~2 s so tamper-before-
  verify-scan is timing-dependent (~1 miss in 5 observed); the script retries once
  automatically. Uses port 8490 by default (USBK_PORT to override) so it does not
  collide with the main e2e's 8489.

## Running
```
./smoke-test.sh            # no daemon needed
./run-e2e.sh               # full gate, needs target/release/usbkeeper
./run-negative.sh          # verify-mismatch / refuse-format case
```
As the operator (passwordless sudo) or root; scripts adapt. `/api/state` schema the
pollers speak: `{"ports":[{"port":"loopN","device":{"dev","state","dest","pct","error"}}]}`
(done words: done|complete|completed|archived|ok; error words: error|failed).

## Caveats
- **Kernel/module skew**: after a reboot into ≥K2 `ensure-loop.sh` becomes a
  passthrough; until then it insmods from `/var/cache/pacman/pkg/linux-<run>…pkg.tar.zst`.
  A kernel upgrade without reboot silently kills all loop fixtures — re-run
  ensure-loop.sh after detaching leftovers.
- **mkfs.exfat labels**: exfatprogs 1.4.4 rejects labels ≥12 chars with the misleading
  "input string is too long" (empirical cap: 11 chars; daemon uses an 11-char
  `USBK-261112`-style label since the 20:19 rebuild). Fixture e2e asserts only
  TYPE=exfat, not the label.
- **exfat mounts** need the exfat kernel module; `mkfs.exfat`/`blkid` are userspace and
  work regardless, so the reformat assertion never depends on module availability.
- **udisks2**: inactive on the host; if ever enabled it can automount fresh loop
  partitions under /run/media and race our explicit mounts. Teardown never touches
  foreign loops (registry + `losetup -j` guard), so a race costs a failed fixture,
  never real data.
- **Dest folder naming**: the current daemon archives under
  `archive/unknown_unknown-7-N-YYYYMMDD` (no vendor/serial on loops). check-stick
  therefore prefers the API `dest` hint; the `*NAME*` glob is fallback only.
- **Concurrent-harness races**: a parallel harness's cleanup `kill`ed one e2e run mid-flight and
  detached all loops (20:08 incident). Registry + guards meant teardown stayed safe.
  Only one harness should own the marker file + loop devices at a time; ports differ
  (8489 e2e, 8490 negative, third-party harness 8499).
