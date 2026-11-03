#!/bin/bash
# Capture traffic for one experiment and run the given command under capture.
#
# Usage:
#   scripts/capture.sh <capture-id> -- <command> [args...]
#   scripts/capture.sh 20261103-baseline-probe -- python3 tools/probe/probe_handshake.py
#
# The pcap is written to captures/<capture-id>.pcap and is intentionally
# gitignored. Only lab traffic (host mm.local, TCP/5900) is captured.

set -uo pipefail

ID="${1:?usage: capture.sh <capture-id> -- <command> [args...]}"
shift
if [[ "${1:-}" == "--" ]]; then
    shift
fi
if [[ $# -lt 1 ]]; then
    echo "error: no command given" >&2
    exit 2
fi

cd "$(dirname "$0")/.." || exit 1
mkdir -p captures
PCAP="captures/${ID}.pcap"

# Start tcpdump in the background. -U writes packets as they arrive so a crash
# still leaves a usable capture.
sudo tcpdump -i en0 -s 0 -U -w "$PCAP" 'host mm.local and tcp port 5900' &
TPID=$!
sleep 1

"$@"
RC=$?

sleep 1
# tcpdump catches SIGINT and flushes its buffers.
sudo pkill -INT -f "tcpdump .*${ID}.pcap" 2>/dev/null || true
wait "$TPID" 2>/dev/null || true

sudo chown "$(id -u):$(id -g)" "$PCAP" 2>/dev/null || true
echo "capture: $PCAP ($(stat -f%z "$PCAP" 2>/dev/null || echo '?') bytes)" >&2
exit "$RC"
