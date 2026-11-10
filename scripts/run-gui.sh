#!/bin/sh
# Launch the live client. Override the host with SS_HOST.
#   SS_HOST=192.168.3.114 ./scripts/run-gui.sh
#   ./scripts/run-gui.sh --evdev        # Linux raw multitouch
set -eu
cd "$(dirname "$0")/.."
: "${SS_HOST:=mm.local}"
exec cargo run --release -p openssc -- --host "$SS_HOST" "$@"
