#!/usr/bin/env python3
"""MVS coefficient VALUES; portable decode plus optional native differential check.

Run: python3 tools/probe/mvs_block_values.py --validate-native
Only this validation option calls the macOS framework. decode_block() is portable.
Detailed rendering semantics and evidence are in NOTES below.
"""
import argparse
import ctypes
import random
import struct
from pathlib import Path
from mvs_block_grammar import Bits, predictor_symbol, coefficient_scale

# Framework 0x7ff9224c2340, 64 u8. Standard JPEG zigzag TRANSPOSED.
SCAN = (0,8,1,2,9,16,24,17,10,3,4,11,18,25,32,40,33,26,19,12,5,6,
        13,20,27,34,41,48,56,49,42,35,28,21,14,7,15,22,29,36,43,50,
        57,58,51,44,37,30,23,31,38,45,52,59,60,53,46,39,47,54,61,62,55,63)


# Default u8 tables: state+8 points to A (luma), state+0x10 to B (Cb/Cr).
QUANT_LUMA = (
    16,11,11,14,24,22,24,33,11,12,11,11,26,18,19,23,
    13,13,14,24,20,21,22,28,13,17,14,21,23,25,34,34,
    17,22,18,25,31,32,34,44,24,20,17,27,35,40,44,54,
    30,35,28,30,34,44,54,64,35,30,33,38,45,55,65,75)
QUANT_CHROMA = (
    19,19,24,47,76,99,99,99,19,21,26,66,99,99,99,99,
    24,26,56,99,99,99,99,99,47,66,99,99,99,99,99,99,
    76,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,
    99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99)


def i16(x):
    return ((x + 32768) & 65535) - 32768


def half_toward_zero(x):
    x = i16(x)
    return (x + (x < 0)) >> 1


def decode_block(payload, start, reference, p1=15, p2=25):
    """Return (192 i16 coefficients, consumed bits), stored in SIMD plane order.

    reference is 192 i16 words. The 0x180-byte buffer is three 8x8 i16 planes,
    NOT a 96-byte 4:2:0 sample tile. This block only codes chroma DC coefficients.
    """
    b = Bits(payload, start)
    out = [0] * 192
    if b.read():
        out[:64] = reference[:64]
        out[64], out[128] = reference[64], reference[128]
        return out, b.pos - start
    copy_chroma = b.read()
    param = p2 if b.read() else p1
    if copy_chroma:
        out[64], out[128] = reference[64], reference[128]
    else:
        out[64] = i16(2 * (half_toward_zero(reference[64]) - predictor_symbol(b)))
        out[128] = i16(2 * (half_toward_zero(reference[128]) - predictor_symbol(b)))
    out[0] = i16(reference[0] - predictor_symbol(b))
    index = 1
    while index < 64:
        scale = coefficient_scale(index, param)
        if b.read():
            k = b.unary(33 if index < 6 else 19)
            if index < 6:
                suffix = b.read(3 if k < 4 else 4)
                magnitude = (4*k+2 if k < 4 else 8*k-14) + (suffix >> 1)
            else:
                suffix = b.read(min(k+2, 4))
                magnitude = (2 if k == 0 else 4 if k == 1 else 8*k-8) + (suffix >> 1)
            value = -magnitude if suffix & 1 else magnitude
        else:
            code = b.read(2)
            if code == 1:
                if not b.read():
                    break
                run = 3 + b.read(2)
                if run == 6:
                    while True:
                        part = b.read(3)
                        run += part
                        if index + run >= 64:
                            raise ValueError('extended run outside block')
                        if part != 7:
                            break
                index += run
                continue
            value = (0, None, 1, -1)[code]
        out[SCAN[index]] = i16(scale * value)
        index += 1
    return out, b.pos - start


def quant_transpose(table):
    """Native expanded table[64:128], u16: transpose, then multiply by 16."""
    result = [0] * 64
    for j, value in enumerate(table):
        result[((j & 7) << 3) | (j >> 3)] = value << 4
    return result


def sat16(x):
    return max(-32768, min(32767, x))


def rgb_from_idct(y, cb, cr):
    """Exact scalar transcription of 3bcb21 SSE path; inputs are i16, scale64."""
    y = i16(y + 8192)
    green = sat16(sat16(y + ((cb * -22554) >> 16)) +
                  sat16(((cr * 18734) >> 16) - cr))
    blue = sat16(sat16(y + cb) + sat16(cb + ((cb * -14942) >> 16)))
    red = sat16(sat16(y + cr) + ((cr * 26345) >> 16))
    return tuple(max(0, min(255, sat16(v + 32) >> 6)) for v in (red, green, blue))


def native_validate(body_path, sizes_path):
    from mvs_oracle import image_base
    from mvs_block import State
    base = image_base()
    addr = lambda a: base + a - 0x7ff9223a3000
    Block = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p,
                            ctypes.c_void_p, ctypes.c_int, ctypes.c_int,
                            ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p)
    native = Block(addr(0x7ff922480b28))
    body = Path(body_path).read_bytes()
    payload = body[int.from_bytes(body[3:6], 'big'):]
    rows = [tuple(map(int, line.split())) for line in Path(sizes_path).read_text().splitlines() if line.strip()]
    out = ctypes.create_string_buffer(0x200)
    side1 = ctypes.create_string_buffer(0x200)
    side2 = ctypes.create_string_buffer(0x200)
    side3 = ctypes.create_string_buffer(0x200)
    rng = random.Random(1234)
    cases = matches = 0
    for mode in ('zero', 'signed-random'):
        for number, start, expected_bits in rows:
            ref = [0]*192 if mode == 'zero' else [rng.randrange(-32768,32768) for _ in range(192)]
            refbuf = ctypes.create_string_buffer(struct.pack('<192h', *ref))
            st = State(payload)
            # Position reader exactly, without using recorded consumed_bits.
            pos = start // 8
            ctypes.c_void_p.from_address(st.addr+0xb0).value = st.base+pos
            if start & 7:
                br = ctypes.CFUNCTYPE(ctypes.c_uint32,ctypes.c_void_p,ctypes.c_uint32)(addr(0x7ff922480895))
                br(st.reader, start & 7)
            rc = native(st.addr,out,refbuf,body[1],body[2],addr(0x7ff9224c2340),side1,side2,side3)
            values, consumed = decode_block(payload,start,ref,body[1],body[2])
            actual = struct.unpack('<192h',out.raw[:0x180])
            cases += 1
            ok = rc == 0 and consumed == expected_bits and tuple(values)==actual
            matches += ok
            if not ok:
                diffs=[(i,a,v) for i,(a,v) in enumerate(zip(actual,values)) if a!=v]
                print('mismatch',mode,number,'bits',consumed,expected_bits,'values',diffs[:10])
                if cases-matches >= 10:
                    print('stopping after 10 mismatches');return False
    print('Native coefficient/size matches: %d/%d (%d words each)' % (matches,cases,192))
    assert tuple(ctypes.string_at(addr(0x7ff9224c2340),64)) == SCAN
    assert tuple(ctypes.string_at(addr(0x7ff9224c2390),64)) == QUANT_LUMA
    assert tuple(ctypes.string_at(addr(0x7ff9224c23d0),64)) == QUANT_CHROMA
    convert = ctypes.CFUNCTYPE(None,ctypes.c_void_p,ctypes.c_void_p,
                              ctypes.c_void_p,ctypes.c_int)(addr(0x7ff9223bcb21))
    pixels = 0
    for _ in range(100):
        spatial = [rng.randrange(-32768,32768) for _ in range(192)]
        samples = (ctypes.c_int16*192)(*spatial)
        argb = ctypes.create_string_buffer(256)
        convert(None,samples,argb,32)
        for j in range(64):
            rgb = rgb_from_idct(spatial[j],spatial[j+64],spatial[j+128])
            assert argb.raw[j*4:j*4+4] == bytes((255,)+rgb), (j,rgb,list(argb.raw[j*4:j*4+4]))
            pixels += 1
    print('Native RGB helper matches: %d/%d signed-input pixels' % (pixels,pixels))
    print('Native scan and both default quantisation tables match constants')
    return matches == cases


NOTES = r'''
VALUE SEMANTICS — 2026-11-04 Codex/OpenCode collaboration

1. Coefficients (VERIFIED against native output):
   decode_block() implements the signed magnitudes and scale exactly. All
   stores wrap to i16, including DC and multiplier*AC. scale(index,param):
       param < 15:  8 when index<param, otherwise 16
       param >=15 and index<6: 2
       param >=15 and index>=6: 2 when index<param, otherwise 8
   Magnitude suffix low bit is sign (1 negative). Unit symbols select +/-scale.
   No predictor is added to AC values. Chroma AC remains zero.

2. Scan (VERIFIED by all-192-word native comparison):
   SCAN is the 64-byte table at 0x7ff9224c2340, supplied as argument 6/r9 to
   block decoder. At 48105f/481063, coded index i is mapped through SCAN[i],
   then coefficient stored as i16 at output+2*SCAN[i]. This is TRANSPOSED
   standard JPEG zigzag: [0,8,1,2,9,16,...], not [0,1,8,16,9,2,...].
   For a conventional scalar IDCT, convert coefficient matrix back to normal
   row-major order: normal[v*8+u] = output[u*8+v]. Native IDCT expects the
   transposed storage directly. Native impulse test confirms stored index8
   varies spatially across x, stored index1 varies across y.

3. DC (VERIFIED against native with zero and random signed16 references):
   byte offsets +0=Y DC; +0x80=Cb DC; +0x100=Cr DC.
   first bit1: copy reference Y[64] and both chroma DC words; zero chroma AC.
   first bit0: copy_chroma bit, then param_select bit (0=p1, 1=p2).
   if copy_chroma==0 read signed predictor symbols in Cb, Cr order:
       Cb = i16(2 * (trunc_towards_zero(i16(refCb)/2) - deltaCb))
       Cr = i16(2 * (trunc_towards_zero(i16(refCr)/2) - deltaCr))
   otherwise copy these two words unchanged.
   Then always read signed Y delta:
       Ydc = i16(refYdc - deltaY)
   The native half computation uses logical shift of the raw u16 plus its
   sign-bit correction; after doubling and i16 wrapping it equals the formula
   above, including odd negative inputs. Important: these are SUBTRACTIONS.
   References are the caller-owned previous coefficient tile. Choosing and
   updating that reference across fill/copy/cache/solid commands is the frame
   walker's responsibility; this function does not infer that caller state.

4. Quantisation and IDCT:
   Default A at 4c2390 -> state+8 -> Y. B at 4c23d0 -> state+0x10 -> Cb/Cr.
   QUANT_LUMA/CHROMA above contain those actual bytes (do not substitute an
   approximate JPEG luma table). Per-session quant-table messages can replace
   the defaults. Type2 update handler at 3b8a20 reads body[1:65] into state+8
   (Y), and the following table body[65:129] into state+0x10 (Cb/Cr), using
   the same expansion/transpose. IMPORTANT for this rendering experiment:
   artifacts/mvs-3.bin contains DIFFERENT tables from the built-in defaults:
   Y starts12,9,9,11,...; chroma starts15,15,18,36,... . Use those captured
   tables for mvs-4.bin after processing the preceding type2 update, rather
   than blindly using QUANT_LUMA/CHROMA constants.
   Initialisation at 3bb489 and 3bb4e0 allocates 128 u16 words per table:
       expanded[j] = raw[j]                         # first 64 words
       expanded[64 + transpose(j)] = raw[j] << 4     # second 64 words
       transpose(j) = ((j & 7)<<3) | (j>>3)
   This is a matrix TRANSPOSE, not zigzag reorder. The scan and quant transpose
   are separate operations. The SIMD IDCT at 3bc67d reads expanded+0x80 bytes
   and pmullw multiplies by the already-transposed coefficient plane:
       dequant_simd[j] = i16(coeff_simd[j] * expanded[64+j])
   Then a fixed-point SSE transform uses pmulhw, wrapping and saturating adds.
   It is not bit-equivalent to a floating IDCT; avoid silently dropping its
   low-word product wrap if implementing exact behaviour.
   For initial rendering without exact SIMD emulation, in normal matrix order:
       f[y,x] = (1/4) * sum_uv C(u)*C(v)*coeff[v,u]*rawQ[v,u]
                        *cos((2*x+1)*u*pi/16)*cos((2*y+1)*v*pi/16)
       C(0)=1/sqrt(2), C(nonzero)=1
       idct_sample ~= 64*f[y,x]   # centered; do not yet add128 or clamp to u8
   That is an APPROXIMATION for non-overflowing products, not a validated exact
   replacement. DC probe with lumaQ[0]=16: native DC1 ->127 at every sample,
   DC2 ->255, DC100 ->12799, DC-1 ->-129; DC0 ->0. Ideal values are128*DC.
   Thus final colour samples are scaled64, and the native fixed point bias can
   be under one sample unit. No extra factor16 should be applied to floating
   IDCT when using raw Q: the <<4 is part of the native fixed-point transform.

5. Chroma / buffer layout — VERIFIED for this block function:
   Reject the prior 96-byte/4:2:0 inference for the 0x180-byte block buffer.
   It contains192 i16 words, THREE8x8 coefficient matrices:
       Y : bytes 0x000..0x07f (64 words)
       Cb: bytes 0x080..0x0ff (64 words)
       Cr: bytes 0x100..0x17f (64 words)
   This DCT grammar writes only Cb[0] and Cr[0]; other chroma coefficients zero.
   Therefore chroma is spatially constant within this8x8 tile after IDCT.
   Wrapper 3bc594 calls8x8 IDCT three times: Y with state+8 Q, Cb/Cr with
   state+0x10 Q, outputting three full64-word spatial planes. No4x4 chroma
   upsampling occurs in this path. This does not rule out a different format
   in the server tile cache or another MVS coding path.

6. Colour conversion (helper 3bcb21):
   rgb_from_idct() is an exact scalar transcription of its SSE arithmetic.
   Inputs Y,Cb,Cr are signed i16 spatial outputs at scale64; Y is centered,
   Cb/Cr are zero-centered. It wraps Y+8192 (128*64) as i16, applies signed
   high-word multiplies and saturating adds, rounds with sat16(+32), then
   arithmetic-shifts6 and saturates unsigned8.
   Approximate familiar coefficients: R=Y+1.402*Cr,
       G=Y-0.34414*Cb-0.71414*Cr, B=Y+1.772*Cb, with Y+=128.
   Integer constants are -22554,18734,-14942,26345 (see function).
   VERIFIED: scalar function matches6400 native pixels with random full-range
   signed16 inputs, covering saturation and wrapping as well as normal colours.
   Helper writes ARGB bytes [255,R,G,B]. Wrapper 3bc622..3bc654 applies the
   shuffle [3,2,1,0,7,6,5,4,...] from4c1000, yielding [B,G,R,255].
   Native probes: centered(0,0,0)->RGB128,128,128;
       (Y=0,Cb=640,Cr=0)->RGB128,125,146;
       (Y=0,Cb=0,Cr=640)->RGB142,121,128.

Evidence: artifacts/mvs-block-sizes.txt + mvs-4.bin, framework block480b28,
scan4c2340, quant init3bb400, wrapper3bc594, SIMD IDCT3bc67d, RGB3bcb21.
Run --validate-native for4704 exact coefficient/size comparisons. This does
not claim a rendered PNG or white.png pixel comparison; frame-level reference
state, transmitted quant updates, and fixed-point IDCT remain to integrate.
'''

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--validate-native',action='store_true')
    p.add_argument('--body',default='artifacts/mvs-4.bin')
    p.add_argument('--sizes',default='artifacts/mvs-block-sizes.txt')
    args=p.parse_args()
    if args.validate_native:
        raise SystemExit(not native_validate(args.body,args.sizes))
    print(NOTES)
