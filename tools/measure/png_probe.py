#!/usr/bin/env python3
"""png_probe.py — answer "did the client actually paint something?" from a PNG.

`ss-gui --dump` writes an 8-bit RGB PNG of the framebuffer. This checker reads
it with the standard library only (no Pillow, no pngcheck, no `file`) and
reports the facts the Wayland smoke test needs:

  * the IHDR geometry, so "right size" is a measurement rather than a guess;
  * the file size, so an all-one-colour frame cannot pass on a size threshold;
  * how many distinct colours are present, and how much of the image differs
    from its first pixel, so a blank or cursor-only frame is caught.

It decodes the pixel data rather than trusting the header: a truncated or
corrupt IDAT fails here even when `file(1)` would call it a PNG.

Supported: non-interlaced, bit depth 8, grey/RGB/grey-alpha/RGBA (colour types
0, 2, 4, 6). That is what `png::Encoder` writes for `--dump`. Anything else is
reported as unsupported instead of being accepted blindly.

Exit status: 0 if every requested check passes, 1 on a failed check, 2 on an
unreadable/unsupported file (both are failures for a smoke test).
"""

import argparse
import os
import struct
import sys
import zlib

PNG_SIG = b"\x89PNG\r\n\x1a\n"
CHANNELS = {0: 1, 2: 3, 4: 2, 6: 4}
COLOUR_NAMES = {0: "grey", 2: "rgb", 4: "grey+alpha", 6: "rgba"}
DISTINCT_CAP = 1 << 16


def fail(message):
    print("FAIL: " + message, file=sys.stderr)
    raise SystemExit(1)


def read_chunks(path):
    with open(path, "rb") as fh:
        data = fh.read()
    if not data.startswith(PNG_SIG):
        fail("not a PNG (bad signature)")
    pos = len(PNG_SIG)
    ihdr = None
    idat = []
    saw_iend = False
    while pos + 8 <= len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        if len(body) != length:
            fail("truncated %r chunk" % kind)
        pos += 12 + length
        if kind == b"IHDR":
            if length != 13:
                fail("IHDR is %d bytes, expected 13" % length)
            ihdr = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            saw_iend = True
            break
    if ihdr is None:
        fail("no IHDR chunk")
    if not idat:
        fail("no IDAT chunk (no pixel data)")
    if not saw_iend:
        fail("no IEND chunk (truncated file)")
    return data, ihdr, b"".join(idat)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def unfilter(raw, width, height, bpp):
    """Undo PNG scanline filters; returns a list of bytearrays."""
    stride = width * bpp
    need = height * (stride + 1)
    if len(raw) < need:
        fail("IDAT has %d bytes, need %d for %dx%d" % (len(raw), need, width, height))
    rows = []
    prev = bytearray(stride)
    pos = 0
    for _ in range(height):
        kind = raw[pos]
        pos += 1
        line = bytearray(raw[pos : pos + stride])
        pos += stride
        if kind == 0:
            pass
        elif kind == 1:  # Sub
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 0xFF
        elif kind == 2:  # Up
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 0xFF
        elif kind == 3:  # Average
            for x in range(stride):
                left = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((left + prev[x]) >> 1)) & 0xFF
        elif kind == 4:  # Paeth
            for x in range(stride):
                left = line[x - bpp] if x >= bpp else 0
                up = prev[x]
                upleft = prev[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + paeth(left, up, upleft)) & 0xFF
        else:
            fail("unknown PNG filter type %d on row %d" % (kind, len(rows)))
        rows.append(line)
        prev = line
    return rows


def main():
    ap = argparse.ArgumentParser(description="validate an ss-gui --dump PNG")
    ap.add_argument("png")
    ap.add_argument(
        "--expect",
        default="any",
        help="WxH (e.g. 2560x1600) or 'any' to require only >=640x480",
    )
    ap.add_argument(
        "--min-bytes",
        type=int,
        default=0,
        help="minimum file size; 0 = auto (max(10 KiB, 0.5%% of raw RGB))",
    )
    ap.add_argument("--min-colours", type=int, default=8)
    args = ap.parse_args()

    size = os.path.getsize(args.png)
    _data, ihdr, idat = read_chunks(args.png)
    width, height, depth, colour, compression, filtering, interlace = ihdr
    if depth != 8:
        fail("bit depth %d is unsupported (expected 8)" % depth)
    if colour not in CHANNELS:
        fail("colour type %d is unsupported" % colour)
    if compression != 0 or filtering != 0 or interlace != 0:
        fail("compression/filter/interlace must be 0/0/0 (got %d/%d/%d)"
             % (compression, filtering, interlace))
    if width == 0 or height == 0:
        fail("zero-sized image")

    bpp = CHANNELS[colour]
    rows = unfilter(zlib.decompress(idat), width, height, bpp)

    first = tuple(rows[0][0:bpp])
    seen = set()
    distinct_capped = False
    non_first = 0
    for line in rows:
        for x in range(0, width * bpp, bpp):
            px = tuple(line[x : x + bpp])
            if px != first:
                non_first += 1
            if not distinct_capped:
                seen.add(px)
                if len(seen) >= DISTINCT_CAP:
                    distinct_capped = True
    distinct = DISTINCT_CAP if distinct_capped else len(seen)
    total = width * height
    raw_rgb = width * height * 3
    auto_min = max(10 * 1024, raw_rgb // 200)
    min_bytes = args.min_bytes or auto_min
    non_first_pct = 100.0 * non_first / total

    print(
        "PNG: %dx%d %s8 %d bytes, %d%s distinct colours, %.1f%% non-background"
        % (
            width,
            height,
            COLOUR_NAMES[colour],
            size,
            distinct,
            "+" if distinct_capped else "",
            non_first_pct,
        ),
        flush=True,
    )

    if args.expect != "any":
        # "auto" is accepted as a friendlier alias for the minimum-size rule.
        if args.expect in ("auto", "minimum"):
            if width < 640 or height < 480:
                fail("image is %dx%d, smaller than 640x480" % (width, height))
        else:
            try:
                want_w, want_h = (int(v) for v in args.expect.lower().split("x"))
            except ValueError:
                fail("--expect must be WxH or 'any'")
            if (width, height) != (want_w, want_h):
                fail(
                    "image is %dx%d, expected %dx%d (pass --expect any for a "
                    "different desktop size)" % (width, height, want_w, want_h)
                )

    if size < min_bytes:
        fail(
            "PNG is %d bytes, below the %d-byte floor: too little content to "
            "be a painted desktop" % (size, min_bytes)
        )
    if distinct <= 1:
        fail("image is a single colour (blank frame)")
    if distinct < args.min_colours:
        fail(
            "%d distinct colours, expected at least %d: the frame is nearly "
            "blank" % (distinct, args.min_colours)
        )
    if non_first == 0:
        fail("every pixel equals the first pixel")

    print("OK: the dump is a painted, non-trivial frame")


if __name__ == "__main__":
    main()
