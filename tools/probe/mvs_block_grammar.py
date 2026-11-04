#!/usr/bin/env python3
"""Portable transcription of MVS DCT block bit consumption.

No framework calls and no oracle sizes are used by block_bits(). Offsets are
relative to stream B. Validation uses the separate recorded size oracle.

Pseudocode (read(n) consumes n bits, MSB first):
    if read(1) == 1: reuse reference block; return
    copy_chroma = read(1)
    param = p2 if read(1) else p1
    if not copy_chroma:
        Cb = i16(2 * (trunc_zero(i16(refCb) / 2) - predictor_symbol()))
        Cr = i16(2 * (trunc_zero(i16(refCr) / 2) - predictor_symbol()))
    else: Cb, Cr = refCb, refCr
    Ydc = i16(refYdc - predictor_symbol())
    index = 1
    while index < 64:
        scale = coefficient_scale(index, param)
        if read(1) == 1:
            k = number of one bits before the next zero
            if index < 6:
                suffix = read(3 if k < 4 else 4)
                magnitude = (4*k+2 if k < 4 else 8*k-14) + (suffix >> 1)
            else:
                suffix = read(min(k+2, 4))
                magnitude = (2 if k == 0 else 4 if k == 1 else 8*k-8)
                magnitude += suffix >> 1
            coefficient[index] = scale * (-magnitude if suffix & 1 else magnitude)
            index += 1
        else:
            code = read(2)
            00: coefficient[index] = 0; index += 1
            10: coefficient[index] = +scale; index += 1
            11: coefficient[index] = -scale; index += 1
            01:
                if read(1) == 0: EOB; return
                run = 3 + read(2)
                if run == 6:
                    repeatedly add read(3) to run until group != 7
                index += run  # zero coefficients; extended-run path rejects >=64

predictor_symbol() is the signed integer code at 0x7ff9223bc1bf, NOT JPEG:
    k = unary ones followed by zero
    k=0: read(1)==0 -> 0; otherwise read sign bit -> +1/-1
    k=1: read(2): magnitude 2 + (suffix>>1), sign suffix&1
    k=2: read(3): magnitude 4 + (suffix>>1), sign suffix&1
    k>=3: read(4): magnitude 8*k-16 + (suffix>>1), sign suffix&1

The native SIMD scan mapping and reference tile reconstruction are outside
this function. Bit consumption matches the recorded oracle for all 2352 rows;
coefficient VALUES above are static-derived, not compared against native output.

Important: the assembly's r12=2/8/16 is a MULTIPLIER, not bits to read:
0x481050/0x481054 multiplies r9 by r12 before writing the coefficient. There
are two loop bodies: indices 1..5 (0x480e12) and 6..63 (0x481217), with
different magnitude codes AND different scale rules. Short-code jump tables
at 0x481a64 and 0x481a74 both map 00/01/10/11 to zero/run/+1/-1.

"""
import argparse
from pathlib import Path


class Bits:
    def __init__(self, data, start=0):
        self.data, self.pos = data, start

    def read(self, n=1):
        if self.pos + n > len(self.data) * 8:
            raise ValueError('truncated payload')
        value = 0
        for _ in range(n):
            value = value * 2 + ((self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return value

    def unary(self, limit):
        ones = 0
        while self.read():
            ones += 1
            if ones >= limit:
                raise ValueError('invalid unary prefix')
        return ones


def predictor_symbol(b):
    k = b.unary(39)
    # 0: zero/nonzero, plus sign for nonzero; other suffixes include sign.
    suffix = b.read(min(k + 1, 4))
    if k == 0:
        return (-1 if b.read() else 1) if suffix else 0
    magnitude = ((1 << k) if k < 3 else (8 * k - 16)) + (suffix >> 1)
    return -magnitude if suffix & 1 else magnitude


def coefficient_scale(index, param):
    """Static transcription of 480e12 and 481217; NOT bit widths."""
    if param < 15:
        return 8 if index < param else 16
    if index < 6:
        return 2
    return 2 if index < param else 8


def consume_block(b):
    if b.read():                         # 480be9: reuse entire block
        return
    copy_chroma = b.read()               # pre-shift bit31
    parameter_select = b.read()          # pre-shift bit30
    if not copy_chroma:
        predictor_symbol(b)             # chroma at +0x80
        predictor_symbol(b)             # chroma at +0x100
    predictor_symbol(b)                 # luma DC at +0
    index = 1
    while index < 64:
        if b.read():                    # non-unit magnitude, 480ef5 / 48130b
            k = b.unary(33 if index < 6 else 19)
            b.read((3 if k < 4 else 4) if index < 6 else min(k + 2, 4))
            index += 1
        else:
            code = b.read(2)            # both dispatch tables: zero/run/+1/-1
            if code != 1:
                index += 1
            elif not b.read():          # 4810ce / 4815a3: EOB
                return
            else:
                run = 3 + b.read(2)
                if run == 6:
                    while True:
                        part = b.read(3)
                        run += part
                        if index + run >= 64:
                            raise ValueError("extended zero run exceeds block")
                        if part != 7:
                            break
                index += run


_PAYLOAD = None


def block_bits(start, payload=None):
    """Return consumed bits, using explicit stream-B bytes or the default capture."""
    global _PAYLOAD
    if payload is None:
        if _PAYLOAD is None:
            body = Path('artifacts/mvs-4.bin').read_bytes()
            _PAYLOAD = body[int.from_bytes(body[3:6], 'big'):]
        payload = _PAYLOAD
    b = Bits(payload, start)
    consume_block(b)
    return b.pos - start


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--body', default='artifacts/mvs-4.bin')
    p.add_argument('--sizes', default='artifacts/mvs-block-sizes.txt')
    args = p.parse_args()
    body = Path(args.body).read_bytes()
    payload = body[int.from_bytes(body[3:6], 'big'):]
    rows = [tuple(map(int, line.split())) for line in Path(args.sizes).read_text().splitlines() if line.strip()]
    matches = 0
    failures = []
    for number, start, expected in rows:
        try:
            actual = block_bits(start, payload)
        except ValueError as e:
            actual = str(e)
        matches += actual == expected
        if actual != expected:
            failures.append((number, start, expected, actual))
    print('matches: %d/%d' % (matches, len(rows)))
    print('first mismatches:', failures[:20])
    raise SystemExit(bool(failures))


if __name__ == '__main__':
    main()
