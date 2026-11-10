#!/bin/sh
# openssc-launch — desktop launcher for openssc.
#
# A desktop session has no terminal, so a failed connect would otherwise leave
# no trace. MVS is the binary default (--no-mvs opts out); the wrapper keeps stderr in a
# per-user log, and puts the last line in a desktop notification when the client
# exits non-zero. Host selection: SS_HOST from the environment, or a file
# $XDG_CONFIG_HOME/openssc/config containing a line such as
#
#     SS_HOST=192.168.3.114
#
# Extra arguments are passed through, for example:
#     openssc-launch --scale 0.5
set -u

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -x "$SELF_DIR/openssc" ]; then
  BIN=$SELF_DIR/openssc
else
  BIN=${SS_GUI:-openssc}
fi

CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/openssc/config
if [ -r "$CONFIG" ]; then
  # shellcheck disable=SC1090
  . "$CONFIG"
fi
HOST=${SS_HOST:-mm.local}

# Raw evdev is the only Linux backend that delivers pinch/rotate (winit's
# Wayland backend exposes no pinch gesture). macOS has its native winit path
# and --evdev is Linux-only, so add it there only.
if [ "$(uname -s)" = "Linux" ]; then
  set -- --evdev "$@"
fi

STATE=${XDG_STATE_HOME:-$HOME/.local/state}/openssc
mkdir -p "$STATE" 2>/dev/null || true
LOG=$STATE/openssc.log

"$BIN" --host "$HOST" "$@" 2>>"$LOG"
rc=$?
if [ "$rc" -ne 0 ]; then
  echo "$(date -Is) openssc exited $rc" >>"$LOG" 2>/dev/null || true
  if command -v notify-send >/dev/null 2>&1; then
    tail -n 1 "$LOG" 2>/dev/null | notify-send -a openssc \
      "openssc exited ($rc)" 2>/dev/null || true
  fi
fi
exit "$rc"
