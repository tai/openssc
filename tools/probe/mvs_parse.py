#!/usr/bin/env python3
"""Prototype MVS command-stream parser, used to pin the DCT block syntax.

Validates itself against the fact that a 2560x1600 frame has exactly 64,000
8x8 tiles: the parse must consume the whole command stream *and* land on that
number.

Usage: python3 tools/probe/mvs_parse.py artifacts/mvs-4.bin
"""
import ctypes
import sys

FW = "/System/Library/PrivateFrameworks/ScreenSharing.framework/Versions/A/ScreenSharing"


def load_tables():
    ctypes.CDLL(FW, mode=ctypes.RTLD_GLOBAL)

    def tbl(bits, syms):
        counts = list(ctypes.string_at(bits, 16))
        total = sum(counts)
        return counts, list(ctypes.string_at(syms, total))

    def canon(counts, symbols):
        codes = {}
        code = 0
        k = 0
        for length in range(1, 17):
            for _ in range(counts[length - 1]):
                codes[(length, code)] = symbols[k]
                k += 1
                code += 1
            code <<= 1
        return codes

    return {
        "ldc": canon(*tbl(0x7FF9224C3480, 0x7FF9224C3491)),
        "lac": canon(*tbl(0x7FF9224C34C0, 0x7FF9224C34E0)),
        "cdc": canon(*tbl(0x7FF9224C34A0, 0x7FF9224C34B1)),
        "cac": canon(*tbl(0x7FF9224C3590, 0x7FF9224C35B0)),
    }


class Bits:
    def __init__(self, data):
        self.data = data
        self.pos = 0
        self.n = len(data) * 8

    def bit(self):
        if self.pos >= self.n:
            raise IndexError
        byte = self.data[self.pos >> 3]
        v = (byte >> (7 - (self.pos & 7))) & 1
        self.pos += 1
        return v

    def read(self, n):
        v = 0
        for _ in range(n):
            v = (v << 1) | self.bit()
        return v

    def huff(self, table):
        code = 0
        for length in range(1, 17):
            code = (code << 1) | self.bit()
            if (length, code) in table:
                return table[(length, code)]
        raise ValueError("bad huffman code")


def parse(path, opts):
    d = open(path, "rb").read()
    doff = (d[3] << 16) | (d[4] << 8) | d[5]
    b = Bits(d[6:doff])
    t = load_tables()

    tiles = 0
    cmds = 0
    codes = {}
    try:
        while True:
            c = b.read(3)
            codes[c] = codes.get(c, 0) + 1
            cmds += 1
            run = 0
            if b.bit():
                v = b.read(4)
                if v < 15:
                    run = v + 1
                else:
                    run = 16 + b.read(8)
            tiles += 1 + run

            if c == 3:
                for _ in range(8):
                    b.read(8)
                b.read(20)
                b.read(20)
            elif c == 4:
                b.read(opts["p4"])
            elif c == 5:
                dct(b, t, opts)
            elif c == 6:
                b.read(16)
    except IndexError:
        pass
    return tiles, cmds, b.pos, codes


def dct(b, t, opts):
    """Best-effort DCT block: flag, two mode bits, predictors, AC loop."""
    flag = b.bit()
    if flag:
        return
    b1 = b.bit()
    b2 = b.bit()
    if b1 == 0:
        b.huff(t[opts["dc"]])
        b.huff(t[opts["dc"]])
    b.huff(t[opts["dc"]])
    # AC loop (guess): pairs of (run,size) until EOB, with values of `size` bits
    k = 1
    while k < 64:
        sym = b.huff(t[opts["ac"]])
        run, size = sym >> 4, sym & 0xF
        if size == 0:
            if run == 15:
                k += 16
                continue
            break
        b.read(size)
        k += run + 1


def main():
    opts = {"p4": 22, "dc": "ldc", "ac": "lac"}
    tiles, cmds, pos, codes = parse(sys.argv[1], opts)
    print(f"tiles={tiles} (target 64000) cmds={cmds} pos={pos} codes={codes}")


if __name__ == "__main__":
    main()
