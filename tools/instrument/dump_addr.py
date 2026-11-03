#!/usr/bin/env python3
"""Dump and annotate a raw function from the loaded ScreenSharing framework.

Usage:
    python3 tools/instrument/dump_addr.py 0x7ff92245dd67 2048 artifacts/fw/swipe_pack.bin
Then:
    python3 tools/instrument/annotate_method.py artifacts/fw/swipe_pack.bin 0x7ff92245dd67
"""

from __future__ import annotations

import ctypes
import sys

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/ScreenSharing"


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    addr = int(sys.argv[1], 0)
    length = int(sys.argv[2], 0)
    out = sys.argv[3] if len(sys.argv) > 3 else "artifacts/fw/function.bin"
    ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)
    data = ctypes.string_at(addr, length)
    with open(out, "wb") as f:
        f.write(data)
    print(f"wrote {out} ({len(data)} bytes) from 0x{addr:x}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
