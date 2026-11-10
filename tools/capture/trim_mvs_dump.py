#!/usr/bin/env python3
"""Trim an `SS_DUMP_MVS` capture to an arrival-time window.

The GUI dump (`SS_DUMP_MVS=<path> openssc ...`) records every MVS body from
connect through whatever the client sees, which includes the initial paint and
any unrelated animation on the remote screen. The decode benchmark wants one
scroll burst, so this cuts the stream by arrival time:

    python3 tools/capture/trim_mvs_dump.py raw.bin burst.bin --from-ms 3000 --to-ms 5000

The format is the one documented in `crates/rfb-proto/tests/mvs_replay.rs`.
Prints the record/tile count of the result. No third-party modules.
"""
import argparse
import struct
import sys

MAGIC = b"SSMVS1\0\0"
HEADER = struct.Struct("<IIHHHHI")  # len, ms, x, y, w, h, encoding


def read(path):
    data = open(path, "rb").read()
    if data[:8] != MAGIC:
        sys.exit(f"{path}: not an SS_DUMP_MVS capture")
    width, height = struct.unpack("<HH", data[8:12])
    at, records = 12, []
    while at < len(data):
        ln, ms, x, y, w, h, enc = HEADER.unpack_from(data, at)
        at += HEADER.size
        records.append((ms, x, y, w, h, enc, data[at : at + ln]))
        at += ln
    if at != len(data):
        sys.exit(f"{path}: truncated record at {at}")
    return width, height, records


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--from-ms", type=int, default=0)
    ap.add_argument("--to-ms", type=int, default=1 << 32)
    args = ap.parse_args()
    width, height, records = read(args.src)
    keep = [r for r in records if args.from_ms <= r[0] <= args.to_ms]
    if not keep:
        sys.exit("no records in that window")
    with open(args.dst, "wb") as f:
        f.write(MAGIC)
        f.write(struct.pack("<HH", width, height))
        for ms, x, y, w, h, enc, body in keep:
            f.write(HEADER.pack(len(body), ms, x, y, w, h, enc))
            f.write(body)
    tiles = sum(((w + 7) // 8) * ((h + 7) // 8) for _, _, _, w, h, _, b in keep if b and b[0] in (0, 1))
    span = keep[-1][0] - keep[0][0]
    print(
        f"{len(keep)} bodies, {tiles} tiles ({tiles * 64 / 1e6:.1f} Mpx), "
        f"{span} ms window -> {args.dst}"
    )


if __name__ == "__main__":
    main()
