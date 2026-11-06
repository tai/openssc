#!/bin/sh
# ss-clone-launch — desktop launcher for ss-gui.
#
# A desktop session has no terminal, so a failed connect would otherwise leave
# no trace. This wrapper always uses the fast path (--mvs), keeps stderr in a
# per-user log, and puts the last line in a desktop notification when the client
# exits non-zero. Host selection: SS_HOST from the environment, or a file
# $XDG_CONFIG_HOME/ss-clone/config containing a line such as
#
#     SS_HOST=192.168.3.114
#
# Extra arguments are passed through, for example:
#     ss-clone-launch --scale 0.5
set -u

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -x "$SELF_DIR/ss-gui" ]; then
  BIN=$SELF_DIR/ss-gui
else
  BIN=${SS_GUI:-ss-gui}
fi

CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/ss-clone/config
if [ -r "$CONFIG" ]; then
  # shellcheck disable=SC1090
  . "$CONFIG"
fi
HOST=${SS_HOST:-mm.local}

STATE=${XDG_STATE_HOME:-$HOME/.local/state}/ss-clone
mkdir -p "$STATE" 2>/dev/null || true
LOG=$STATE/ss-gui.log

"$BIN" --host "$HOST" --mvs "$@" 2>>"$LOG"
rc=$?
if [ "$rc" -ne 0 ]; then
  echo "$(date -Is) ss-gui exited $rc" >>"$LOG" 2>/dev/null || true
  if command -v notify-send >/dev/null 2>&1; then
    tail -n 1 "$LOG" 2>/dev/null | notify-send -a ss-clone \
      "ss-clone exited ($rc)" 2>/dev/null || true
  fi
fi
exit "$rc"
