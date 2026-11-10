#!/bin/sh
# openssc-launch — desktop launcher for openssc.
#
# On Linux a desktop launch opens the connection manager: pick a saved target,
# edit its host/port/user if needed, type the password (never stored) and press
# Connect. The manager spawns the same binary in direct-connect mode and stays
# open for the next target. The targets live in
# ${XDG_CONFIG_HOME:-$HOME/.config}/openssc/targets.conf.
#
# A desktop session has no terminal, so stderr goes to a per-user log and the
# last line of a failed manager/child session becomes a notification.
#
# Direct CLI use is unchanged: `openssc --host mm.local` connects immediately.
# Extra arguments go to the manager's own parser; a Connect child uses the
# manager defaults (MVS is the binary default, plus --evdev on Linux).
set -u

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -x "$SELF_DIR/openssc" ]; then
  BIN=$SELF_DIR/openssc
else
  BIN=${SS_GUI:-openssc}
fi

STATE=${XDG_STATE_HOME:-$HOME/.local/state}/openssc
mkdir -p "$STATE" 2>/dev/null || true
LOG=$STATE/openssc.log

# Linux/Wayland is the manager target. macOS uses its own bundle and keeps the
# direct CLI path; this wrapper is not installed there.
if [ "$(uname -s)" = "Linux" ]; then
  set -- --manager "$@"
fi

"$BIN" "$@" 2>>"$LOG"
rc=$?
if [ "$rc" -ne 0 ]; then
  echo "$(date -Is) openssc exited $rc" >>"$LOG" 2>/dev/null || true
  if command -v notify-send >/dev/null 2>&1; then
    tail -n 1 "$LOG" 2>/dev/null | notify-send -a openssc \
      "openssc exited ($rc)" 2>/dev/null || true
  fi
fi
exit "$rc"
