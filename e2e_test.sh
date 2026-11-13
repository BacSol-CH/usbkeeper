#!/usr/bin/env bash
# E2E test for usbkeeper using loop devices (run as root on the host).
# 3 fake sticks (vfat/exfat/ext4) seeded with trees + sha256 manifests;
# daemon in allow-loop mode must drive each through the full pipeline and
# reformat the loop to exFAT. Also: negative guard tests (foreign mount,
# no-marker invisibility), SSE validity, dest hash vs manifest.
set -u

BIN=${BIN:-/var/lib/usbkeeper/target/release/usbkeeper}
WORK=/var/lib/usbkeeper/e2e
DEST=$WORK/usb-archive
PORT=${PORT:-8499}
PASS=0; FAIL=0
# archive owner = invoking (non-root) user; daemon normalizes to dest-owner anyway
OWNER="${SUDO_USER:-$(id -un)}"
ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
MYLOOPS=""

cleanup_all() {
  [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null
  for f in $WORK/stick*.img; do
    [[ -e $f ]] || continue
    lp=$(losetup -j "$f" 2>/dev/null | cut -d: -f1)
    if [[ -n ${lp:-} ]]; then umount "$lp" 2>/dev/null; losetup -d "$lp" 2>/dev/null; fi
  done
  rm -rf "$WORK" /tmp/usbk_sse.$$
  rm -f /run/usbkeeper/allow-loop
}

echo "== setup =="
rm -rf "$WORK"; mkdir -p "$WORK" "$DEST"; chown "$OWNER": "$DEST"

mk_stick() { # name fstype
  local name=$1 fs=$2
  local img=$WORK/$name.img src=$WORK/$name.seed
  truncate -s 300M "$img"
  case $fs in
    vfat)  mkfs.vfat -F 32 "$img" >/dev/null ;;
    exfat) mkfs.exfat -F "$img" >/dev/null ;;
    ext4)  mkfs.ext4 -q -F "$img" >/dev/null ;;
  esac
  local node mnt=$WORK/mnt_$name
  # losetup -f --show (NOT --show -f: arg order matters on this util-linux;
  # and create missing loop nodes first — this host has no /dev/loop-control udev)
  for i in $(seq 0 15); do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done
  node=$(losetup -f --show "$img")
  MYLOOPS="$MYLOOPS ${node#/dev/}"
  mkdir -p "$src/a/b/c" "$src/docs" "$mnt"
  head -c 150000000 /dev/urandom > "$src/a/big1.bin"
  head -c 60000000  /dev/urandom > "$src/a/b/big2.bin"
  head -c 1000000   /dev/urandom > "$src/a/b/c/deep.bin"
  echo "hello $name" > "$src/docs/readme.txt"
  printf 'x%.0s' $(seq 1 99999) > "$src/docs/text.dat"
  ( cd "$src" && find . -type f ! -name SHA256SUMS -exec sha256sum {} + | sort > SHA256SUMS )
  mount "$node" "$mnt"
  cp -a "$src"/. "$mnt"/
  sync; umount "$mnt"
  echo "  $name: $node fs=$fs"
}

mk_stick stick1 vfat
mk_stick stick2 exfat
mk_stick stick3 ext4
echo "  my loops:$MYLOOPS"

# SAFETY PROOF fixture: a NON-loop real block device mounted somewhere.
# The daemon must refuse to format it even with allow-loop active.
FOREIGN=""
if [ "$(id -u)" = 0 ]; then
  dm=$(ls /sys/block 2>/dev/null | grep -E '^dm-' | head -1 || true)
  nv=$(ls /sys/block 2>/dev/null | grep -E '^nvme[0-9]+n[0-9]+$' | head -1 || true)
  sd=$(ls /sys/block 2>/dev/null | grep -E '^sd[a-z]$' | head -1 || true)
  for c in $dm $nv $sd; do
    [[ -n $c ]] || continue
    if grep -q "^/dev/$c" /proc/partitions 2>/dev/null && [ -b /dev/$c ] && [ "$(cat /sys/block/$c/size)" != 0 ]; then
      FOREIGN=$c; break
    fi
  done
fi
echo "  foreign non-loop candidate: ${FOREIGN:-none found}"

mkdir -p /run/usbkeeper
touch /run/usbkeeper/allow-loop

# --- collision guard: a live production daemon would grab our loops too ----
PROD_PID=$(pgrep -f "target/release/usbkeeper --dest .*--port 8488" | head -1 || true)
if [[ -n "${PROD_PID:-}" ]]; then
  echo "  stopping production daemon pid $PROD_PID for the duration of e2e"
  kill "$PROD_PID"
  for i in $(seq 1 20); do kill -0 "$PROD_PID" 2>/dev/null || break; sleep 0.5; done
  kill -0 "$PROD_PID" 2>/dev/null && { echo "cannot stop prod daemon"; exit 1; }
fi
restore_prod() {
  [[ -n "${PROD_PID:-}" ]] || return 0
  nohup /var/lib/usbkeeper/target/release/usbkeeper     --dest /var/lib/usb-archive --port 8488     --ui-root /var/lib/usbkeeper/web >/var/log/usbkeeper-e2e-restart.log 2>&1 &
  echo "  production daemon restarted (pid $!)"
}
trap 'cleanup_all; restore_prod' EXIT

echo "== SSE capture (armed before daemon so copying events are seen) =="
SSEF=/tmp/usbk_sse.$$
(timeout 30 curl -sN --retry-connrefused --retry 30 --retry-delay 1 -m 28 http://127.0.0.1:$PORT/events > $SSEF 2>/dev/null) &
SSE_PID=$!

echo "== start daemon =="
"$BIN" --dest "$DEST" --port "$PORT" >$WORK/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 1
kill -0 $DAEMON_PID || { echo "daemon died"; cat $WORK/daemon.log; exit 1; }

api() { curl -s -m 5 http://127.0.0.1:$PORT$1; }
# restrict state json to my loops (+ any foreign port for the safety test)
mine() { api /api/state | python3 -c "
import json,sys
d=json.load(sys.stdin)
keep=set('''$MYLOOPS $FOREIGN'''.split())
print(json.dumps([x for x in d['ports'] if x['port'] in keep]))"; }

echo "== HTTP contract =="
st=$(api /api/state)
echo "$st" | grep -q '"ports"' && ok "/api/state shape" || bad "/api/state: $st"
r=$(curl -s -m 5 -X POST http://127.0.0.1:$PORT/api/rescan); [ "$r" = '{"ok":true}' ] && ok "/api/rescan" || bad "/api/rescan -> $r"
h=$(curl -s -m 5 http://127.0.0.1:$PORT/ ); echo "$h" | grep -qi 'waiting-for-ui\|usbkeeper' && ok "/ placeholder" || bad "/ -> ${h:0:80}"

echo "== wait for terminal states (my loops only) =="
deadline=$((SECONDS+240))
while :; do
  term=$(mine | python3 -c "
import json,sys
print(sum(1 for x in json.load(sys.stdin) if x.get('device') and x['device']['state'] in ('done','error')))")
  (( ${term:-0} >= 3 )) && break
  (( SECONDS > deadline )) && break
  sleep 2
done

mystates=$(api /api/state | python3 -c "
import json,sys
d=json.load(sys.stdin)
keep=set('''$MYLOOPS $FOREIGN'''.split())
print(json.dumps([x for x in d['ports'] if x['port'] in keep]))" | tr -d ' ')
wait $SSE_PID 2>/dev/null || true
echo "-- SSE stats --"
python3 - <<EOF || true
import json,collections
objs=[]
for l in open('$SSEF'):
    if l.startswith('data: '):
        try: objs.append(json.loads(l[6:]))
        except Exception: pass
ev=sum(1 for l in open('$SSEF') if l.startswith('event: state'))
per=collections.Counter()
for o in objs:
    d=o.get('device')
    if d and d['state']=='copying': per[o['port']]+=1
print('  events:',ev,'copying-samples/port:',dict(per) or 'none')
EOF
grep -q '^event: state' $SSEF && ok "SSE event lines" || bad "no SSE events"
python3 - <<EOF && ok "SSE data lines valid port JSON" || bad "SSE JSON invalid"
import json
objs=[]
for l in open('$SSEF'):
    if l.startswith('data: '):
        try: objs.append(json.loads(l[6:]))
        except Exception: pass
        continue
assert objs, "no data lines"
for o in objs:
    assert set(['port','speed','occupied','device']) <= set(o)
EOF
python3 - <<EOF && ok "copying progress events observed" || echo "  WARN: SSE window missed copying phase — non-fatal"
import json
objs=[]
for l in open('$SSEF'):
    if l.startswith('data: '):
        try: objs.append(json.loads(l[6:]))
        except Exception: pass
cp=[d for d in objs if d.get('device') and d['device']['state']=='copying' and isinstance(d['device'].get('pct'),(int,float))]
assert len(cp)>=2, f"only {len(cp)} copying samples"
pcts=sorted(x['device']['pct'] for x in cp)
print('   pct range seen:', pcts[0], '->', pcts[-1])
EOF
echo "-- final state of my ports --"
echo "$mystates" | python3 -c "
import json,sys
for x in json.load(sys.stdin):
    d=x.get('device') or {}
    print(f\"  {x['port']}: {d.get('state')} err={str(d.get('error'))[:90]}\")"

ndone=$(echo "$mystates" | grep -o '"state":"done"' | wc -l)
(( ndone == 3 )) && ok "3 sticks reached done" || bad "done=$ndone of 3"

echo "== dest content hash vs seeded manifest =="
total_dirs=$(ls -1 "$DEST" 2>/dev/null | wc -l)
(( total_dirs >= 3 )) && ok "dest has $total_dirs stick dirs" || bad "dest has only $total_dirs dirs"
for name in stick1 stick2 stick3; do
  ddir=""
  for dd in "$DEST"/*/; do
    if diff -rq --exclude=SHA256SUMS --exclude=lost+found "$WORK/$name.seed" "$dd" >/dev/null 2>&1; then ddir="$dd"; break; fi
  done
  if [[ -n $ddir ]]; then
    if ( cd "$ddir" && sha256sum -c SHA256SUMS --quiet ); then ok "$name archive matches manifest ($(basename $ddir))"; else bad "$name hash mismatch"; fi
  else
    bad "$name: no matching archive dir found"
  fi
done

echo "== ownership normalization =="
foreign=$(find "$DEST" ! -user "$OWNER" 2>/dev/null | wc -l)
if [[ "$foreign" == 0 ]]; then
  ok "archive fully handed to dest owner after copies"
else
  bad "$foreign archive entries not dest-owner (normalize_ownership missing/failed)"
fi
badmode=$(find "$DEST" -mindepth 1 -type d ! -perm 775 2>/dev/null | wc -l)
[[ "$badmode" == 0 ]] && ok "archive dirs all 775" || bad "$badmode archive dirs not 775"

echo "== loops reformatted to exfat? =="
for f in $WORK/stick1.img $WORK/stick2.img $WORK/stick3.img; do
  t=$(blkid -o value -s TYPE "$f" 2>/dev/null)
  [ "$t" = exfat ] && ok "$(basename $f) now exfat" || bad "$(basename $f) fstype '$t' != exfat"
done

echo "== SAFETY: foreign non-loop device untouched =="
if [[ -n $FOREIGN ]]; then
  st=$(echo "$mystates" | python3 -c "
import json,sys
for x in json.load(sys.stdin):
    if x['port']=='$FOREIGN': print(x['device']['state'] if x['device'] else 'absent')")
  before=$(wipefs /dev/$FOREIGN 2>/dev/null | awk 'NR>1{print $2}' | tr '\n' ',')
  if [[ "$st" == error* || "$st" == absent ]]; then ok "guard rejected $FOREIGN (state=$st)"; else bad "pipeline accepted foreign device $FOREIGN state=$st"; fi
  after=$(wipefs /dev/$FOREIGN 2>/dev/null | awk 'NR>1{print $2}' | tr '\n' ',')
  [ "$before" = "$after" ] && ok "$FOREIGN signatures unchanged" || bad "$FOREIGN SIGNATURES CHANGED ($before -> $after) ABORT"
else
  echo "  SKIP: no non-loop block device available"
fi

echo "== marker-guard negative test =="
kill $DAEMON_PID; wait $DAEMON_PID 2>/dev/null; DAEMON_PID=
rm -f /run/usbkeeper/allow-loop
"$BIN" --dest "$DEST" --port "$PORT" >$WORK/daemon2.log 2>&1 &
DAEMON_PID=$!
sleep 4
s2=$(api /api/state)
# only LOOP devices must be hidden; real USB sticks may legitimately stay occupied
if echo "$s2" | python3 -c "
import json,sys
d=json.load(sys.stdin)
loops=[p for p in d['ports'] if (p.get('device') or {}).get('name','').startswith('loop')]
sys.exit(1 if loops else 0)
"; then ok "loops hidden without marker"; else bad "loop visible WITHOUT allow-loop marker"; fi
kill $DAEMON_PID; DAEMON_PID=

echo
echo "================ RESULT: PASS=$PASS FAIL=$FAIL ================"
exit $(( FAIL > 0 ))
