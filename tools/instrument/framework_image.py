#!/usr/bin/env python3
"""Reach ScreenSharing.framework's Mach-O image inside the dyld shared cache.

The framework has no on-disk Mach-O; dlopen() still maps it, so the in-memory
headers are fully readable. This tool parses the load commands and can dump or
scan sections (useful for locating Huffman tables for the MVS decoder).

Usage:
    python3 tools/instrument/framework_image.py --list
    python3 tools/instrument/framework_image.py --dump __DATA_CONST --out artifacts/fw-data.bin
    python3 tools/instrument/framework_image.py --find-strings 'DecodeMVS|MultiVariant'
"""
from __future__ import annotations

import argparse
import ctypes
import struct
import sys

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/ScreenSharing"

LC_SEGMENT_64 = 0x19


def load() -> None:
    ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)


def image_slide(index: int) -> int:
    dyld = ctypes.CDLL(None)
    dyld._dyld_get_image_vmaddr_slide.restype = ctypes.c_long
    dyld._dyld_get_image_vmaddr_slide.argtypes = [ctypes.c_uint32]
    return dyld._dyld_get_image_vmaddr_slide(index) & 0xFFFF_FFFF_FFFF_FFFF


def image_index() -> int:
    dyld = ctypes.CDLL(None)
    dyld._dyld_image_count.restype = ctypes.c_uint32
    dyld._dyld_get_image_name.restype = ctypes.c_char_p
    dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]
    dyld._dyld_get_image_header.restype = ctypes.c_void_p
    dyld._dyld_get_image_header.argtypes = [ctypes.c_uint32]
    for i in range(dyld._dyld_image_count()):
        name = dyld._dyld_get_image_name(i) or b""
        if b"ScreenSharing.framework" in name and b"/ScreenSharing" in name:
            return i
    raise SystemExit("ScreenSharing.framework image not found")


def image_header() -> int:
    dyld = ctypes.CDLL(None)
    dyld._dyld_get_image_header.restype = ctypes.c_void_p
    dyld._dyld_get_image_header.argtypes = [ctypes.c_uint32]
    return dyld._dyld_get_image_header(image_index())


def cstr(buf: bytes) -> str:
    return buf.split(b"\0")[0].decode("ascii", "replace")


def segments(base: int):
    magic, _cpu, _sub, _filetype, ncmds, _sizeofcmds, _flags, _res = struct.unpack(
        "<IiiIIIII", ctypes.string_at(base, 32)
    )
    if magic != 0xFEEDFACF:
        raise SystemExit(f"unexpected Mach-O magic 0x{magic:x}")
    off = 32
    out = []
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack("<II", ctypes.string_at(base + off, 8))
        if cmd == LC_SEGMENT_64:
            seg = struct.unpack("<16sQQQQiiII", ctypes.string_at(base + off + 8, 64))
            segname = cstr(seg[0])
            vmaddr, vmsize, _fileoff, _filesize, _maxp, _initp, nsects, _fl = seg[1:9]
            sects = []
            soff = off + 72
            for _ in range(nsects):
                s = struct.unpack("<16s16sQQIIIIIIII", ctypes.string_at(base + soff, 80))
                sects.append((cstr(s[0]), cstr(s[1]), s[2], s[3]))
                soff += 80
            out.append((segname, vmaddr, vmsize, sects))
        off += cmdsize
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--dump")
    ap.add_argument("--out")
    ap.add_argument("--find-strings")
    args = ap.parse_args()

    load()
    base = image_header()
    slide = image_slide(image_index())
    print(f"image header @ 0x{base:x} (slide 0x{slide:x})")
    segs = [(n, v + slide, sz, [(a, b, c + slide, d) for a, b, c, d in sects])
            for n, v, sz, sects in segments(base)]

    if args.list:
        for segname, vmaddr, vmsize, sects in segs:
            print(f"[{segname}] vmaddr=0x{vmaddr:x} size=0x{vmsize:x} ({vmsize} bytes)")
            for sect, sseg, addr, size in sects:
                print(f"    {sseg},{sect} 0x{addr:x} {size} bytes")

    if args.dump and args.out:
        for segname, _vmaddr, _vmsize, sects in segs:
            for sect, _sseg, addr, size in sects:
                if sect == args.dump or segname == args.dump:
                    data = ctypes.string_at(addr, size)
                    with open(args.out, "wb") as f:
                        f.write(data)
                    print(f"wrote {args.out} from {segname},{sect} ({size} bytes) @ 0x{addr:x}")

    if args.find_strings:
        import re

        pat = re.compile(args.find_strings.encode())
        for segname, _vmaddr, vmsize, _sects in segs:
            pass
        for segname, vmaddr, vmsize, _sects in segs:
            data = ctypes.string_at(vmaddr, vmsize)
            for m in pat.finditer(data):
                start = data.rfind(b"\0", max(0, m.start() - 200), m.start()) + 1
                end = data.find(b"\0", m.end())
                print(f"0x{vmaddr + start:x}: {data[start:end][:120]!r}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
