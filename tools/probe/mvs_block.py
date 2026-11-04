#!/usr/bin/env python3
"""Call the real MVS DCT block decoder (0x7ff922480b28) from our own process.

If it can be driven directly, it consumes exactly the right number of bits per
block, which lets us walk the whole stream with the real command/run/block
decoders and confirm the 64,000-tile total.

Usage: python3 tools/probe/mvs_block.py artifacts/mvs-4.bin
"""
import ctypes
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"


# The decoder functions take a pointer into the *viewer* state, with the bit
# reader at +0xa8 (acc), +0xac (avail) and +0xb0 (ptr). Emulate that layout.
STATE_SIZE = 0x1000
ACC_OFF, AVAIL_OFF, PTR_OFF = 0xA8, 0xAC, 0xB0


class State:
    def __init__(self, buf):
        self.buf = ctypes.create_string_buffer(STATE_SIZE)
        self.addr = ctypes.addressof(self.buf)
        self.set(0, 0, ctypes.addressof(buf))

    def set(self, acc, avail, ptr):
        ctypes.c_uint32.from_address(self.addr + ACC_OFF).value = acc
        ctypes.c_int32.from_address(self.addr + AVAIL_OFF).value = avail
        ctypes.c_void_p.from_address(self.addr + PTR_OFF).value = ptr

    def get(self):
        return (ctypes.c_uint32.from_address(self.addr + ACC_OFF).value,
                ctypes.c_int32.from_address(self.addr + AVAIL_OFF).value,
                ctypes.c_void_p.from_address(self.addr + PTR_OFF).value)

    def copy(self):
        n = State.__new__(State)
        n.buf = ctypes.create_string_buffer(STATE_SIZE)
        n.addr = ctypes.addressof(n.buf)
        ctypes.memmove(n.addr, self.addr, STATE_SIZE)
        return n


def base_addr():
    ctypes.CDLL(FW, mode=ctypes.RTLD_GLOBAL)
    d = ctypes.CDLL(None)
    d._dyld_image_count.restype = ctypes.c_uint32
    d._dyld_get_image_name.restype = ctypes.c_char_p
    d._dyld_get_image_name.argtypes = [ctypes.c_uint32]
    d._dyld_get_image_header.restype = ctypes.c_void_p
    d._dyld_get_image_header.argtypes = [ctypes.c_uint32]
    for i in range(d._dyld_image_count()):
        n = d._dyld_get_image_name(i) or b""
        if b"ScreenSharing.framework" in n and n.endswith(b"/ScreenSharing"):
            return d._dyld_get_image_header(i)
    raise SystemExit("no framework")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body, len(body))
    base = base_addr()

    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p)
    Block = ctypes.CFUNCTYPE(
        ctypes.c_int,
        ctypes.c_void_p,   # rdi state
        ctypes.c_void_p,         # rsi out
        ctypes.c_void_p,         # rdx ref
        ctypes.c_int,            # ecx
        ctypes.c_int,            # r8d
        ctypes.c_int,            # r9d
        ctypes.c_void_p,         # stack arg 1
        ctypes.c_void_p,         # stack arg 2
        ctypes.c_void_p,         # stack arg 3
    )
    br = BitReader(base + (0x7FF922480895 - 0x7FF9223A3000))
    rr = ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000))
    blk = Block(base + (0x7FF922480B28 - 0x7FF9223A3000))

    st = State(buf)

    def consumed():
        _acc, avail, ptr = st.get()
        return (ptr - ctypes.addressof(buf)) * 8 - avail

    # walk to the first DCT
    while True:
        c = br(st.addr, 3)
        rr(st.addr)
        if c == 5:
            break
        if c == 3:
            for _ in range(8):
                br(st.addr, 8)
            br(st.addr, 20)
            br(st.addr, 20)
        elif c == 4:
            b1 = br(st.addr, 1)
            b2 = br(st.addr, 1)
            if b1 == 0 and b2 == 0:
                br(st.addr, 8)
                br(st.addr, 6)
                br(st.addr, 6)
        elif c == 6:
            br(st.addr, 16)

    before = consumed()
    out = ctypes.create_string_buffer(0x200)
    ref = ctypes.create_string_buffer(0x200)
    side1 = ctypes.create_string_buffer(0x200)
    side2 = ctypes.create_string_buffer(0x200)
    side3 = ctypes.create_string_buffer(0x200)
    rc = blk(st.addr, ctypes.cast(out, ctypes.c_void_p),
             ctypes.cast(ref, ctypes.c_void_p), 0, 0, 0,
             ctypes.cast(side1, ctypes.c_void_p),
             ctypes.cast(side2, ctypes.c_void_p),
             ctypes.cast(side3, ctypes.c_void_p))
    after = consumed()
    print(f"block rc={rc} consumed={after - before} bits (from bit {before})")
    print("out[0:16] =", out.raw[:16].hex(" "))


if __name__ == "__main__":
    main()
