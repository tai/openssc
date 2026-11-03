# Apple framebuffer codecs

Status: **quality tiers (0x3e8/0x3e9/0x3ea) VERIFIED**, **MultiVariant (0x3f3)
container VERIFIED, tile bitstream still being reverse engineered**.

Measured on `mm.local` (2560×1600, macOS 12, LAN) — time for the server to
produce one full-screen update:

| Encoding | Full frame time | Bytes on the wire |
|---|---|---|
| Raw `0x00` | **1590 ms** | 16,384,000 |
| Zlib `0x06` | **880 ms** | ~600 KB – 4 MB |
| `0x3ea` (RGB555) | **700 ms** | 632,819 |
| `0x3e9` (8-bit YCoCg) | **700 ms** | 125,065 |
| `0x3e8` (4-bit) | **700 ms** | 33,474 |
| **MVS `0x3f3`** | **60 ms** | 86,029 |

Two conclusions:

1. Every deflate tier hits the same ~700 ms floor regardless of payload size
   (33 KB and 632 KB both take 700 ms), so the cost is the server's per-pixel
   conversion, not compression. Changing the encoding does **not** reduce
   latency, only bandwidth.
2. MVS is 12× faster because the server encodes it on the GPU (OpenCL DCT
   kernels). MVS is the only path to a responsive mirror.

## 1. Quality tiers (0x3e8 / 0x3e9 / 0x3ea)

Advertising one of these makes the server use it. Framing matches Zlib: a
`u32 compressed_len` followed by a deflate stream, but the stream is
**persistent across rectangles** (same as `0x06`) — a fresh decompressor per
rect works only for the first, full-screen rectangle and then fails.

`0x3ea` pixel format, verified numerically against a `screencapture` reference:

- **RGB555, big-endian words**, bit 15 unused. Not 5-6-5.
- `0x7fdf` → white `(255,255,255)`
- `0x7d8b` → `(255,99,90)` for a button whose real colour is `(255,95,87)`
- `0x284f` → `(82,16,123)` for a purple whose real colour is `(85,19,125)`

Decoding `0x3ea` with RGB565 produced plausible-looking but wrong colours
(warm cast, hue-shifted gradients), which is how the 555 layout was found.

`0x3e9` and `0x3e8` are accepted by the server but their pixel layouts are not
decoded yet; they are kept as opaque payloads for experiments.

## 2. MultiVariant (`0x3f3`)

Delivered as ordinary TCP framebuffer rectangles: `w`, `h`, `encoding` and a
`u32 body_len`, then a self-describing body. It is **independent of the
AVConference/UDP HEVC path** and needs no `0x1c` handshake.

### 2.1 Container — VERIFIED from captures

Body (after the `u32` length) starts with:

```
+0x00 u8     type_of_update     0 = partial/full tile stream, 2 = quant tables
+0x01 u8     p1                  observed 0x0f
+0x02 u8     p2                  observed 0x19
+0x03 u8[3]  data_off           BE, offset from body start to the data stream
+0x06 ...    command bitstream
+data_off    data stream
```

A `type_of_update = 2` message is exactly **129 bytes**: the type byte followed
by 64 bytes of luma and 64 bytes of chroma quantization values. It always
precedes the first tile stream. Example from `artifacts/mvs-3.bin`.

Real captures (`artifacts/mvs-4.bin`, 2560×1600): `type=0, p1=0x0f, p2=0x19,
data_off=4513`, total 85,987 bytes.

### 2.2 Tile bitstream — OPEN

The layout is documented from static analysis of `EncodePartialUpdateMVS`
(`docs/protocol/input-and-gestures.md` §8.9.4 of the RFC notes):

- commands 0–7: Skip, MatchPrevious, MatchAbove, TwoColor (8-byte mask + two
  YCbCr 8/6/6 colours), Solid (one colour), DCT (quantized coefficients),
  Cache6 (16-bit index), Cache7 (implicit previous+1);
- colours packed as `Y(8) << 12 | Cb(6) << 6 | Cr(6)`;
- run-length repeat gated by a 5-bit field (`0x10 | (n-1)` for n ≤ 15, `0x1f`
  followed by a base-128 varint otherwise);
- the block ends with the marker `0x6d 0x76 0x73` (`"mvs"`).

What has been established empirically:

- reading 3 bits LSB-first from the first command byte (`0x41`) yields code 4,
  which matches the documented "Solid" position in the list;
- the 20 bits that follow do **not** decode to the expected top-left colour
  under either YCbCr channel order, so the field order (or the position of the
  repeat field) is still wrong;
- our captured full frame does **not** end with `6d 76 73`, so either the
  marker applies to a different stream shape or this build omits it.

### 2.3 Encoder map (lab build, macOS 12 `screensharingd` 3.9.8)

The daemon is stripped, so entry points were recovered from RIP-relative
references to its own `__cstring` symbols. Working copy:
`artifacts/bin/screensharingd` (universal), x86_64 slice
`artifacts/bin/ssd-x86`, full listing `artifacts/bin/ssd.asm`, and focused
extracts in `artifacts/bin/{MVSSetup,EncodePartialUpdateMVS,EncodeMVS}.asm`.

| Address | Role |
|---|---|
| `0x10002080c` | `cmpl $1011, 140(%r13)` — encoding dispatch, calls the MVS setup on match |
| `0x10003b128` | MVS setup: logs `reset the multi variant info`, `AllocateMultiVariantCodecMemoryBuffers` |
| `0x10003b539` | `EncodePartialUpdateMVS` (string ref at `0x10003c2f1`) |
| `0x10003d0f7` | `EncodeMVS` (string ref at `0x10003e796`) |
| `0x10003d0a7` | codec selection: logs `higher quality DCT codec`, `DetermineEncodingParameters` |
| `0x10001b56a` | reference to `DCTHuffmanEncodePartialUpdate` |

Tooling: `lipo -thin x86_64`, `otool -l` for `__TEXT` (`vmaddr 0x100000000`,
`fileoff 0`, so file offset == vmaddr − 0x100000000), `objdump -d`, then scan
for `leaq disp(%rip)` whose target lands on a known string address.

### 2.4 Encoder internals recovered (2026-11-04)

**Bit writer** — `BitWriteStoreBits` at `0x10003c89c`
(listing: `artifacts/bin/BitWriter.asm`). Signature `(state, value: u32,
bits: u32)`, state = `{ u8 *ptr; u32 acc; int free; }`:

```
free -= bits;
if (free <= 0) {
    ptr[0] = acc >> 24;      // top byte
    ptr[1] = byte2(acc);     // next byte  (movb 10(%rdi) is inside acc!)
    ptr += 2;
    acc <<= 16;
    free += 16;
}
acc |= value << (free & 31);
```

So the command/data bitstreams are **16-bit big-endian units, MSB-first**:
read a `u16` big-endian, then consume bits from the most significant end. This
corrects the earlier "byte-oriented MSB-first" assumption and is the single most
important detail for the decoder.

**Tile cache** — `0x10003c9d0` (listing `artifacts/bin/BitWriter.asm` tail):

- 65536-entry `u16` index table, hashed with `(x * 0xF69A2ED1) >> 49` and
  `% 136067`;
- entries are **104 bytes**: a `u32` key followed by 100 bytes of tile data;
- the tile copy is 96 bytes of pixels plus the key, and 96 = **64 luma + 16 Cb +
  16 Cr**, i.e. chroma is **4:2:0 subsampled** on 8×8 tiles;
- a per-viewer counter at `+2474` allocates cache slots, incrementing and
  wrapping at 65000 — that is the 16-bit index used by the Cache6 command.

**Encoder buffer / geometry** — `EncodePartialUpdateMVS` at `0x10003b539`:

- the rectangle argument is `{i16 left, top, right, bottom}` (the encoder
  computes `right-left` and `bottom-top`), not `x,y,w,h`;
- the working buffer is `((2*w + 14) & ~15) * ((h + 7) & ~7) + 520` bytes, i.e.
  **2 bytes per pixel**, tile height rounded to 8, row stride rounded to 16
  bytes;
- the first `callq 0x10003c377` allocates/returns the codec state; the
  quantisation-table message is emitted separately (`EncodeMVSQuantizationTables`).

**Tile loop** (around `0x10003b9e0`–`0x10003bb20`): the loop first writes a
**6-bit value** `(n == -1 ? 63 : n - 1)`, then per tile writes a **3-bit code**
taken from a runtime-built byte table at `-240(%rbp)` (a negative byte is an
escape and selects a different bit width), followed by 1-, 2- and 8-bit fields
and calls to `0x10003cc71` for the coefficient bits. This is the Huffman/escape
machinery; its table is built per call, not stored statically, so it has to be
derived from the algorithm rather than lifted from the binary.

The 64-entry table built just before the loop (`0x10003b91d`–`0x10003b9d1`)
is a per-call run-length/Huffman description of the 64 DCT coefficients: it
walks the array at `4(%r12)` and for each entry derives a code in `-239(%rbp,…)`,
marks zeros with `0x80`, and records the first non-zero index in `r15`, which is
then emitted as the 6-bit `(n == -1 ? 63 : n - 1)` prefix. Coefficients beyond
the first non-zero use a second 3-bit code path. This is a custom (not JPEG)
run-length scheme fused with the escape table, and it is the remaining piece
that must be reproduced exactly.

Next: either finish the loop above, or — cheaper — extract the *client's*
Huffman tables from `ScreenSharing.framework` in the dyld shared cache. The
framework loads via `dlopen`, so its Mach-O header can be reached with
`_dyld_get_image_header`, then `__DATA`/`__CONST` scanned for a
`(bits[16], values[…])` pair, which is the usual shape of such tables.

## 3. Repro

```sh
# Quality tier (decodes to correct pixels today)
cargo run -p ss-client -- stream --host mm.local --encoding 1002 --frames 3

# MVS: capture a quant-table message + a full tile stream
cargo run -p ss-client -- stream --host mm.local --encoding 1011 --frames 4
# writes artifacts/mvs-N.bin for each 0x3f3 rect

# Reference for pixel comparison
ssh tai@mm.local 'screencapture -x /tmp/ref.png'
```
