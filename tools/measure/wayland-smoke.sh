#!/usr/bin/env bash
# wayland-smoke.sh — prove that ss-gui builds, connects, paints and can present
# a window on this Linux/Wayland host, without a camera and without eyeballing.
#
#   tools/measure/wayland-smoke.sh
#   tools/measure/wayland-smoke.sh --host mm.local --expect 2560x1600
#   tools/measure/wayland-smoke.sh --skip-build            # use an existing binary
#   tools/measure/wayland-smoke.sh --no-grim               # headless check only
#
# What it does:
#   1. cargo build --release -p ss-gui
#   2. runs it against the lab server with --mvs and the built-in headless dump
#      path (--dump/--dump-after/--exit-after-dump) long enough to paint
#   3. validates the PNG with tools/measure/png_probe.py (stdlib Python only):
#      geometry, byte size, decoded pixel count, distinct colours
#   4. if a real Wayland session is already present, also captures the presented
#      window with grim and prints the path (best effort; never a pass condition)
#   5. requires a clean exit (status 0). In grim mode the window is closed
#      through the compositor after the capture, so the normal teardown runs.
#
# Session choice:
#   * If $WAYLAND_DISPLAY names a live socket under $XDG_RUNTIME_DIR, use it.
#     That is the real target and the only mode where grim can see the window.
#   * Otherwise start a private headless weston on its own socket. This makes the
#     check reproducible over ssh/CI when no compositor is running; grim is
#     skipped because weston's headless backend has no captured output.
#   The ssh route needs WAYLAND_DISPLAY and XDG_RUNTIME_DIR exported; running
#   from the Hyprland session already has them.
#
# It proves: release build, auth + session, MVS decode, at least one painted
# framebuffer, a non-trivial dumped image of the expected size, a clean exit, and
# (with grim) that the compositor has a mapped ss-gui window. It does NOT prove
# input, gestures, clipboard, file copy, frame rate, latency, colour accuracy, or
# that the image is the current remote screen (a stale frame would pass).
#
# Exit status: 0 only if every requested check passes; 1 on failure with a
# message and the client log; 2 on bad usage. Dumps/logs stay under --out (a
# temp directory by default) and are never committed.

set -u

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

HOST=${SS_HOST:-mm.local}
PORT=5900
REMOTE_USER=tai
PASSWORD=${SS_PASSWORD:?set SS_PASSWORD}
OUT=${TMPDIR:-/tmp}/ss-wayland-smoke
SECS=30
DUMP_AFTER=6
EXPECT=2560x1600
MIN_COLOURS=8
SKIP_BUILD=0
GRIM=auto                       # auto | off
BIN=${SS_GUI_BIN:-$ROOT/target/release/ss-gui}

usage() {
  sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

die() {
  echo "FAIL: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case $1 in
    --host) HOST=$2; shift 2;;
    --port) PORT=$2; shift 2;;
    --user) REMOTE_USER=$2; shift 2;;
    --password) PASSWORD=$2; shift 2;;
    --out) OUT=$2; shift 2;;
    --secs) SECS=$2; shift 2;;
    --dump-after) DUMP_AFTER=$2; shift 2;;
    --expect) EXPECT=$2; shift 2;;
    --min-colours) MIN_COLOURS=$2; shift 2;;
    --bin) BIN=$2; shift 2;;
    --skip-build) SKIP_BUILD=1; shift;;
    --no-grim) GRIM=off; shift;;
    -h|--help) usage;;
    *) echo "unknown argument: $1" >&2; usage;;
  esac
done

command -v python3 >/dev/null 2>&1 || die "python3 is required (stdlib only)"
[ -f "$HERE/png_probe.py" ] || die "missing $HERE/png_probe.py"

mkdir -p "$OUT"
DUMP=$OUT/ss-gui.png
LOG=$OUT/ss-gui.log
GRIM_PNG=$OUT/ss-gui-grim.png
rm -f "$DUMP" "$LOG" "$GRIM_PNG"

# ---------------------------------------------------------------- build
if [ "$SKIP_BUILD" -eq 0 ]; then
  command -v cargo >/dev/null 2>&1 || \
    die "cargo not found; export PATH=\"\$HOME/.cargo/bin:\$PATH\""
  echo "== build: cargo build --release -p ss-gui"
  if ! (cd "$ROOT" && cargo build --release -p ss-gui); then
    cat >&2 <<'EOF'
FAIL: release build failed.

If the error mentions `grab::` (for example "cannot find module or crate
`grab`" in crates/ss-gui/src/main.rs), the Linux platform seam has not landed
yet. That is a crates/ problem owned by another change, not this harness; do
not edit crates/ to get the smoke test green. Re-run after it lands.

A Linux host also needs a C toolchain and pkg-config for the winit/softbuffer
build. See "Linux/Wayland" in README.md and docs/testing.md.
EOF
    exit 1
  fi
else
  [ -x "$BIN" ] || die "--skip-build asked for $BIN, which is not executable"
fi
[ -x "$BIN" ] || die "client binary is not executable: $BIN"

# ------------------------------------------------------- compositor
SESSION_KIND=
WESTON_PID=
CLIENT_PID=

wayland_socket_ok() {
  [ -n "${WAYLAND_DISPLAY:-}" ] || return 1
  case $WAYLAND_DISPLAY in
    /*) [ -S "$WAYLAND_DISPLAY" ];;
    *) [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ];;
  esac
}

cleanup() {
  if [ -n "${CLIENT_PID:-}" ] && kill -0 "$CLIENT_PID" 2>/dev/null; then
    kill -TERM "$CLIENT_PID" 2>/dev/null || true
    sleep 0.5
    kill -KILL "$CLIENT_PID" 2>/dev/null || true
  fi
  if [ -n "${WESTON_PID:-}" ] && kill -0 "$WESTON_PID" 2>/dev/null; then
    kill -TERM "$WESTON_PID" 2>/dev/null || true
    wait "$WESTON_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

if wayland_socket_ok; then
  SESSION_KIND=existing
  echo "== compositor: existing Wayland session (WAYLAND_DISPLAY=$WAYLAND_DISPLAY)"
elif command -v weston >/dev/null 2>&1; then
  SESSION_KIND=headless
  export XDG_RUNTIME_DIR="$OUT/runtime"
  mkdir -p "$XDG_RUNTIME_DIR"
  chmod 700 "$XDG_RUNTIME_DIR"
  export WAYLAND_DISPLAY=ss-smoke-$$
  echo "== compositor: no session; starting private headless weston ($WAYLAND_DISPLAY)"
  weston --backend=headless-backend.so --socket="$WAYLAND_DISPLAY" \
         --idle-time=0 --no-config >"$OUT/weston.log" 2>&1 &
  WESTON_PID=$!
  for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
    kill -0 "$WESTON_PID" 2>/dev/null || die "weston exited; see $OUT/weston.log"
    sleep 0.1
  done
  [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || \
    die "headless weston did not create a socket; see $OUT/weston.log"
else
  die "no Wayland session ($WAYLAND_DISPLAY) and no weston to start one"
fi

do_grim=0
if [ "$GRIM" != off ] && [ "$SESSION_KIND" = existing ] && command -v grim >/dev/null 2>&1; then
  do_grim=1
fi

# ------------------------------------------------------------- client
ARGS=(--host "$HOST" --port "$PORT" --user "$REMOTE_USER" \
      --password "$PASSWORD" --mvs \
      --dump "$DUMP" --dump-after "$DUMP_AFTER")
if [ "$do_grim" -eq 1 ]; then
  # Keep the window alive for grim; close it through the compositor afterwards,
  # so the normal teardown runs and its exit status is still checked.
  ARGS+=(--exit-after-secs "$SECS")
else
  # The headless path: dump, then let the client exit by itself.
  ARGS+=(--exit-after-dump --exit-after-secs "$SECS")
fi

echo "== client: $BIN ${ARGS[*]}"
"$BIN" "${ARGS[@]}" >"$LOG" 2>&1 &
CLIENT_PID=$!

wait_for_dump() {
  local limit=$1 n=0
  while :; do
    # "wrote <path>" is printed only after write_png has returned, so the file
    # cannot be read half-written.
    if [ -s "$DUMP" ] && grep -Fq "wrote $DUMP" "$LOG" 2>/dev/null; then
      return 0
    fi
    kill -0 "$CLIENT_PID" 2>/dev/null || return 1
    n=$((n + 1))
    [ "$n" -ge $((limit * 5)) ] && return 1
    sleep 0.2
  done
}

window_info() {
  command -v hyprctl >/dev/null 2>&1 || return 1
  hyprctl -j clients 2>/dev/null | python3 -c '
import json, sys
try:
    clients = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for c in clients:
    cls = c.get("class") or c.get("initialClass") or ""
    title = c.get("title") or ""
    if cls == "ss-gui" or title.startswith("ss-clone"):
        if not c.get("mapped", True) or c.get("hidden", False):
            continue
        addr = c.get("address") or ""
        x, y = c.get("at", [0, 0])
        w, h = c.get("size", [0, 0])
        if addr and w > 0 and h > 0:
            print("%s %d,%d %dx%d" % (addr, x, y, w, h))
            sys.exit(0)
sys.exit(1)'
}

# Watch the process even when the dump arrives early; a hung window fails.
client_deadline=$((SECS + 15))
started=$(date +%s)
if ! wait_for_dump "$SECS"; then
  kill -0 "$CLIENT_PID" 2>/dev/null && { kill -TERM "$CLIENT_PID"; sleep 0.5; kill -KILL "$CLIENT_PID" 2>/dev/null; }
  wait "$CLIENT_PID" 2>/dev/null
  echo "--- client log (tail) ---" >&2
  tail -30 "$LOG" >&2 || true
  die "no dump written within ${SECS}s. Check that $HOST:$PORT is reachable and the Wayland variables are exported."
fi

addr=       # compositor window address, used for a clean close
geom=       # grim geometry
if [ "$do_grim" -eq 1 ]; then
  echo "== grim: looking for the ss-gui window"
  info=
  for _ in $(seq 1 50); do
    info=$(window_info 2>/dev/null) && break
    sleep 0.2
  done
  if [ -n "$info" ]; then
    addr=${info%% *}
    geom=${info#* }
    if grim -g "$geom" "$GRIM_PNG" 2>"$OUT/grim.err"; then
      echo "   window $geom -> $GRIM_PNG"
    else
      echo "   warning: grim -g '$geom' failed (see $OUT/grim.err); falling back to the full output"
    fi
  fi
  if [ ! -s "$GRIM_PNG" ] && command -v grim >/dev/null 2>&1; then
    if grim "$GRIM_PNG" 2>"$OUT/grim.err"; then
      echo "   full output (window geometry unavailable) -> $GRIM_PNG"
    else
      echo "   warning: grim failed (see $OUT/grim.err); the dump check still stands"
    fi
  fi
fi

terminated=0
if [ -n "$addr" ] && command -v hyprctl >/dev/null 2>&1; then
  echo "== close: hyprctl window close for $addr"
  # Hyprland 0.56 takes a Lua dispatcher; older releases take
  # `closewindow address:...`. Try both, and treat an error-looking reply as a
  # failure (hyprctl exits 0 even when the dispatcher rejects the syntax).
  out=$(hyprctl dispatch "hl.dsp.window.close({address=\"$addr\"})" 2>&1) || true
  case "$out" in
    *error*|*Error*|*expected*|*unknown*) out=$(hyprctl dispatch closewindow "address:$addr" 2>&1) || true;;
  esac
  case "$out" in
    *error*|*Error*|*expected*|*unknown*|"")
      echo "   warning: compositor close failed; sending SIGTERM"
      kill -TERM "$CLIENT_PID" 2>/dev/null || true
      terminated=1;;
  esac
elif [ "$do_grim" -eq 1 ]; then
  echo "== close: no compositor window address; sending SIGTERM (clean exit not checked)"
  kill -TERM "$CLIENT_PID" 2>/dev/null || true
  terminated=1
fi

# Wait for the client to finish. A genuinely hung client is killed at the
# deadline and reported as a failure.
timed_out=0
while kill -0 "$CLIENT_PID" 2>/dev/null; do
  now=$(date +%s)
  if [ $((now - started)) -ge "$client_deadline" ]; then
    kill -TERM "$CLIENT_PID" 2>/dev/null || true
    sleep 0.5
    kill -KILL "$CLIENT_PID" 2>/dev/null || true
    timed_out=1
    break
  fi
  sleep 0.2
done
wait "$CLIENT_PID" 2>/dev/null
client_rc=$?
CLIENT_PID=

if [ "$timed_out" -eq 1 ]; then
  echo "--- client log (tail) ---" >&2
  tail -30 "$LOG" >&2 || true
  die "client did not exit within ${client_deadline}s after the dump"
fi
if [ "$terminated" -eq 1 ]; then
  echo "warning: client was stopped with SIGTERM; clean exit was not checked"
  client_rc="stopped with SIGTERM (clean exit not checked)"
elif [ "$client_rc" -ne 0 ]; then
  echo "--- client log (tail) ---" >&2
  tail -30 "$LOG" >&2 || true
  if [ "$client_rc" -eq 139 ]; then
    cat >&2 <<'EOF'
FAIL: the client wrote the dump but segfaulted on exit (status 139, SIGSEGV).

This was a softbuffer Wayland teardown bug, fixed in 4598e4c. If it is back,
the crash is in drop of WaylandDisplayImpl after winit tears down the
connection; that is a crates/ss-gui lifetime problem, not the smoke harness.
Do not edit crates/ here; report it to the platform owner.
EOF
  else
    echo "FAIL: the client exited $client_rc after writing the dump." >&2
  fi
  exit 1
fi

# ----------------------------------------------------------- validate
[ -s "$DUMP" ] || {
  echo "--- client log (tail) ---" >&2
  tail -30 "$LOG" >&2 || true
  die "client produced no dump at $DUMP"
}

echo "== validate: python3 tools/measure/png_probe.py"
if ! python3 "$HERE/png_probe.py" "$DUMP" \
      --expect "$EXPECT" --min-colours "$MIN_COLOURS"; then
  echo "--- client log (tail) ---" >&2
  tail -30 "$LOG" >&2 || true
  die "the dump exists but is not a usable painted frame: $DUMP"
fi

echo
echo "PASS — Wayland smoke test"
echo "  binary:  $BIN"
echo "  session: $SESSION_KIND (${WAYLAND_DISPLAY:-none})"
echo "  dump:    $DUMP"
echo "  log:     $LOG"
[ -s "$GRIM_PNG" ] && echo "  grim:    $GRIM_PNG"
[ -s "$GRIM_PNG" ] || echo "  grim:    not captured (no real session, no grim, or it failed)"
echo "  exit:    $client_rc"
echo
echo "Proved: release build; TCP/auth/session to $HOST:$PORT; MVS decode; at least"
if [ "$terminated" -eq 1 ]; then
  echo "        one painted frame of $EXPECT with non-trivial content;"
  echo "        clean exit NOT checked (the client was stopped with SIGTERM);"
else
  echo "        one painted frame of $EXPECT with non-trivial content; clean exit;"
fi
echo "        (with grim) a mapped ss-gui window seen by the compositor."
echo "Not proved: input, gestures, clipboard/file copy, frame rate, latency, colour"
echo "        fidelity, or that the frame is current. No camera is involved."
exit 0
