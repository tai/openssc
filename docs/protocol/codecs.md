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

### 2.5 Command stream — framing VERIFIED (2026-11-04)

The command writer (`0x100041e89`) keeps, per viewer, the *last code*
(`+2366`) and a run counter (`+2368`):

```
if (code == last_code)  count++;
else {
    if (count != 0) { write_repeat(count); write_3bits(code); count = 0; }
    else            { write_4bits(code); }        // only the very first command
    last_code = code;
}
```

so the stream is `code, payload, repeat, code, payload, repeat, …`, i.e. each
run's length is emitted just before the next code. Only the **first** code is
4 bits wide; every later code is 3 bits.

`write_repeat` (`0x10003fe11`):

```
n <= 15 : 5 bits, value 0x10 | (n-1)        // 16..30
n >  15 : 5 bits, value 0x1f, then base-128 varint of (n-16)
          (8 bits per group; high bit 0x80 = continuation)
```

Payload widths recovered so far: **Solid = 24 bits**, **Cache6 = 16 bits**
(tile-cache index), Skip / MatchPrevious / MatchAbove / Cache7 = 0 bits,
TwoColor presumed 8-byte mask + two 24-bit colours, **DCT = variable** (see
below).

Partial parses of a real 2560×1600 frame are self-consistent but not yet
confirmed:

- reading a 4-bit first code gives `Solid`, a 216-tile run, then `DCT` at bit
  52;
- reading a 4-bit header then 3-bit codes gives `Skip×10, Cache6×15,
  MatchPrevious×9`, then desynchronises at bit 52.

Both hypotheses reach the **same bit position** and fail at the same place, and
the first Solid colour (0x19d841) does **not** match the reference menu-bar grey
(218,218,218), so the colour packing is still unconfirmed. The consistent
failure point is the first DCT payload, i.e. the coefficient coder is the single
blocker for both parsing and decoding.

**DCT payloads are inline in the same stream** (stream A): the coefficient
coder writes luma coefficients to it and chroma to the second stream, which
begins at `data_off`. That is why a command parse desynchronises at the first
DCT unless the coefficient coder is implemented.

### 2.6 Coefficient VLC table — recovered

The coefficient writer (`0x10003cc71`) looks up `magnitude (1..63)` in a static
**256-entry `u32` table at `0x100085b90`**: the top 4 bits are the code length,
the rest the code, and the low bit of the code carries the sign.

| magnitude | bits | code |
|---|---|---|
| 1 | 4 | 0x8 |
| 2 | 4 | 0xa |
| 3–6 | 6 | 0x30,0x32,0x34,0x36 |
| 7–14 | 8 | 0xe0…0xee (step 2) |
| 15–22 | 9 | 0x1e0… |
| … | 10,11,12,13,14 bits ×8 each | |
| | 15 bits ×1 | |

Larger magnitudes use an escape (`0x10003cd26`): 8-bit groups with a
continuation bit.

## 2.7 Client decoder — FOUND (2026-11-04)

Earlier searches of `ScreenSharing.framework` were invalid: shared-cache images
must be read at `vmaddr + slide` (slide `0x8fae000` this boot).
`tools/instrument/framework_image.py` now applies it, and with correct
addresses the client decoder is present:

| Symbol | Address |
|---|---|
| `DecodeMVSUpdate` | `0x7ff9223b85e4` |
| `DecodeMVSPartialUpdate` (state/quant setup) | `0x7ff9223bb400` |
| Huffman table builder | `0x7ff9223bb724` |
| per-update tile driver | `0x7ff9223bbad6` |
| SIMD IDCT | `0x7ff9223bc67d`, `0x7ff9223bcb21` |
| bit reader | `0x7ff9223bc1bf` |

`DecodeMVSUpdate` reads a `u32` BE length, rejects `> 0x5f5e101`, allocates
`len + 0x7d0`, reads the body, then switches on `body[0]`: `2` = quantisation
update, `1` = full, else partial. The full path takes `body[1]`/`body[2]` as
parameters and starts the bitstream at **`body+3`**, so the 6-byte
`type/p1/p2/data_off` header inferred from the encoder is at least partly wrong
for captured type-0 frames.

**The codec is JPEG-shaped.** Tables extracted from the framework:

| Table | Counts | Symbols |
|---|---|---|
| luma DC | `0,0,1,5,1,1,1,1,1,1` | 0–11 |
| luma AC | `0,0,2,1,3,3,2,4,3,5,5,4,4,0,0,1` | `01 02 03 00 04 11 05 12 21 31 41 …` |
| chroma DC | `0,0,3,1,1,1,1,1,1,1,1,1` | 0–11 |
| chroma AC | `0,0,2,1,2,4,4,3,4,7,5,4,4,0,1,2` | `00 01 02 03 11 04 05 21 31 …` |

AC symbols are `(run << 4) | size` including EOB `0x00` and ZRL `0xF0` — exactly
baseline JPEG. Quantisation tables are the standard JPEG luma
(`16,11,11,14,24,22,24,33,…`) and chroma (`19,19,24,47,76,99,…`) tables.
`tools/instrument/dump_mvs_tables.py` extracts all six at runtime (`--json`).

**Bit reader** (`0x7ff9223bc1bf`): state `+0xa8` accumulator, `+0xac` available
bits, `+0xb0` byte pointer. Refills one **byte** at a time, big-endian
(`eax = *ptr++; acc |= eax << (24 - avail); avail += 8`), consuming from the MSB.
A loop then shifts the accumulator one bit at a time while the sign bit is clear
(up to 39 bits, error at 40) — some codes are **unary-prefixed**, which is why
the fixed 3-bit command assumption failed.

**Remaining gap**: the framing between command codes, the 6-bit first-non-zero
prefix and the JPEG blocks. Decoding a block at the offset the command parse
predicts does not land on a valid repeat, so either the prefix belongs to the
block grammar or the payload widths differ. Recorded next step: read the command
dispatch inside `0x7ff9223bbad6` instead of inferring it.

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

### 2.8 Stream layout and DCT grammar (2026-11-04, continued)

Re-reading the client settles the container question: for type 0 the second
stream pointer is computed as `body + data_off` (`0x7ff9223b91e2`:
`lea rcx,[r15+3]; lea rcx,[rcx+rdx-3]`), so the 6-byte header
(`type, p1, p2, data_off[3]`) is correct after all:

- stream **A** = `body + 6` (commands + luma), ends at `data_off`
- stream **B** = `body + data_off` (chroma)

(the type-1 "full" path uses `body + 3`, i.e. no `data_off` field.)

The DCT coefficient grammar is now partly readable from `0x7ff9223b967c`:

```
6-bit value  ->  count = value + 1        (matches the encoder's 6-bit prefix)
count x 3-bit codes, MSB-first, each applied as
    running = running >= 0 ? running + code : running - code
```

so a block is `[6-bit count][count * 3-bit]`. There is a second path taken when
the parameter at `[rsp+0x78]` is >= 15, which appears to be the Huffman path
(the JPEG tables above).

What still does not line up: parsing the command stream
(`code / payload / repeat`) at either `body+6` or `body+3` desynchronises within
a few commands, and inserting the `[6-bit][3-bit * n]` DCT payload does not fix
it. So the *command* grammar remains the one unknown; the coefficient grammar,
container layout, Huffman tables, quant tables, IDCT and bit reader are all
identified.

### 2.9 Command dispatch and payloads — VERIFIED (2026-11-04)

The command code is a 16-bit field consumed before dispatch; the decoder
range-checks it (`cmp dil,7`, `0x7ff9223ba34b`) and jumps through a table of
**8 signed 32-bit relative offsets at `0x7ff9223bb3bc`**:

| code | handler | payload |
|---|---|---|
| 0 | `0x7ff9223ba37b` | none — fills the tile with `0xFF` bytes |
| 1 | `0x7ff9223ba3b2` | none — copies the tile **above** |
| 2 | `0x7ff9223ba423` | none — copies the **previous** tile |
| 3 | `0x7ff9223ba4b9` | 8 × 8-bit mask, then two colours |
| 4 | `0x7ff9223ba589` | 1-bit flag, 1-bit flag, then `Y(8) Cb(6) Cr(6)` |
| 5 | `0x7ff9223ba76d` | DCT block via `0x7ff922480b28` |
| 6 | `0x7ff9223ba819` | 16-bit index: hi byte then lo byte |
| 7 | `0x7ff9223babd6` | none — implicit previous index + 1 |

The decode-side bit reader is `0x7ff922480895(state, nbits) -> eax`.

Closed-form parsing still drifts: with a 4-bit first code and a 23-bit code-4
payload the first two commands align (their repeats decode to valid 16..30
values), but the third command desynchronises. The remaining discrepancy is a
**one-bit shift** somewhere in the command/repeat framing — the 22-bit payload
the handler implies lands the repeat on an impossible value 15, while 23 bits
lands it on a valid 30. So the encoder's 5-bit repeat field is right, but one
bit is being consumed (or not consumed) before it.

### 2.10 Corrected framing: run comes BEFORE the payload (2026-11-04)

Reading the client loop at `0x7ff9223ba24c` settles it:

```
call bit_reader(state, 3)      -> command code (always 3 bits, every tile)
store [rsp+0xa0] = code
call 0x7ff92248090c            -> run length  (no arguments, uses state)
store [rsp+0x9c] = run
... then dispatch through the jump table and read the payload
```

So the layout is **`[3-bit command][run][payload]`** — every earlier model that
put the payload first, or used a 4-bit first code, was wrong, which is why the
parse always desynchronised at the same place.

`0x7ff92248090c` decodes the run as:

```
flag = read(1)
if flag == 0            -> run = 0            (single tile)
else
    v = read(4)
    if v != 15          -> run = v + 1        (1..15 extra tiles)
    else                -> escape: read 8 bits b0, then a continuation bit;
                           b0 + 16, then further 8-bit groups shifted by 7
```

This matches the encoder's `0x10 | (n-1)` short form exactly (the high bit is
the flag, the low nibble is n-1), and the escape's `n - 16` base.

With this framing the parser consumes the **entire command stream** for the
first time instead of desynchronising; only the long-run escape is still
mismatched, which inflates the tile total. That is the last remaining piece
before a full decoder.

### 2.11 Current parse state (2026-11-04, end of session)

With `[3-bit command][run][payload]`, tiles of **8x8** (grid 320x200 =
**64,000** for 2560x1600), a 22-bit code-4 payload and a 6-bit DCT field, the
parser consumes the whole command stream and totals **65,577** tiles. A sweep
over payload sizes finds 63,637..65,577, i.e. within ~2.5% of the exact 64,000.

That residual is the variable-length DCT block payload: the handler at
`0x7ff9223ba76d` calls `0x7ff922480b28`, which decodes the block from the same
bit-reader state and consumes as many bits as its coefficients require.

So the remaining work is exactly one function: decode `0x7ff922480b28`
(JPEG-style block: DC category + bits, then AC run/size pairs until EOB/ZRL,
using the tables in `dump_mvs_tables.py`), which both fixes the tile count and
makes DCT tiles renderable.
