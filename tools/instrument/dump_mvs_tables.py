#!/usr/bin/env python3
"""Dump the MVS Huffman and quantisation tables from ScreenSharing.framework.

The client decoder builds its canonical Huffman tables from compact
`(bits[16], symbols[])` pairs that live in the framework's __TEXT __const.
Addresses below were recovered from `DecodeMVSPartialUpdate`
(0x7ff9223bb400) and the table builder at 0x7ff9223bb724.

Usage:
    python3 tools/instrument/dump_mvs_tables.py [--json]
"""
from __future__ import annotations

import ctypes
import json
import sys

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"

# (name, bits address, symbols address) — canonical JPEG-style tables.
HUFFMAN = [
    ("luma_dc", 0x7FF9224C3480, 0x7FF9224C3491),
    ("luma_ac", 0x7FF9224C34C0, 0x7FF9224C34E0),
    ("chroma_dc", 0x7FF9224C34A0, 0x7FF9224C34B1),
    ("chroma_ac", 0x7FF9224C3590, 0x7FF9224C35B0),
]
# 64-byte quantisation tables (expanded to u16, <<4, zigzag-ordered by the
# client at 0x7ff9223bb4a9..0x7ff9223bb4ce and 0x7ff9223bb500..0x7ff9223bb525).
QUANT = [
    ("quant_a", 0x7FF9224C2390),
    ("quant_b", 0x7FF9224C23D0),
]


def canonical(counts: list[int], symbols: list[int]) -> dict[str, int]:
    """JPEG canonical code assignment: (length, code) -> symbol."""
    out = {}
    code = 0
    k = 0
    for length in range(1, 17):
        for _ in range(counts[length - 1]):
            out[f"{length}:{code}"] = symbols[k]
            k += 1
            code += 1
        code <<= 1
    return out


def main() -> int:
    ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)
    result = {"huffman": {}, "quant": {}}
    for name, bits, syms in HUFFMAN:
        counts = list(ctypes.string_at(bits, 16))
        total = sum(counts)
        symbols = list(ctypes.string_at(syms, total))
        result["huffman"][name] = {
            "counts": counts,
            "symbols": symbols,
            "codes": canonical(counts, symbols),
        }
    for name, addr in QUANT:
        result["quant"][name] = list(ctypes.string_at(addr, 64))

    if "--json" in sys.argv:
        print(json.dumps(result, indent=2))
        return 0
    for name, (bits, syms) in [(n, (b, s)) for n, b, s in HUFFMAN]:
        counts = list(ctypes.string_at(bits, 16))
        total = sum(counts)
        symbols = list(ctypes.string_at(syms, total))
        print(f"{name}: counts={counts} symbols={symbols[:16]}{'...' if total > 16 else ''}")
    for name, addr in QUANT:
        vals = list(ctypes.string_at(addr, 64))
        print(f"{name}: {vals[:16]}...")
    return 0


if __name__ == "__main__":
    sys.exit(main())
