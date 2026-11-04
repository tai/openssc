#!/usr/bin/env python3
"""Search the MVS DCT block model against the exact 64,000-tile oracle.

The framework's own bit reader and run reader provide ground truth for the
command/run layer; only the DCT block model is a hypothesis. A correct model
must make the whole command stream parse and total exactly 64,000 tiles
(320x200 tiles of 8x8 for a 2560x1600 frame).

Usage: python3 tools/probe/mvs_search.py artifacts/mvs-4.bin
"""
import ctypes
import itertools
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/SceneSharing"

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"


class State(ctypes.Structure):
    _fields_ = [("acc", ctypes.c_uint32),
                ("avail", ctypes.c_int32),
                ("ptr", ctypes.c_void_p)]


def setup():
    ctypes.CDLL(FW, mode=ctypes.RTLD_GLOBAL)
    d = ctypes.CDLL(None)
    d._dyld_image_count.restype = ctypes.c_uint32
    d._dyld_get_image_name.restype = ctypes.c_char_p
    d._dyld_get_image_name.argtypes = [ctypes.c_uint32]
    d._dyld_get_image_header.restype = ctypes.c_void_p
    d._dyld_get_image_header.argtypes = [ctypes.c_uint32]
    base = None
    for i in range(d._dyld_image_count()):
        n = d._dyld_get_image_name(i) or b""
        if b"ScreenSharing.framework" in n and n.endswith(b"/ScreenSharing"):
            base = d._dyld_get_image_header(i)
    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State), ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State))
    Sym = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.POINTER(State), ctypes.POINTER(ctypes.c_uint16))
    return (BitReader(base + (0x7FF922480895 - 0x7FF9223A3000)),
            ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000)),
            Sym(base + (0x7FF9223BC1BF - 0x7FF9223A3000)))


def run_model(br, rr, sym, st, model):
    """Walk the stream; returns (tiles, cmds, ok)."""
    p4, nflag, ngamma, acmode, acw, acn = model

    def gamma():
        out = ctypes.c_uint16(0)
        rc = sym(ctypes.byref(st), ctypes.byref(out))
        if rc != 0:
            raise ValueError("gamma")
        return out.value

    tiles = 0
    cmds = 0
    while True:
        c = br(ctypes.byref(st), 3)
        run = rr(ctypes.byref(st))
        tiles += 1 + run
        cmds += 1
        if c == 3:
            for _ in range(8):
                br(ctypes.byref(st), 8)
            br(ctypes.byref(st), 20)
            br(ctypes.byref(st), 20)
        elif c == 4:
            br(ctypes.byref(st), p4)
        elif c == 5:
            flag = br(ctypes.byref(st), nflag)
            if flag:
                continue
            for _ in range(ngamma):
                gamma()
            if acmode == "fixed":
                for _ in range(acn):
                    br(ctypes.byref(st), acw)
            else:
                for _ in range(acn):
                    gamma()
                    gamma()
        elif c == 6:
            br(ctypes.byref(st), 16)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body, len(body))
    br, rr, sym = setup()

    results = []
    for p4 in (20, 22, 23, 24):
        for nflag in (1,):
            for ngamma in (0, 1, 2, 3):
                for acmode, acw, acn in (("fixed", 2, 63), ("fixed", 8, 63),
                                         ("gamma", 0, 63), ("gamma", 0, 32),
                                         ("fixed", 2, 0), ("fixed", 8, 0)):
                    st = State(0, 0, ctypes.cast(buf, ctypes.c_void_p))
                    model = (p4, nflag, ngamma, acmode, acw, acn)
                    try:
                        tiles, cmds = (None, None)
                        # inline walk to catch IndexError with partials
                        results.append((0, model, 0, 0))
                        results.pop()
                        tiles, cmds = walk(br, rr, sym, st, model)
                        results.append((abs(tiles - 64000), model, tiles, cmds))
                    except (IndexError, ValueError):
                        continue
    results.sort(key=lambda r: r[0])
    for diff, model, tiles, cmds in results[:12]:
        print(f"diff={diff:6d} tiles={tiles:6d} cmds={cmds:5d} model={model}")


def walk(br, rr, sym, st, model):
    p4, nflag, ngamma, acmode, acw, acn = model

    def gamma():
        out = ctypes.c_uint16(0)
        if sym(ctypes.byref(st), ctypes.byref(out)) != 0:
            raise ValueError
        return out.value

    tiles = 0
    cmds = 0
    try:
        while True:
            c = br(ctypes.byref(st), 3)
            run = rr(ctypes.byref(st))
            tiles += 1 + run
            cmds += 1
            if c == 3:
                for _ in range(8):
                    br(ctypes.byref(st), 8)
                br(ctypes.byref(st), 20)
                br(ctypes.byref(st), 20)
            elif c == 4:
                br(ctypes.byref(st), p4)
            elif c == 5:
                if br(ctypes.byref(st), nflag):
                    continue
                for _ in range(ngamma):
                    gamma()
                if acmode == "fixed":
                    for _ in range(acn):
                        br(ctypes.byref(st), acw)
                else:
                    for _ in range(acn):
                        gamma()
                        gamma()
            elif c == 6:
                br(ctypes.byref(st), 16)
    except (IndexError, ValueError):
        return tiles, cmds


if __name__ == "__main__":
    main()
