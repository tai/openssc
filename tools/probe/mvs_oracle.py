#!/usr/bin/env python3
"""Call the real ScreenSharing.framework MVS primitives from our own process.

The framework lives in the dyld shared cache and dlopen() maps it, so the
decoder's static functions are callable by address. That gives ground truth for
the bit-stream grammar without attaching to the sandboxed app.

Recovered addresses (offset from the framework image base):
    bit_reader   0x7ff922480895  (state, nbits) -> value
    read_run     0x7ff92248090c  (state)        -> run
    sym_decode   0x7ff9223bc1bf  (state, &out)  -> 0 (gamma-coded integer)

state = { u32 acc; i32 avail; u8 *ptr; }

Usage: python3 tools/probe/mvs_oracle.py artifacts/mvs-4.bin [count]
"""
import ctypes
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"

BIT_READER_VM = 0x7FF922480895 - 0x7FF9223A3000  # offset from image base
RUN_VM = 0x7FF92248090C - 0x7FF9223A3000
SYM_VM = 0x7FF9223BC1BF - 0x7FF9223A3000


class State(ctypes.Structure):
    _fields_ = [("acc", ctypes.c_uint32),
                ("avail", ctypes.c_int32),
                ("ptr", ctypes.c_void_p)]


def image_base():
    ctypes.CDLL(FW, mode=ctypes.RTLD_GLOBAL)
    dyld = ctypes.CDLL(None)
    dyld._dyld_image_count.restype = ctypes.c_uint32
    dyld._dyld_get_image_name.restype = ctypes.c_char_p
    dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]
    dyld._dyld_get_image_header.restype = ctypes.c_void_p
    dyld._dyld_get_image_header.argtypes = [ctypes.c_uint32]
    for i in range(dyld._dyld_image_count()):
        name = dyld._dyld_get_image_name(i) or b""
        if b"ScreenSharing.framework" in name and name.endswith(b"/ScreenSharing"):
            return dyld._dyld_get_image_header(i)
    raise SystemExit("framework image not found")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 16
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body, len(body))

    base = image_base()
    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State), ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.POINTER(State))
    SymDecode = ctypes.CFUNCTYPE(ctypes.c_int32, ctypes.POINTER(State), ctypes.POINTER(ctypes.c_uint16))
    bit_reader = BitReader(base + BIT_READER_VM)
    read_run = ReadRun(base + RUN_VM)
    sym = SymDecode(base + SYM_VM)

    st = State(0, 0, ctypes.cast(buf, ctypes.c_void_p))
    print(f"base=0x{base:x} body={len(body)} bytes")
    print(f"{'#':>3} {'cmd':>3} {'run':>6} {'acc':>18} {'avail':>6} {'off':>7}")
    for i in range(count):
        off_before = ctypes.cast(st.ptr, ctypes.c_void_p).value - ctypes.addressof(buf)
        cmd = bit_reader(ctypes.byref(st), 3)
        run = read_run(ctypes.byref(st))
        off_after = ctypes.cast(st.ptr, ctypes.c_void_p).value - ctypes.addressof(buf)
        print(f"{i:3d} {cmd:3d} {run:6d} 0x{st.acc:08x} {st.avail:6d} {off_before:7d}->{off_after}")

    # gamma integer check at the current position
    out = ctypes.c_uint16(0)
    ok = sym(ctypes.byref(st), ctypes.byref(out))
    print(f"sym_decode -> ok={ok} value={out.value}")


if __name__ == "__main__":
    main()
