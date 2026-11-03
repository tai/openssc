#!/usr/bin/env python3
"""Annotate a dumped x86_64 method with resolved Objective-C selectors.

Reads `mov rsi, [rel ADDR]` selector references from the live shared cache and
prints the disassembly with selector names. The dyld shared cache slide is
stable across processes on this machine, so addresses from a previous dump are
valid after dlopen()ing the framework.

Usage:
    python3 tools/instrument/annotate_method.py artifacts/fw/SSSession_stSendGestureSwipeEvent_.bin 0x7ff9224160b8
"""

from __future__ import annotations

import ctypes
import re
import subprocess
import sys

OBJC = ctypes.CDLL("/usr/lib/libobjc.A.dylib")
OBJC.sel_getName.restype = ctypes.c_char_p
OBJC.sel_getName.argtypes = [ctypes.c_void_p]
OBJC.class_getName.restype = ctypes.c_char_p
OBJC.class_getName.argtypes = [ctypes.c_void_p]

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/ScreenSharing"


def deref(addr: int) -> int:
    return int.from_bytes(ctypes.string_at(addr, 8), "little")


def name_of(ptr: int) -> str | None:
    if not ptr:
        return None
    try:
        s = OBJC.sel_getName(ptr)
        if s:
            return s.decode("utf-8", "replace")
    except Exception:
        pass
    try:
        c = OBJC.class_getName(ptr)
        if c:
            return "class:" + c.decode("utf-8", "replace")
    except Exception:
        pass
    return None


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    path = sys.argv[1]
    base = int(sys.argv[2], 0)
    ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)

    out = subprocess.run(
        ["ndisasm", "-b", "64", "-o", hex(base), path],
        capture_output=True,
        text=True,
        check=True,
    ).stdout

    sel_at_call = None
    for line in out.splitlines():
        m = re.search(r"mov rsi,\[rel (0x[0-9a-f]+)\]", line)
        if m:
            addr = int(m.group(1), 16)
            try:
                ptr = deref(addr)
                name = name_of(ptr)
            except Exception:
                name = None
            sel_at_call = name
            if name:
                line = f"{line}    ; {name}"
        if "call r15" in line and sel_at_call:
            line = f"{line}    ; [{sel_at_call}]"
            sel_at_call = None
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
