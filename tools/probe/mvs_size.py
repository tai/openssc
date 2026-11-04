#!/usr/bin/env python3
"""Find the DCT block's bit length by brute force, using the framework's own
bit/run readers as the oracle for everything else.

At the first DCT command we try every candidate block length, then walk forward
with the known fixed payloads and count how many commands parse cleanly. The
correct length maximises that count.

Usage: python3 tools/probe/mvs_size.py artifacts/mvs-4.bin [maxsize]
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
    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State), ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State))
    return (BitReader(base + (0x7FF922480895 - 0x7FF9223A3000)),
            ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000)))


def walk(br, rr, st, start, limit_cmds=300, skip_dct=True):
    """Walk commands from a saved state; return (cmds, tiles, reached_eof)."""
    st.acc, st.avail, st.ptr = start
    tiles = 0
    cmds = 0
    try:
        for _ in range(limit_cmds):
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
                b1 = br(ctypes.byref(st), 1)
                b2 = br(ctypes.byref(st), 1)
                if b1 == 0 and b2 == 0:
                    br(ctypes.byref(st), 8)
                    br(ctypes.byref(st), 6)
                    br(ctypes.byref(st), 6)
                else:
                    return cmds - 1, tiles, False
            elif c == 5:
                return cmds - 1, tiles, False
            elif c == 6:
                br(ctypes.byref(st), 16)
    except IndexError:
        return cmds, tiles, True
    return cmds, tiles, False


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    maxsize = int(sys.argv[2]) if len(sys.argv) > 2 else 4000
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body, len(body))
    br, rr = setup()
    st = State(0, 0, ctypes.cast(buf, ctypes.c_void_p))

    # advance to the first DCT, keeping a saved state
    while True:
        c = br(ctypes.byref(st), 3)
        rr(ctypes.byref(st))
        if c == 5:
            break
        if c == 3:
            for _ in range(8):
                br(ctypes.byref(st), 8)
            br(ctypes.byref(st), 20)
            br(ctypes.byref(st), 20)
        elif c == 4:
            b1 = br(ctypes.byref(st), 1)
            b2 = br(ctypes.byref(st), 1)
            if b1 == 0 and b2 == 0:
                br(ctypes.byref(st), 8)
                br(ctypes.byref(st), 6)
                br(ctypes.byref(st), 6)
        elif c == 6:
            br(ctypes.byref(st), 16)

    save = (st.acc, st.avail, st.ptr)
    print(f"first DCT reached; searching block size 0..{maxsize} bits")
    best = []
    for n in range(0, maxsize):
        st.acc, st.avail, st.ptr = save
        # consume n bits
        left = n
        while left > 0:
            take = min(left, 24)
            br(ctypes.byref(st), take)
            left -= take
        cmds, tiles, eof = walk(br, rr, st, (st.acc, st.avail, st.ptr))
        best.append((cmds, n, tiles, eof))
    best.sort(reverse=True)
    for cmds, n, tiles, eof in best[:10]:
        print(f"  size={n:5d} bits -> {cmds:3d} clean cmds, tiles={tiles}, eof={eof}")


if __name__ == "__main__":
    main()
