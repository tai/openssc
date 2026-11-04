#!/usr/bin/env python3
"""Walk an MVS command stream using the framework's own bit reader and run
reader, so the command/run layer is ground truth and only the payload widths
are ours.

Usage: python3 tools/probe/mvs_walk.py artifacts/mvs-4.bin
"""
import ctypes
import sys

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
    if not base:
        raise SystemExit("no framework")
    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State), ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State))
    br = BitReader(base + (0x7FF922480895 - 0x7FF9223A3000))
    rr = ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000))
    return br, rr


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body, len(body))
    br, rr = setup()
    st = State(0, 0, ctypes.cast(buf, ctypes.c_void_p))

    def consumed():
        off = ctypes.cast(st.ptr, ctypes.c_void_p).value - ctypes.addressof(buf)
        return off * 8 - st.avail

    def read(n):
        return br(ctypes.byref(st), n)

    tiles = 0
    cmds = 0
    codes = {}
    try:
        while True:
            c = read(3)
            run = rr(ctypes.byref(st))
            tiles += 1 + run
            cmds += 1
            codes[c] = codes.get(c, 0) + 1
            if c == 3:
                for _ in range(8):
                    read(8)
                read(20)
                read(20)
            elif c == 4:
                b1 = read(1)
                b2 = read(1)
                if b1 == 0 and b2 == 0:
                    read(8)
                    read(6)
                    read(6)
                else:
                    raise SystemExit(f"code4 variant b1={b1} b2={b2} at bit {consumed()}")
            elif c == 5:
                raise SystemExit(f"DCT at bit {consumed()} tiles={tiles}")
            elif c == 6:
                read(16)
            elif c in (0, 1, 2, 7):
                pass
    except IndexError:
        pass
    except Exception as e:
        print("stopped:", e)
    print(f"tiles={tiles} cmds={cmds} codes={codes} (target 64000)")


if __name__ == "__main__":
    main()
