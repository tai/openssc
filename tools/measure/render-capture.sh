#!/usr/bin/env bash
# render-capture.sh — present the same remote screen through one filter at one
# window size, dump the 1:1 framebuffer, capture the actual window with grim,
# and report the draw times. Used for the before/after evidence in
# docs/testing.md's render-quality section.
#
#   tools/measure/render-capture.sh --filter nearest --size 1280x800 --out artifacts/render/nearest-1280
#   tools/measure/render-capture.sh --filter auto    --size 1600x1000 --out artifacts/render/box-1600
#
# This is a Hyprland lab harness: it uses `hyprctl` only to float/resize/place
# the window so grim captures a known rectangle. The client itself does not
# depend on Hyprland. For a compositor-agnostic check use wayland-smoke.sh.
#
# The run is serialized on /tmp/openssc-gui.lock with `flock -o` (the lock fd
# is close-on-exec so wl-copy cannot inherit and hold it), refuses to start if
# any openssc is already running, uses --no-clipboard (clipboard is another
# agent's test surface), and is bounded by --exit-after-secs plus timeout.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
HOST=${SS_HOST:-mm.local}
FILTER=auto
SIZE=1280x800
OUT=
WORKSPACE=
SECS=40
SKIP_BUILD=0
BIN=${SS_GUI_BIN:-$ROOT/target/release/openssc}

usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}
die() { echo "FAIL: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case $1 in
    --filter) FILTER=$2; shift 2;;
    --size) SIZE=$2; shift 2;;
    --out) OUT=$2; shift 2;;
    --workspace) WORKSPACE=$2; shift 2;;
    --secs) SECS=$2; shift 2;;
    --host) HOST=$2; shift 2;;
    --bin) BIN=$2; shift 2;;
    --skip-build) SKIP_BUILD=1; shift;;
    -h|--help) usage;;
    *) echo "unknown argument: $1" >&2; usage;;
  esac
done
[ -n "$OUT" ] || die "--out is required (put it under artifacts/)"
case $FILTER in auto|nearest|box|bilinear) ;; *) die "--filter must be auto, nearest, box or bilinear";; esac
case $SIZE in *x*) ;; *) die "--size must be WxH";; esac

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

command -v flock >/dev/null 2>&1 || die "flock is required"
command -v hyprctl >/dev/null 2>&1 || die "hyprctl is required for window placement"
command -v grim >/dev/null 2>&1 || die "grim is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"

# One client at a time across all agents. -o is important: wl-copy and other
# children must not inherit the lock fd or the lock can outlive this script.
exec 9>/tmp/openssc-gui.lock
flock -o -w 600 9 || die "could not acquire /tmp/openssc-gui.lock"
if pgrep -x openssc >/dev/null; then
  die "another openssc is already running; wait for it to exit"
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
  command -v cargo >/dev/null 2>&1 || die "cargo not found; export PATH=\"\$HOME/.cargo/bin:\$PATH\""
  (cd "$ROOT" && cargo build --release -p openssc) || die "release build failed"
fi
[ -x "$BIN" ] || die "no client binary at $BIN"

LOG=$OUT/client.log
DUMP=$OUT/dump.png
GRIM=$OUT/grim.png
rm -f "$LOG" "$DUMP" "$GRIM"

# Use the exact requested size for both the client request and the Hyprland
# resize, so a floating window and the screenshot agree.
SS_DRAW_TIMING=1 "$BIN" --host "$HOST" --mvs --no-clipboard --debug-input \
  --filter "$FILTER" --window-size "$SIZE" \
  --dump "$DUMP" --dump-after 8 --exit-after-secs "$SECS" >"$LOG" 2>&1 &
pid=$!
CLIENT_PID=$pid
cleanup() {
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    sleep 0.5
    kill -KILL "$pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

addr=
for _ in $(seq 1 100); do
  kill -0 "$pid" 2>/dev/null || break
  addr=$(PID=$pid hyprctl -j clients | PID=$pid python3 -c 'import json,sys,os
for c in json.load(sys.stdin):
    if c.get("pid")==int(os.environ["PID"]): print(c["address"]); break' 2>/dev/null)
  [ -n "$addr" ] && break
  sleep 0.1
done
[ -n "$addr" ] || { tail -20 "$LOG" >&2 || true; die "client window did not appear"; }

if [ -n "$WORKSPACE" ]; then
  hyprctl dispatch "hl.dsp.window.move({ workspace = \"$WORKSPACE\", window = \"address:$addr\" })" >/dev/null 2>&1 || true
  sleep 1
fi
hyprctl dispatch "hl.dsp.focus({ window = \"address:$addr\" })" >/dev/null 2>&1 || true
hyprctl dispatch "hl.dsp.window.float({ action = \"enable\", window = \"address:$addr\" })" >/dev/null 2>&1 || true
hyprctl dispatch "hl.dsp.window.resize({ x = ${SIZE%x*}, y = ${SIZE#*x}, window = \"address:$addr\" })" >/dev/null 2>&1 || true
sleep 1

# Centre the window on its own monitor. A 1600x1000 window placed from the
# monitor origin can spill onto the monitor below, which would make grim cross
# a boundary and compare two different outputs.
hyprctl -j clients >"$OUT/clients.json" 2>/dev/null || true
hyprctl -j monitors >"$OUT/monitors.json" 2>/dev/null || true
centre=$(python3 - "$OUT/clients.json" "$OUT/monitors.json" "$pid" <<'PY'
import json, sys
try:
    clients = json.load(open(sys.argv[1]))
    monitors = json.load(open(sys.argv[2]))
except Exception:
    sys.exit(0)
pid = int(sys.argv[3])
for c in clients:
    if c.get("pid") != pid:
        continue
    mid = c.get("monitor")
    m = next((m for m in monitors if m.get("id") == mid), None)
    if not m:
        sys.exit(0)
    scale = m.get("scale") or 1
    mw = m.get("width", 0) / scale
    mh = m.get("height", 0) / scale
    w, h = c.get("size", [0, 0])
    print("%d %d" % (m.get("x", 0) + (mw - w) / 2, m.get("y", 0) + (mh - h) / 2))
    break
PY
)
if [ -n "$centre" ]; then
  set -- $centre
  hyprctl dispatch "hl.dsp.window.move({ x = $1, y = $2, window = \"address:$addr\" })" >/dev/null 2>&1 || true
  sleep 1
fi

# Centre on whichever monitor the window is on is left to Hyprland; the
# geometry is read back below so grim captures exactly what was presented.

for _ in $(seq 1 150); do
  grep -Fq "wrote $DUMP" "$LOG" 2>/dev/null && break
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.1
done
[ -s "$DUMP" ] || { tail -30 "$LOG" >&2 || true; die "no 1:1 dump was written"; }

# Park the pointer in the window body (avoids edge-triggered docks/tooltips),
# let the compositor settle, then capture the actual presented pixels.
size_w=${SIZE%x*}
size_h=${SIZE#*x}
cur=$(PID=$pid hyprctl -j clients | PID=$pid python3 -c 'import json,sys,os
for c in json.load(sys.stdin):
    if c.get("pid")==int(os.environ["PID"]):
        x,y=c.get("at",[0,0]); print("%d %d"%(x,y)); break')
set -- $cur
if [ $# -eq 2 ]; then
  hyprctl dispatch movecursor "$(( $1 + size_w / 2 ))" "$(( $2 + size_h / 2 ))" >/dev/null 2>&1 || true
fi
sleep 3
info=$(PID=$pid hyprctl -j clients | PID=$pid python3 -c 'import json,sys,os
for c in json.load(sys.stdin):
    if c.get("pid")==int(os.environ["PID"]):
        x,y=c.get("at",[0,0]); w,h=c.get("size",[0,0]); print("%d,%d %dx%d"%(x,y,w,h)); break')
[ -n "$info" ] || die "window disappeared before grim"
grim -g "$info" "$GRIM" 2>"$OUT/grim.err" || die "grim failed; see $OUT/grim.err"
printf '%s\n' "$info" >"$OUT/geometry.txt"
grep -E 'window:|render: |draw took|wrote .*png' "$LOG" >"$OUT/draw.log" || true

echo "captured $FILTER at $SIZE"
echo "  dump: $DUMP"
echo "  grim: $GRIM ($info)"
echo "  log:  $LOG"
python3 - "$OUT/draw.log" <<'PY'
import re, statistics, sys
vals=[float(m.group(1)) for m in re.finditer(r'draw took ([0-9.]+) ms', open(sys.argv[1]).read())]
if vals:
    vals.sort(); n=len(vals)
    p95=vals[min(n-1, round(0.95*(n-1)))]
    print(f"  draw: n={n} p50={statistics.median(vals):.2f} ms p95={p95:.2f} ms max={vals[-1]:.2f} ms")
else:
    print("  draw: no samples (client printed nothing)")
PY

hyprctl dispatch "hl.dsp.window.close({ window = \"address:$addr\" })" >/dev/null 2>&1 || true
for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
if kill -0 "$pid" 2>/dev/null; then
  echo "note: close did not complete; sending SIGTERM" >&2
  kill -TERM "$pid" 2>/dev/null || true
fi
wait "$pid" 2>/dev/null
rc=$?
CLIENT_PID=
if pgrep -x openssc >/dev/null; then
  echo "note: another openssc is running; this script did not start it" >&2
fi
exit "$rc"
