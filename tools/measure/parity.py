#!/usr/bin/env python3
"""Measure how far a client window trails the remote screen, from a recording.

Two screens filmed by one camera: the remote machine on one, the local machine
running the client on the other. Both show the same page, so a scroll produces a
motion burst in each at the same instant, offset by the client's visual latency.
This script turns that into numbers.

    python3 tools/measure/parity.py clone=c1.raw clone=c2.raw app=a1.raw app=a2.raw

Input is a raw grayscale frame stream at 640x480 (see camera_ab.sh, which writes
it), or one of the listed labels repeated for several runs. `clone` and `app`
are just labels: every run with the same label is pooled.

The two windows do not have to be at the same screen position. For each kind the
bottom crop is derived automatically from where the motion actually is, and
printed so it can be pinned with `--crop` for a repeat run:

    --crop clone=320,465,345,440      # y0,y1,x0,x1 in camera pixels
    --top  45,215,240,430             # remote screen crop

Metrics, per scroll burst:

    remote      how long the motion lasts on the remote screen
    client      how long it lasts in the client window
    stretch     client / remote; ~1.0 means "presented in real time"
    head        when the client's motion starts, relative to the remote's
    tail        when the client's motion ends, relative to the remote's
    envelope    the shift that best aligns the two motion envelopes
    centroid    the same for the energy centroids

`head` says how fast the first frame appears, `tail` how long the client keeps
catching up afterwards. A client that decodes in real time has both small.

Limitations, all of which apply to both clients equally: a 30 fps camera gives
33 ms granularity, so medians matter more than any single burst; the sensor
reads the bottom of the frame later than the top (rolling shutter, ~10 ms); and
a client that paints in large jumps can produce a clipped burst whose negative
`tail` is a detection artefact rather than the window finishing early.
"""

import argparse
import statistics as st
import sys

W, H = 640, 480
FPS = 30.0


def load(path):
    data = open(path, 'rb').read()
    fs = W * H
    n = len(data) // fs
    if n == 0:
        sys.exit(f"{path}: not a raw {W}x{H} frame stream")
    return data, n


def motion(data, crop, block=16):
    """Mean absolute frame difference per frame inside `crop` (y0,y1,x0,x1)."""
    y0, y1, x0, x1 = crop
    fs = W * H
    n = len(data) // fs
    w = x1 - x0
    out = []
    for i in range(1, n):
        base, prev = i * fs, (i - 1) * fs
        tot = 0
        for y in range(y0, y1):
            o = y * W + x0
            a = data[prev + o: prev + o + w]
            b = data[base + o: base + o + w]
            tot += sum(abs(p - q) for p, q in zip(a, b))
        out.append(tot / ((y1 - y0) * w))
    return out


def heat(data, band, block=16):
    """Motion energy per block inside `band`; used to find the live region."""
    y0, y1, x0, x1 = band
    fs = W * H
    n = len(data) // fs
    cells = []
    for y in range(y0, y1 - block + 1, block):
        for x in range(x0, x1 - block + 1, block):
            tot = 0
            for i in range(1, n):
                base, prev = i * fs, (i - 1) * fs
                for yy in range(y, y + block):
                    o = yy * W + x
                    a = data[prev + o: prev + o + block]
                    b = data[base + o: base + o + block]
                    tot += sum(abs(p - q) for p, q in zip(a, b))
            cells.append((tot / ((n - 1) * block * block), y, x))
    return cells


def auto_crop(data, band, block=16, keep=0.5, min_side=48):
    """Bounding box of the blocks carrying a decent share of the motion."""
    cells = heat(data, band, block)
    if not cells:
        return band
    top = max(v for v, _, _ in cells)
    if top <= 0:
        return band
    hot = [(y, x) for v, y, x in cells if v >= keep * top]
    y0 = min(y for y, _ in hot)
    y1 = max(y for y, _ in hot) + block
    x0 = min(x for _, x in hot)
    x1 = max(x for _, x in hot) + block
    y0 = max(band[0], min(y0, band[1] - min_side))
    x0 = max(band[2], min(x0, band[3] - min_side))
    return (y0, max(y1, y0 + min_side), x0, max(x1, x0 + min_side))


def p95(v):
    return sorted(v)[min(len(v) - 1, int(0.95 * len(v)))] or 1.0


def bursts(sig, thr=0.15, gap=7, skip=10):
    """Runs above `thr` (after normalising by p95), merged across short gaps."""
    n = len(sig)
    out, i = [], skip
    while i < n:
        if sig[i] >= thr:
            last = i
            j = i
            while j < n and j - last < gap:
                if sig[j] >= thr:
                    last = j
                j += 1
            out.append((i, last))
            i = last + 1
        else:
            i += 1
    return out


def centroid(sig, i0, i1):
    w = sum(sig[i0:i1 + 1]) or 1.0
    return sum(sig[i] * i for i in range(i0, i1 + 1)) / w


def envelope_lag(top, bot, a0, a1, b0, b1, max_lag=16):
    lo = max(min(a0, b0) - 4, 0)
    hi = min(max(a1, b1) + max_lag, len(top))
    scores = {}
    for lag in range(-2, max_lag + 1):
        s, c = 0.0, 0
        for i in range(lo, hi):
            j = i + lag
            if lo <= j < hi:
                s += (top[i] - bot[j]) ** 2
                c += 1
        if c > 30:
            scores[lag] = s / c
    if not scores:
        return None
    best = min(scores, key=lambda k: scores[k])
    lag = float(best)
    if best - 1 in scores and best + 1 in scores:
        a, b, c = scores[best - 1], scores[best], scores[best + 1]
        den = a - 2 * b + c
        if den:
            lag = best + 0.5 * (a - c) / den
    return lag


def analyse(label, path, top_crop, bot_crop=None):
    data, n = load(path)
    if bot_crop is None:
        bot_crop = auto_crop(data, (int(H * 0.52), H - 2, 60, W - 70))
    top = motion(data, top_crop)
    bot = motion(data, bot_crop)
    tn = [v / p95(top) for v in top]
    bn = [v / p95(bot) for v in bot]
    bt = bursts(tn)
    bb = bursts(bn)

    rows, used = [], set()
    for a0, a1 in bt:
        cand = [(abs(b0 - a0), j) for j, (b0, _) in enumerate(bb)
                if j not in used and -3 <= b0 - a0 <= 40]
        if not cand:
            continue
        j = min(cand)[1]
        used.add(j)
        b0, b1 = bb[j]
        lag = envelope_lag(top, bot, a0, a1, b0, b1)
        rows.append(dict(
            remote=(a1 - a0) / FPS * 1000,
            client=(b1 - b0) / FPS * 1000,
            stretch=(b1 - b0) / max(a1 - a0, 1),
            head=(b0 - a0) / FPS * 1000,
            tail=(b1 - a1) / FPS * 1000,
            env=(lag / FPS * 1000) if lag is not None else None,
            cent=(centroid(bot, b0, b1) - centroid(top, a0, a1)) / FPS * 1000,
        ))
    print("%-7s %-12s %3d frames  bottom=%d,%d,%d,%d  bursts=%d"
          % (label, path.split('/')[-1], n, *bot_crop, len(rows)))
    for r in rows:
        env = "n/a" if r['env'] is None else "%.0f" % r['env']
        print("   remote %4.0f  client %4.0f ms  x%.2f  head %4.0f  tail %4.0f  env %4s  cent %4.0f"
              % (r['remote'], r['client'], r['stretch'], r['head'], r['tail'], env, r['cent']))
    return label, rows


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("runs", nargs="+", metavar="LABEL=FILE.raw",
                    help="one or more recordings; runs sharing a label are pooled")
    ap.add_argument("--top", default="45,215,240,430",
                    help="remote-screen crop y0,y1,x0,x1 (default: %(default)s)")
    ap.add_argument("--crop", action="append", default=[], metavar="LABEL=y0,y1,x0,x1",
                    help="client-window crop; derived automatically when omitted")
    args = ap.parse_args()

    top_crop = tuple(int(v) for v in args.top.split(","))
    crops = {}
    for spec in args.crop:
        kind, box = spec.split("=", 1)
        crops[kind] = tuple(int(v) for v in box.split(","))

    pooled = {}
    for run in args.runs:
        if "=" not in run:
            sys.exit(f"expected LABEL=FILE.raw, got {run!r}")
        label, path = run.split("=", 1)
        label, rows = analyse(label, path, top_crop, crops.get(label))
        pooled.setdefault(label, []).extend(rows)

    if not any(pooled.values()):
        sys.exit("no scroll bursts found — check the crops (--top/--crop)")

    print()
    for label, rows in pooled.items():
        def med(key):
            vals = [r[key] for r in rows if r[key] is not None]
            return st.median(vals) if vals else float('nan')
        print("%-7s n=%-2d  remote %4.0f  client %4.0f ms  stretch x%.2f  "
              "head %4.0f  tail %4.0f  env %4.0f  cent %4.0f"
              % (label, len(rows), med('remote'), med('client'), med('stretch'),
                 med('head'), med('tail'), med('env'), med('cent')))


if __name__ == "__main__":
    main()
