#!/usr/bin/env python3
"""Enumerate Objective-C classes/methods in ScreenSharing.framework.

The framework's Mach-O is only in the dyld shared cache on modern macOS, but
dlopen() resolves it by path. No code injection required.

Usage:
    python3 tools/instrument/dump_framework_classes.py [--filter REGEX]
"""

from __future__ import annotations

import argparse
import ctypes
import re
import sys

OBJC = ctypes.CDLL("/usr/lib/libobjc.A.dylib")

OBJC.objc_getClass.restype = ctypes.c_void_p
OBJC.objc_getClass.argtypes = [ctypes.c_char_p]
OBJC.class_getName.restype = ctypes.c_char_p
OBJC.class_getName.argtypes = [ctypes.c_void_p]
OBJC.class_getImageName.restype = ctypes.c_char_p
OBJC.class_getImageName.argtypes = [ctypes.c_void_p]
OBJC.class_copyMethodList.restype = ctypes.POINTER(ctypes.c_void_p)
OBJC.class_copyMethodList.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint)]
OBJC.method_getName.restype = ctypes.c_void_p
OBJC.method_getName.argtypes = [ctypes.c_void_p]
OBJC.sel_getName.restype = ctypes.c_char_p
OBJC.sel_getName.argtypes = [ctypes.c_void_p]
OBJC.objc_copyClassList.restype = ctypes.POINTER(ctypes.c_void_p)
OBJC.objc_copyClassList.argtypes = [ctypes.POINTER(ctypes.c_uint)]

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/ScreenSharing"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--filter", default=r"gesture|swipe|magnif|rotat|scroll|touch|mouse|key|event|send|paste|display|encod|input", type=str)
    args = ap.parse_args()
    pat = re.compile(args.filter, re.I)

    try:
        ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)
    except OSError as exc:
        print(f"dlopen failed: {exc}", file=sys.stderr)
        return 1

    count = ctypes.c_uint(0)
    classes = OBJC.objc_copyClassList(ctypes.byref(count))
    shown = 0
    for i in range(count.value):
        cls = classes[i]
        if not cls:
            continue
        image = OBJC.class_getImageName(cls)
        if not image or b"ScreenSharing" not in image:
            continue
        cname = OBJC.class_getName(cls).decode("utf-8", "replace")
        n = ctypes.c_uint(0)
        methods = OBJC.class_copyMethodList(cls, ctypes.byref(n))
        names = []
        for j in range(n.value):
            sel = OBJC.method_getName(methods[j])
            sname = OBJC.sel_getName(sel).decode("utf-8", "replace")
            if pat.search(sname) or pat.search(cname):
                names.append(sname)
        if names:
            print(f"\n## {cname}")
            for s in sorted(names):
                print(f"  {s}")
            shown += 1
    print(f"\nclasses with matches: {shown} / {count.value}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
