#!/usr/bin/env python3
"""Recover the DCT block sizes by constrained search.

The command/run layer is ground truth (framework readers). At each DCT the
block length is unknown, so we branch over candidate lengths and keep states
whose tile count tracks the expected rate (64,000 tiles over the command
stream). A solution must consume the stream and total exactly 64,000.

Usage: python3 tools/probe/mvs_beam.py artifacts/mvs-4.bin [beam]
"""
import ctypes
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"
TARGET_TILES = 64000


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
    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p)
    return (BitReader(base + (0x7FF922480895 - 0x7FF9223A3000)),
            ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000)))


class State:
    __slots__ = ("buf", "acc", "avail", "off")

    def __init__(self, acc=0, avail=0, off=0):
        self.acc, self.avail, self.off = acc, avail, off

    def clone(self):
        return State(self.acc, self.avail, self.off)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    beam = int(sys.argv[2]) if len(sys.argv) > 2 else 6
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    body = d[6:doff]
    buf = ctypes.create_string_buffer(body + b"\0" * 64, len(body) + 64)
    limit = len(body) * 8 - 16
    br, rr = setup()
    base_addr = ctypes.addressof(buf)

    # one scratch state struct reused for every probe
    scratch = ctypes.create_string_buffer(0x1000)
    S = ctypes.addressof(scratch)

    def load(st):
        ctypes.c_uint32.from_address(S + 0xA8).value = st.acc
        ctypes.c_int32.from_address(S + 0xAC).value = st.avail
        ctypes.c_void_p.from_address(S + 0xB0).value = base_addr + st.off

    def store():
        return State(ctypes.c_uint32.from_address(S + 0xA8).value,
                     ctypes.c_int32.from_address(S + 0xAC).value,
                     ctypes.c_void_p.from_address(S + 0xB0).value - base_addr)

    def pos(st):
        return st.off * 8 - st.avail

    def step(st, skip=0):
        """Consume one command+run(+payload). Returns (cmd, run, st) or None."""
        load(st)
        try:
            c = br(S + 0xA8, 3)
            run = rr(S + 0xA8)
            if skip:
                left = skip
                while left > 0:
                    take = min(left, 24)
                    br(S + 0xA8, take)
                    left -= take
            elif c == 3:
                for _ in range(8):
                    br(S + 0xA8, 8)
                br(S + 0xA8, 20)
                br(S + 0xA8, 20)
            elif c == 4:
                br(S + 0xA8, 22)
            elif c == 6:
                br(S + 0xA8, 16)
        except IndexError:
            return None
        return c, run, store()

    # beam entries: (score, State, tiles, sizes[])
    stats = [(0.0, State(), 0, [])]
    dct_sizes = []
    for _ in range(4000):
        nxt = []
        done = False
        for score, st, tiles, sizes in stats:
            if pos(st) >= limit:
                done = True
                if abs(tiles - TARGET_TILES) < 200:
                    print(f"FOUND tiles={tiles} pos={pos(st)} sizes={sizes}")
                    return
                continue
            load(st)
            try:
                c = br(S + 0xA8, 3)
                run = rr(S + 0xA8)
            except IndexError:
                continue
            if c == 5:
                for size in range(0, 321, 4):
                    probe = st.clone()
                    r = step(probe, size)
                    if r is None:
                        continue
                    score2 = abs((tiles + 1 + r[1]) - pos(r[2]) * TARGET_TILES / limit)
                    nxt.append((score2, r[2], tiles + 1 + r[1], sizes + [size]))
            else:
                r = step(st)
                if r is None:
                    continue
                score2 = abs((tiles + 1 + r[1]) - pos(r[2]) * TARGET_TILES / limit)
                nxt.append((score2, r[2], tiles + 1 + r[1], sizes))
        if not nxt:
            break
        nxt.sort(key=lambda e: e[0])
        stats = nxt[:beam]
    print("no exact solution; best states:")
    for score, st, tiles, sizes in stats[:3]:
        print(f"  tiles={tiles} pos={pos(st)}/{limit} sizes={sizes[:12]}...")


if __name__ == "__main__":
    main()
