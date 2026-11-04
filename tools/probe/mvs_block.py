#!/usr/bin/env python3
"""Full MVS frame walk using the framework's own readers and block decoder.

Confirmed layout (Codex, 2026-11-04):
  stream A = body[6:data_off]   commands only: 1 flag bit, then [cmd3][run]*
  stream B = body[data_off:]    every command payload

Payload widths:
  code 0 fill / 1 copy-above / 2 copy-previous / 7 cache7 : 0 bits
  code 3 : 8-bit row mask, then one 8-bit row per clear bit
  code 4 : flag b1, flag b2; if !b2 read (1+b1) colours of Y8/Cb6/Cr6;
           if b1 also read the 8-bit mask + one 8-bit row per clear bit
  code 5 : DCT block via 0x7ff922480b28 (variable; decoded by the framework)
  code 6 : 16-bit cache index (hi byte, lo byte)

Usage: python3 tools/probe/mvs_block.py artifacts/mvs-4.bin
"""
import ctypes
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"
ACC_OFF, AVAIL_OFF, PTR_OFF = 0xA8, 0xAC, 0xB0
STATE_SIZE = 0x1000


class State:
    def __init__(self, data):
        self.buf = ctypes.create_string_buffer(STATE_SIZE)
        self.addr = ctypes.addressof(self.buf)
        self.nbits = len(data) * 8
        self.data = ctypes.create_string_buffer(data + bytes(4096), len(data) + 4096)
        self.base = ctypes.addressof(self.data)
        ctypes.c_uint32.from_address(self.addr + ACC_OFF).value = 0
        ctypes.c_int32.from_address(self.addr + AVAIL_OFF).value = 0
        ctypes.c_void_p.from_address(self.addr + PTR_OFF).value = self.base

    @property
    def reader(self):
        return self.addr + ACC_OFF

    def pos(self):
        ptr = ctypes.c_void_p.from_address(self.addr + PTR_OFF).value
        avail = ctypes.c_int32.from_address(self.addr + AVAIL_OFF).value
        return (ptr - self.base) * 8 - avail

    def total(self):
        return self.nbits


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
    raise SystemExit("no framework image")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "artifacts/mvs-4.bin"
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    base = base_addr()

    BitReader = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32)
    ReadRun = ctypes.CFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p)
    Block = ctypes.CFUNCTYPE(
        ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
        ctypes.c_int, ctypes.c_int, ctypes.c_void_p,
        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p)
    br = BitReader(base + (0x7FF922480895 - 0x7FF9223A3000))
    rr = ReadRun(base + (0x7FF92248090C - 0x7FF9223A3000))
    blk = Block(base + (0x7FF922480B28 - 0x7FF9223A3000))

    commands = State(d[6:doff])
    payload = State(d[doff:])

    out = ctypes.create_string_buffer(0x200)
    ref = ctypes.create_string_buffer(0x200)
    side1 = ctypes.create_string_buffer(0x200)
    side2 = ctypes.create_string_buffer(0x200)
    side3 = ctypes.create_string_buffer(0x200)

    def rd(n):
        return br(payload.reader, n)

    br(commands.reader, 1)  # leading flag bit

    dump = "--dump" in sys.argv
    rows = []
    tiles = 0
    cmds = 0
    codes = {}
    blocks = 0
    block_bits = 0
    try:
        while tiles < 64000:
            c = br(commands.reader, 3)
            run = rr(commands.reader)
            cmds += 1
            codes[c] = codes.get(c, 0) + 1
            for _ in range(run + 1):
                if c == 3:
                    m = rd(8)
                    for row in range(8):
                        if not m & (128 >> row):
                            rd(8)
                elif c == 4:
                    bitmap = rd(1)
                    reuse = rd(1)
                    if not reuse:
                        for _color in range(1 + bitmap):
                            rd(8)
                            rd(6)
                            rd(6)
                    if bitmap:
                        m = rd(8)
                        for row in range(8):
                            if not m & (128 >> row):
                                rd(8)
                elif c == 5:
                    before = payload.pos()
                    rc = blk(payload.addr, ctypes.cast(out, ctypes.c_void_p),
                             ctypes.cast(ref, ctypes.c_void_p), d[1], d[2],
                             ctypes.cast(side3, ctypes.c_void_p),
                             ctypes.cast(side1, ctypes.c_void_p),
                             ctypes.cast(side2, ctypes.c_void_p),
                             ctypes.cast(side3, ctypes.c_void_p))
                    if rc != 0:
                        raise SystemExit(f"block decode failed rc={rc} at tile {tiles}")
                    if dump:
                        rows.append((blocks, before, payload.pos() - before))
                    blocks += 1
                    block_bits += payload.pos() - before
                elif c == 6:
                    rd(16)
                tiles += 1
                if tiles >= 64000:
                    raise StopIteration
    except (IndexError, StopIteration):
        pass

    print(f"tiles={tiles} (target 64000) commands={cmds} codes={codes}")
    print(f"blocks={blocks} block_bits={block_bits}")
    print(f"stream A {commands.pos()}/{commands.total()} bits")
    print(f"stream B {payload.pos()}/{payload.total()} bits "
          f"({payload.total() - payload.pos()} left)")
    a_end = commands.pos()
    b_end = payload.pos()
    # Cross-checks against the independently validated command parser
    # (tools/probe/mvs_commands.py, evidence artifacts/codex-mvs-commands.txt).
    EXPECT_TILES, EXPECT_CMDS, EXPECT_BLOCKS = 64000, 1534, 2352
    assert tiles == EXPECT_TILES, f"tiles {tiles} != {EXPECT_TILES}"
    assert cmds == EXPECT_CMDS, f"commands {cmds} != {EXPECT_CMDS}"
    assert blocks == EXPECT_BLOCKS, f"blocks {blocks} != {EXPECT_BLOCKS}"
    # Each stream is terminated by a BIT-ALIGNED 0x6d marker followed only by
    # zero padding. Markers are not byte-aligned, so consume them through the
    # readers at the exact bit positions (never bytes.find).
    a_marker = br(commands.reader, 8)
    b_marker = br(payload.reader, 8)
    assert a_marker == 0x6D, f"A marker 0x{a_marker:02x} != 0x6d at bit {a_end}"
    assert b_marker == 0x6D, f"B marker 0x{b_marker:02x} != 0x6d at bit {b_end}"

    def leftover_zeros(state):
        left = state.total() - state.pos()
        while left >= 8:
            assert br(state.reader, 8) == 0, "non-zero padding"
            left -= 8
        return state.total() - state.pos()

    a_pad = leftover_zeros(commands)
    b_pad = leftover_zeros(payload)
    assert a_pad < 8 and b_pad < 8, f"padding {a_pad}/{b_pad} bits"
    if dump:
        out_path = "artifacts/mvs-block-sizes.txt"
        with open(out_path, "w") as fh:
            for idx, start, size in rows:
                fh.write(f"{idx}\t{start}\t{size}\n")
        print(f"wrote {out_path} ({len(rows)} blocks)")
    print(f"assertions OK: markers 0x6d at A bit {a_end} and B bit {b_end}, "
          f"padding {a_pad}/{b_pad} bits")


if __name__ == "__main__":
    main()
