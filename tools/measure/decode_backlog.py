#!/usr/bin/env python3
"""Turn `SS_LATDBG=1` client logs into a decode-backlog report.

`SS_LATDBG=1 openssc ...` makes the session reader stamp each update's arrival
(`ARR`) and the decoder stamp each finished batch (`DEC updates=N ... last_arr=`),
with `PAINT` lines when the UI presents. The gap between an update arriving and
the picture being ready is the backlog: if it grows during a scroll, the decoder
is losing ground and the picture plays the scroll back late, however fast the
network is.

    SS_LATDBG=1 ./target/release/openssc --host mm.local --mvs > /tmp/dbg.log 2>&1 &
    # ... scroll ...
    python3 tools/measure/decode_backlog.py /tmp/dbg.log

Reads both log shapes: the per-batch `DEC ... last_arr=` lines used now, and the
older per-update `ARR`/`DEC` pairs (paired FIFO-wise).

What to look for, from measurements on the lab pair (see docs/testing.md):

    p50 < 30 ms, max < 100 ms      decode keeps up; the picture is real-time
    p50 in the hundreds of ms      the decoder is behind; fix decode, not the net

A still screen has no updates at all, so run a scroll while capturing.
"""

import re
import sys
import statistics as st

BATCH = re.compile(
    r"\[\s*(\d+) ms\] DEC updates=(\d+) rects=(\d+) first_arr=(\d+) last_arr=(\d+) took=([\d.]+) ms")
LEGACY_ARR = re.compile(r"\[\s*(\d+) ms\] ARR")
LEGACY_DEC = re.compile(r"\[\s*(\d+) ms\] DEC rects=(\d+) took=([\d.]+) ms")
PAINT = re.compile(r"\[\s*(\d+) ms\] PAINT")


def parse(path):
    batches, legacy_arr, legacy_dec, paints = [], [], [], []
    for line in open(path, errors="replace"):
        m = BATCH.search(line)
        if m:
            batches.append(dict(end=int(m.group(1)), updates=int(m.group(2)),
                                first_arr=int(m.group(4)), last_arr=int(m.group(5)),
                                took=float(m.group(6))))
            continue
        m = LEGACY_ARR.search(line)
        if m:
            legacy_arr.append(int(m.group(1)))
            continue
        m = LEGACY_DEC.search(line)
        if m:
            legacy_dec.append((int(m.group(1)), int(m.group(2)), float(m.group(3))))
            continue
        m = PAINT.search(line)
        if m:
            paints.append(int(m.group(1)))
    return batches, legacy_arr, legacy_dec, paints


def pct(vals, p):
    if not vals:
        return float('nan')
    k = min(len(vals) - 1, int(p / 100.0 * len(vals)))
    return sorted(vals)[k]


def report(ages, label):
    if not ages:
        return
    print("  backlog (%s): p50 %4.0f  p90 %4.0f  max %4.0f ms  (n=%d)"
          % (label, pct(ages, 50), pct(ages, 90), max(ages), len(ages)))


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    total = []
    for path in sys.argv[1:]:
        batches, arr, dec, paints = parse(path)
        print(f"{path}:")
        if batches:
            # Age of the newest content in the batch when it finished decoding.
            ages = [b['end'] - b['last_arr'] for b in batches]
            span = [b['last_arr'] - b['first_arr'] for b in batches]
            report(ages, "newest content at decode end")
            print("  batch arrival window: p50 %4.0f  max %4.0f ms   batch size p50 %d updates"
                  % (pct(span, 50), max(span), pct([b['updates'] for b in batches], 50)))
            total.extend(ages)
            if paints:
                lags = []
                for p in paints:
                    prev = [b for b in batches if b['end'] <= p]
                    if prev:
                        lags.append(p - prev[-1]['last_arr'])
                if lags:
                    print("  paint lag vs newest content: p50 %4.0f  max %4.0f ms"
                          % (pct(lags, 50), max(lags)))
        elif dec and arr:
            n = min(len(arr), len(dec))
            ages = [dec[i][0] - arr[i] for i in range(n)]
            report(ages, "DEC-ARR, FIFO pairs")
            total.extend(ages)
        else:
            print("  no ARR/DEC lines — was the client started with SS_LATDBG=1?")
    if len(sys.argv) > 2:
        print()
        report(total, "all runs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
