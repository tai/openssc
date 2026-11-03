#!/usr/bin/env python3
"""Decode an Apple 0x3ea (RGB565 + deflate) rectangle payload to PNG.

Usage: tier_png.py <payload.bin> <width> <height> <out.png> [--big]
"""
import struct
import sys
import zlib


def write_png(path, width, height, rgb):
    raw = bytearray()
    stride = width * 3
    for y in range(height):
        raw.append(0)  # filter: none
        raw += rgb[y * stride:(y + 1) * stride]

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main():
    payload = open(sys.argv[1], "rb").read()
    width = int(sys.argv[2])
    height = int(sys.argv[3])
    out = sys.argv[4]
    big = "--big" in sys.argv
    swap = "--bgr" in sys.argv

    d = zlib.decompressobj()
    raw = d.decompress(payload)
    need = width * height * 2
    if len(raw) < need:
        sys.exit(f"short decompress: {len(raw)} < {need}")

    rgb = bytearray(width * height * 3)
    order = "big" if big else "little"
    for i in range(width * height):
        v = int.from_bytes(raw[i * 2:i * 2 + 2], order)
        if swap:
            b = (v >> 11) & 0x1F
            g = (v >> 5) & 0x3F
            r = v & 0x1F
        else:
            r = (v >> 11) & 0x1F
            g = (v >> 5) & 0x3F
            b = v & 0x1F
        rgb[i * 3 + 0] = (r << 3) | (r >> 2)
        rgb[i * 3 + 1] = (g << 2) | (g >> 4)
        rgb[i * 3 + 2] = (b << 3) | (b >> 2)
    write_png(out, width, height, rgb)
    print(f"wrote {out} ({width}x{height}, {order}-endian 565)")


if __name__ == "__main__":
    main()
