#!/bin/zsh
# camera_ab.sh — record one client presenting a synthetic scroll, for the
# two-screen visual-latency measurement.
#
# The camera films the remote machine and the local machine at once. This script
# does the local half mechanically: start the client, put its window in a known
# place, scroll the page to the top, film four identical scroll bursts, and
# convert the recording to the raw grayscale stream parity.py reads.
#
#   ./camera_ab.sh --client clone --label c1      # ss-gui
#   ./camera_ab.sh --client app   --label a1      # Screen Sharing.app
#
# Then compare the runs:
#
#   python3 tools/measure/parity.py clone=c1.raw clone=c2.raw app=a1.raw app=a2.raw
#
# Interleave the two clients (clone, app, clone, app) so that Wi-Fi and page
# state drift cannot bias one side.
#
# Requirements (macOS harness only; the client's real target is Linux): an
# ffmpeg with avfoundation (`/opt/local/bin/ffmpeg` by default), the `scroll`
# helper built from scroll.c in this directory, and Accessibility permission for
# the terminal so synthetic events and window moves are allowed.

set -u

HERE=${0:A:h}
OUT=${TMPDIR:-/tmp}/ss-measure
CLIENT=clone
LABEL=run
BURSTS=4
BURST_PX=300
STEPS=20
GAP=1.4
WIN_X=200; WIN_Y=120; WIN_W=1280; WIN_H=800
SCROLL_X=700; SCROLL_Y=500
SECS=11
FFMPEG=/opt/local/bin/ffmpeg
REMOTE=          # e.g. tai@mm.local, only needed with --url
URL=             # reset the remote page to this before recording
BIN=             # client binary, defaults per --client

usage() {
  sed -n '2,30p' $0 | sed 's/^# \{0,1\}//'
  exit 2
}

while [[ $# -gt 0 ]]; do
  case $1 in
    --client) CLIENT=$2; shift 2;;
    --label) LABEL=$2; shift 2;;
    --bursts) BURSTS=$2; shift 2;;
    --burst-px) BURST_PX=$2; shift 2;;
    --gap) GAP=$2; shift 2;;
    --out) OUT=$2; shift 2;;
    --url) URL=$2; shift 2;;
    --remote) REMOTE=$2; shift 2;;
    --secs) SECS=$2; shift 2;;
    --bin) BIN=$2; shift 2;;
    -h|--help) usage;;
    *) echo "unknown argument: $1"; usage;;
  esac
done

mkdir -p "$OUT"
SCROLL=$OUT/scroll
if [[ ! -x $SCROLL ]]; then
  # built out of tree: the repo tracks source, not binaries
  clang -O2 -o "$SCROLL" "$HERE/scroll.c" -framework ApplicationServices || exit 1
fi

if [[ -z $BIN ]]; then
  case $CLIENT in
    clone) BIN=${SS_GUI_BIN:-./target/release/ss-gui};;
    app) BIN="";;                     # started through the vnc:// URL scheme
    *) echo "--client must be clone or app"; exit 2;;
  esac
fi

stop_clients() {
  pkill -f 'ss-gui' 2>/dev/null
  osascript -e 'tell application "Screen Sharing" to quit' >/dev/null 2>&1
  sleep 2
}

focus() {
  osascript -e "tell application \"System Events\" to set frontmost of process \"$1\" to true" \
            -e 'delay 0.5' >/dev/null 2>&1
}

# Both clients are pinned to the same rectangle. Screen Sharing.app preserves the
# remote aspect ratio inside the requested size, so it gets extra height to land
# on the same content width.
pin() {
  local h=$2
  osascript -e "tell application \"System Events\" to tell process \"$1\" to set position of window 1 to {$WIN_X, $WIN_Y}" \
            -e "tell application \"System Events\" to tell process \"$1\" to set size of window 1 to {$WIN_W, $h}" >/dev/null 2>&1
  sleep 1
}

if [[ -n $URL && -n $REMOTE ]]; then
  ssh -o ConnectTimeout=8 "$REMOTE" "osascript -e 'tell application \"Google Chrome\"' -e 'activate' -e 'tell active tab of front window to set URL to \"$URL\"' -e 'end tell'" >/dev/null 2>&1
  sleep 4
fi

stop_clients
if [[ $CLIENT == app ]]; then
  open "vnc://${SS_USER:-tai}@${SS_HOST:-mm.local}"
  sleep 9
  focus "Screen Sharing"
  pin "Screen Sharing" 840
else
  ( "$BIN" --host ${SS_HOST:-mm.local} --mvs --exit-after-secs $((SECS + 25)) \
      > "$OUT/$LABEL.log" 2>&1 & )
  sleep 9
  focus "ss-gui"
  pin "ss-gui" 800
fi

"$SCROLL" $SCROLL_X $SCROLL_Y 6000 40 1     # page to the top
sleep 2.5

( timeout 40 "$FFMPEG" -hide_banner -loglevel error -f avfoundation -framerate 30 \
    -video_size 640x480 -i "0:none" -t "$SECS" -y "$OUT/vis-$LABEL.mov" >/dev/null 2>&1 ) &
sleep 1.5
for ((i = 0; i < BURSTS; i++)); do
  "$SCROLL" $SCROLL_X $SCROLL_Y $BURST_PX $STEPS -1
  sleep $GAP
done
wait

stop_clients

"$FFMPEG" -hide_banner -loglevel error -i "$OUT/vis-$LABEL.mov" \
  -vf format=gray -f rawvideo -pix_fmt gray -y "$OUT/$LABEL.raw" 2>/dev/null
if [[ ! -s $OUT/$LABEL.raw ]]; then
  echo "no recording: $OUT/$LABEL.raw is empty (camera busy or permission denied?)"
  exit 1
fi
echo "$LABEL: $(( $(stat -f%z "$OUT/$LABEL.raw") / 307200 )) frames -> $OUT/$LABEL.raw"
