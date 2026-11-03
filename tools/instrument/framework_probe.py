#!/usr/bin/env python3
"""Probe ScreenSharing.framework classes, ivars, and method implementations.

The framework lives only in the dyld shared cache, but dlopen() resolves it.
This tool dumps Objective-C metadata and raw x86_64 method bytes for offline
disassembly (no task_for_pid / SIP debugging required).

Examples:
    python3 tools/instrument/framework_probe.py --ivars SSGesture
    python3 tools/instrument/framework_probe.py --methods SSSession --match 'stSendGesture|stSendEvent'
    python3 tools/instrument/framework_probe.py --dump SSSession --match 'stSendGesture' --out artifacts/fw
"""

from __future__ import annotations

import argparse
import ctypes
import os
import re
import sys

OBJC = ctypes.CDLL("/usr/lib/libobjc.A.dylib")

OBJC.objc_getClass.restype = ctypes.c_void_p
OBJC.objc_getClass.argtypes = [ctypes.c_char_p]
OBJC.class_getName.restype = ctypes.c_char_p
OBJC.class_getName.argtypes = [ctypes.c_void_p]
OBJC.class_copyMethodList.restype = ctypes.POINTER(ctypes.c_void_p)
OBJC.class_copyMethodList.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint)]
OBJC.method_getName.restype = ctypes.c_void_p
OBJC.method_getName.argtypes = [ctypes.c_void_p]
OBJC.method_getTypeEncoding.restype = ctypes.c_char_p
OBJC.method_getTypeEncoding.argtypes = [ctypes.c_void_p]
OBJC.method_getImplementation.restype = ctypes.c_void_p
OBJC.method_getImplementation.argtypes = [ctypes.c_void_p]
OBJC.sel_getName.restype = ctypes.c_char_p
OBJC.sel_getName.argtypes = [ctypes.c_void_p]
OBJC.class_copyIvarList.restype = ctypes.POINTER(ctypes.c_void_p)
OBJC.class_copyIvarList.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint)]
OBJC.ivar_getName.restype = ctypes.c_char_p
OBJC.ivar_getName.argtypes = [ctypes.c_void_p]
OBJC.ivar_getOffset.restype = ctypes.c_long
OBJC.ivar_getOffset.argtypes = [ctypes.c_void_p]
OBJC.ivar_getTypeEncoding.restype = ctypes.c_char_p
OBJC.ivar_getTypeEncoding.argtypes = [ctypes.c_void_p]
OBJC.objc_copyClassList.restype = ctypes.POINTER(ctypes.c_void_p)
OBJC.objc_copyClassList.argtypes = [ctypes.POINTER(ctypes.c_uint)]

FRAMEWORK = "/System/Library/PrivateFrameworks/ScreenSharing.framework/ScreenSharing"


def load() -> None:
    ctypes.CDLL(FRAMEWORK, mode=ctypes.RTLD_GLOBAL)


def all_classes() -> list[int]:
    count = ctypes.c_uint(0)
    ptr = OBJC.objc_copyClassList(ctypes.byref(count))
    out = []
    for i in range(count.value):
        c = ptr[i]
        if c:
            name = OBJC.class_getName(c)
            if name:
                out.append(c)
    return out


def find_classes(pattern: str) -> list[tuple[str, int]]:
    rx = re.compile(pattern, re.I)
    out = []
    for c in all_classes():
        name = OBJC.class_getName(c).decode("utf-8", "replace")
        if rx.search(name):
            out.append((name, c))
    return sorted(out)


def methods(cls: int):
    n = ctypes.c_uint(0)
    ptr = OBJC.class_copyMethodList(cls, ctypes.byref(n))
    out = []
    for i in range(n.value):
        m = ptr[i]
        sel = OBJC.sel_getName(OBJC.method_getName(m)).decode("utf-8", "replace")
        typ = (OBJC.method_getTypeEncoding(m) or b"").decode("utf-8", "replace")
        imp = OBJC.method_getImplementation(m) or 0
        out.append((sel, typ, imp))
    return out


def ivars(cls: int):
    n = ctypes.c_uint(0)
    ptr = OBJC.class_copyIvarList(cls, ctypes.byref(n))
    out = []
    for i in range(n.value):
        iv = ptr[i]
        name = (OBJC.ivar_getName(iv) or b"").decode("utf-8", "replace")
        typ = (OBJC.ivar_getTypeEncoding(iv) or b"").decode("utf-8", "replace")
        off = OBJC.ivar_getOffset(iv)
        out.append((name, typ, off))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ivars", default=None, help="regex of classes whose ivars to print")
    ap.add_argument("--methods", default=None, help="regex of classes whose methods to print")
    ap.add_argument("--match", default=".", help="regex of method names")
    ap.add_argument("--dump", default=None, help="regex of classes whose matching IMPs to dump")
    ap.add_argument("--out", default="artifacts/fw")
    ap.add_argument("--length", type=int, default=2048)
    args = ap.parse_args()

    load()
    mrx = re.compile(args.match, re.I)

    if args.ivars:
        for name, cls in find_classes(args.ivars):
            print(f"\n## {name}")
            for ivname, typ, off in ivars(cls):
                print(f"  +0x{off:03x} {typ:<20} {ivname}")

    if args.methods:
        for name, cls in find_classes(args.methods):
            ms = [m for m in methods(cls) if mrx.search(m[0])]
            if ms:
                print(f"\n## {name}")
                for sel, typ, imp in ms:
                    print(f"  {sel}  [{typ}]  imp=0x{imp:x}")

    if args.dump:
        os.makedirs(args.out, exist_ok=True)
        for name, cls in find_classes(args.dump):
            for sel, typ, imp in methods(cls):
                if not mrx.search(sel) or not imp:
                    continue
                safe = re.sub(r"[^A-Za-z0-9_.-]", "_", f"{name}_{sel}")
                path = os.path.join(args.out, safe + ".bin")
                with open(path, "wb") as f:
                    f.write(ctypes.string_at(imp, args.length))
                print(f"dumped {path}  imp=0x{imp:x}  type={typ}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
