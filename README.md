# usbkeeper

> **Status: Unmaintained.** This tool was built for one specific hardware
> setup and reached its final state. It will NOT be maintained or developed
> further: no bug fixes, no security updates, no dependency bumps, no PRs.
> Use it as-is at your own risk; fork it if you need something alive.

Root-run USB-stick ingest daemon for the host: auto-detects qualifying sticks,
archives their content read-only, verifies by checksum, then reformats the
stick to a clean exFAT filesystem. Ships an embedded HTTP/SSE API and serves
the single-file WebUI in `web/`.

## Start it (real sticks)

```sh
./run.sh                      # preflight + sudo + launch on :8488
USBK_DEST=/path/to/archive ./run.sh --port 9000
```

`run.sh` checks for `mkfs.exfat` (exfatprogs), `rsync`, `blkid`, `wipefs`,
creates the archive dir, then runs:

```sh
usbkeeper --dest /var/lib/usb-archive --port 8488 --ui-root /var/lib/usbkeeper/web
```

No flags are needed for real sticks: anything that shows up in `/sys/block`
with a USB parent chain and removable media is picked up automatically every
2 s. **Loop devices are ignored unless** `--allow-loop` or the marker file
`/run/usbkeeper/allow-loop` exists — that mode is for testing only.

## Pipeline (per stick, parallel threads)

`detected -> mounting -> copying -> verifying -> unmounting -> formatting -> done`

1. poll `/sys/class/block`; qualify = USB parent chain AND removable==1 (or usb-storage driver)
2. `mount -o ro` at `/run/usbkeeper/<dev>/fs` using the blkid-detected fstype
   (unsupported fs -> state `error`, never formatted)
3. `rsync` copy into `$DEST/<stick-id>/` (`-aH` for ext4/ntfs sources;
   `-rltDox` for vfat/exfat, which cannot carry timestamps/ownership)
4. verify: `rsync -rcn` checksum dry-run must list zero differences, plus a
   size cross-check (5 % / 8 MB slack on FAT-family padding). Any mismatch ->
   `verify-mismatch` error, format is NOT attempted, card stays red.
5. re-check sysfs USB guard IMMEDIATELY before wiping (safety law), refuse
   >= 4 TB, refuse any device with live mounts, hard-exclude `/` and `/home`
6. `wipefs -a` + `mkfs.exfat -F -L USBK-YYMMDD` whole device
   (exfatprogs 1.4.4 caps labels at 8 chars; spec's `USBK-YYYYMMDD` does not fit)
7. state `done`. Errors persist until rescan/removal.

Archive folder naming (`stick-id`): sanitized serial when present, else
`vendor_model-MAJ-MIN-YYYYmmdd`, all suffixed with the ingest date.

## HTTP API (frozen contract)

| Route | Result |
|---|---|
| `GET /api/state` | `{"ports":[{"port":"1-6.1","speed":"5000","occupied":true,"device":{...}}]}` |
| `GET /events` | SSE stream, event name `state`, data = one port object (~2/s throttle during copy) |
| `GET /` , `GET /ui` | `/var/lib/usbkeeper/web/index.html` if present, else placeholder "waiting-for-ui" |
| `POST /api/rescan` | forces a watcher pass, returns `{"ok":true}` |

Device fields: `dev vendor model serial size_bytes fs dest state pct
bytes_done bytes_total bytes_per_sec eta_sec error ts`.

## Testing

* unit: `cargo test --release` (progress parser, JSON ts, id sanitizer, guards)
* loop-device E2E (as root): `./e2e_test.sh` — seeds vfat+exfat+ext4 fake
  sticks with sha256 manifests, asserts full pipeline incl. `done`, hash
  match in archive, exFAT reformat, SSE validity, and that loops stay
  invisible without the allow-loop marker.
* fixture-harness E2E: `test/run-e2e.sh` (same assertions, separate harness).

## Design notes

Zero external crates: std::thread + hand-rolled HTTP/1.1+SSE over
`TcpListener`. The workload is blocking subprocess I/O (rsync/mkfs) and a 2 s
poll loop — async adds risk without benefit here. One thread per stick,
shared registry behind a Mutex, bounded sync_channel fan-out to SSE clients
(slow client events dropped, connection kept).

Known gaps: speed label falls back to raw sysfs Mbps value for non-USB3/2
rates; a stick pulled mid-copy lands in `error` (by design); no systemd unit
— run via `run.sh` or your supervisor of choice.
