# Apple framebuffer codecs

**Current MVS framing: see §2.19 and [the verified experiment](../findings/2026-11-04-codex-mvs-streams.md). Earlier exploratory framing claims below are superseded. Pixel decoding remains unverified.**

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

### 2.12 DCT block syntax (2026-11-04, `0x7ff922480b28`)

The block decoder is **not** baseline JPEG. Reading it in order:

```
1 bit  flag
if flag:                                   // block present
    2 bits  mode                          // selects a parameter (bt eax,30)
    huffman_decode(state) -> predictor A  // table depends on mode
    A = (refA + A) >> 1; A *= 2           // differential against the previous
                                          // tile's value, then doubled
    huffman_decode(state) -> predictor B
    B = (refB + B) >> 1; B *= 2
    ... coefficient loop with per-tile predictors ...
```

So per block there is a 1-bit presence flag, a 2-bit mode, and then two
Huffman-coded values that are *differentially* combined with the previous
tile's predictors (`(ref + delta) >> 1 << 1`). A further 2- or 8-bit field
(`lea r12d,[rcx*8+8]` vs `cmovnc 2`) selects the coefficient bit width based
on the mode byte (`cmp r8b,0xf`).

That is why a fixed DCT payload width could not fit: the block's size depends
on Huffman-decoded symbols *and* on running predictor state, and the residual
2.5% tile-count error at 64,000 comes from exactly this.

Implementing this faithfully is now a bounded-but-large piece of work:
1-bit flag, 2-bit mode, two differential Huffman predictors, the AC
run/size loop with `0x00` EOB and `0xF0` ZRL, then dequantise with the JPEG
tables and run the SIMD-equivalent IDCT.

### 2.13 Where the work stands

| Piece | Status |
|---|---|
| Container, 6-byte header, stream A/B split | done |
| Command set, jump table, payloads | done |
| Run-length coding | done |
| Bit reader semantics | done |
| JPEG Huffman + quant tables (extracted) | done |
| Command stream parse | runs to end, 2.5% tile-count error |
| DCT block syntax | mapped, not yet implemented |
| Dequant + IDCT + YCbCr→RGB | identified (SIMD routines) |

The remaining work is implementation rather than discovery: port the block
syntax above and validate the 64,000-tile grid exactly, then compare pixels
against `screencapture`.

### 2.14 Symbol decoder is unary-prefixed, not plain Huffman (2026-11-04)

`0x7ff9223bc1bf` does **not** decode canonical Huffman codes. It runs:

```
loop:
    if (avail <= 0) refill one byte (big-endian into the 32-bit accumulator)
    bit = acc >> 31
    acc <<= 1; avail--
    count++
    if (count <= 0x26 && bit == 1) goto loop      // stop at the first 0 bit
if (count == 0x28) -> protocol error
... then the remainder is decoded using the table ...
```

So a symbol starts with a **unary prefix counting 1 bits** (0..39, 40 = error), and
the canonical tables extracted earlier are used *after* that prefix. A plain
JPEG-style `(length, code)` lookup therefore fails immediately (the prototype
`tools/probe/mvs_parse.py` hits `bad huffman code` within ~100 commands).

This is the last missing piece: port `0x7ff9223bc1bf` exactly (unary prefix +
table lookup at `0x7ff9223bc2ee`), then the block decoder `0x7ff922480b28`, and
the parse should land on exactly 64,000 tiles.

### 2.15 Symbol code structure (2026-11-04, final for this session)

The tail of `0x7ff9223bc1bf` dispatches on the unary prefix length
(`cmp r9b,0`, `cmp r9b,1`, `cmp r9b,2`, …) and each case reads additional bits
and indexes a per-prefix table. So a symbol is an **Elias-gamma-style code**:
`k` one-bits, a zero, then `k` more bits selecting a table entry — not a
canonical `(length, code)` Huffman code.

That explains why every table-based prototype failed immediately. Recovering
the remaining work is mechanical but sizable:

1. transcribe each of the ~40 prefix cases of `0x7ff9223bc1bf` (table + extra
   bits) into a decoder;
2. port the block decoder `0x7ff922480b28` (1-bit flag, 2-bit mode, two
   differential predictors, mode-dependent width, AC run/size loop);
3. dequantise with the extracted JPEG tables, IDCT, YCbCr→RGB;
4. validate: command parse must land on exactly **64,000** tiles, then compare
   pixels against `screencapture`.

### 2.16 Symbol decoder decoded; block grammar still not converged

Reading every branch of `0x7ff9223bc1bf` shows it is a **variable-length integer**
decoder, not a table lookup:

| prefix (leading 1s) | value range |
|---|---|
| 0 | 0..1 (one more bit) |
| 1 | 2..3 |
| 2 | 4..7 |
| k >= 3 | `8*(k+1)-24 .. +7`, i.e. 8..15, 16..23, ... |

i.e. `k` one-bits, a zero, then `k+1` bits whose top `k` bits select within
`[2^k, 2^(k+1)-1]`. The extracted JPEG tables are therefore **not** used for
these values.

With this decoder the command stream again parses end-to-end without
desynchronising, but the tile total lands at ~21,000 instead of 64,000 for every
AC-termination variant tried (EOB on run==0, on level==0, on both, or none),
i.e. the block payload is being over-consumed by roughly 3x. The block's exact
field sequence (how many gamma values, and how the AC run/level pairs are
terminated) is the one remaining unknown.

Status: container, commands, runs, bit reader, gamma decoder and tables are all
known and implemented in `tools/probe/mvs_parse.py`; the block grammar needs a
few more focused iterations against the 64,000-tile oracle.

### 2.17 In-process oracle: calling the framework's decoder directly (2026-11-04)

A much better tool than static reading: the framework is `dlopen`-able from our
own process, so its static decoder functions can be called by address through
`ctypes`. `tools/probe/mvs_oracle.py` does this and works:

| Function | Address | Calling convention |
|---|---|---|
| bit reader | `0x7ff922480895` | `(state + 0xa8, nbits) -> value` |
| run reader | `0x7ff92248090c` | `(state + 0xa8) -> run` |
| gamma integer | `0x7ff9223bc1bf` | `(state, &out_u16) -> 0` |
| codec init | `0x7ff9223bb400` | `(viewer) -> state` (works on a zeroed 0x2000 buffer) |

The state layout matters: the bit/run readers take a pointer to the
**`state+0xa8`** sub-struct (`acc`, `avail`, `ptr`), while the gamma decoder and
init take the **state base**. Getting that wrong segfaults.

**Ground truth confirmed** with the oracle on a captured frame: a command is
`3 bits`, followed by the run (1 flag bit + 4 bits / escape). That matches the
client loop exactly, so the command/run layer is settled.

The DCT block decoder (`0x7ff922480b28`) could not yet be driven directly: it
needs specific arguments (the codec state, two planes, and three stack
arguments built from viewer fields) and crashes with plausible-looking dummies.
`tools/probe/mvs_block.py` records the attempt.

Next: pin those arguments from the handler at `0x7ff9223ba76d` (note `rdi` is
loaded from `[rsp+0x68]`, which was set from `ecx`, so at least one argument is
a pointer smuggled through a register), then use the real block decoder to size
every block and confirm the 64,000-tile total.

### 2.18 Block-size search attempts (2026-11-04, end of session)

Three independent approaches failed to pin the DCT block length:

1. **Fixed-size sweep** with the oracle for command/run: no size 0..320 makes
   the walk survive; runs immediately decode to garbage after the first block.
2. **Boundary search** (try every offset, count clean commands afterwards): the
   best scores are 30-33 commands for many different sizes, i.e. no signal.
3. **Beam search** guided by the tile rate (64,000 over the stream): diverges to
   millions of tiles; no state reaches the stream end with the right total.

So the block is not a fixed width, and no cheap constraint recovers it. The
remaining route is to make the real block decoder callable. Its first argument
is loaded as 64 bits from a slot that the caller writes with a 32-bit store
(`mov [rsp+0x68],ecx` then `mov rdi,[rsp+0x68]` at `0x7ff9223ba76d`), so either
the slot's upper half is pre-seeded elsewhere or the value is not a plain
pointer. Resolving that, plus the three stack arguments built from viewer
fields, would let the framework size every block for us.

Status: the command/run layer is confirmed by direct measurement; the block
grammar is the sole remaining unknown and needs the real decoder driven
in-process (or an instrumented build with debug permissions).

## 2.19 CORRECTION — two independent streams (VERIFIED 2026-11-04)

Everything in §2.7-2.18 that models a single bitstream is superseded.
`screensharingd` keeps **two readers** in the decoder state:

| Reader | State fields | Contents |
|---|---|---|
| A (commands) | `+0x98` | one leading flag bit, then `[3-bit command][run]` repeated |
| B (payloads) | `+0xa8` | every command payload |

**Type-0 body:**

```
+0x00 u8    type_of_update (0 = partial; 1 = full uses a DIFFERENT header)
+0x01 u8    p1
+0x02 u8    p2
+0x03 u8[3] data_off, BE, offset from body start
+0x06 ..    stream A  (commands only)
data_off .. stream B  (all payloads)
```

`state+0xb0` (the payload cursor) points at `body + data_off`.

**Stream A:** one flag bit, then repeated `3-bit command` + run. Run = 1 flag
bit (0 → single tile) else 4 bits → `v+1`; `v == 15` escapes to `16 + value`, where value is a little-endian base-128 integer (up to three bytes, high bit = continuation).
Each stream ends with a **bit-aligned `0x6d` marker** followed by zero padding
— it is *not* byte-aligned, so byte scans do not find it.

**Stream B payload widths:**

| code | payload |
|---|---|
| 0 fill, 1 copy-above, 2 copy-previous, 7 cache+1 | none |
| 3 two-colour bitmap | 8-bit row mask, then one 8-bit row per clear bit |
| 4 colour/bitmap | 2 flags `b1,b2`; if `b2==0` read `1+b1` colours of `Y8 Cb6 Cr6` (20 bits each); if `b1==1` also the 8-bit mask + one 8-bit row per clear bit |
| 5 DCT | variable, `0x7ff922480b28` |
| 6 cache index | 16 bits (hi, lo) |

Codes 3/6/7 were **not exercised** by the validated capture; their widths are
static-derived only.

**Full-frame validation** (`artifacts/mvs-4.bin`, 2560x1600 white desktop):

```
tiles 64,000   commands 1,534   DCT blocks 2,352   DCT bits 127,230
stream A 12,361/12,376 bits (+0x6d, 7 zero bits)
stream B 145,378/145,392 bits (+0x6d, 6 zero bits)
```

Both the independent command parser (`tools/probe/mvs_commands.py`, Codex) and
the full payload walk (`tools/probe/mvs_block.py`) agree, and the walker
asserts the tile count, command count, block count, both bit-aligned markers and
the zero padding.

**Block-decoder ABI** (`0x7ff922480b28`), callable in-process through ctypes:

```
rc = block(state, out, ref, p1, p2, r9_ptr, stack1, stack2, stack3)
```

`rc == 0` on success. Note the calling conventions: the bit and run readers take
`state+0xa8` while the gamma decoder takes the state base, and `r9` must be a
`c_void_p`. Driving it with zeroed dummy predictors establishes **bit
consumption only**, not correct pixels.

Evidence: `docs/findings/2026-11-04-codex-mvs-streams.md`,
`artifacts/codex-mvs-commands.txt`.

## 2.20 Push mode — spatial records, coefficient ring, type-1 (VERIFIED 2026-11-04)

Server push (`0x09`) makes the server emit MVS bodies continuously instead of
answering requests. That stream contains **two body types**, and the type-0
deltas in it are *not* self-contained: they reference tiles sent earlier. This
section is the decoder contract. Evidence: `docs/findings/2026-11-04-push-mode-mvs.md`,
captures `artifacts/push-pC/` (89 bodies) and `artifacts/push-codex-manifest.txt`
(222 bodies), framework disassembly `artifacts/push-codex-cache-annotated.asm`.

### 2.20.1 Two per-session tables, not one

| table | address | entry | count | written by |
|---|---|---|---|---|
| spatial records | `viewer+0xd80` | 84 B | `ceil(W/8)*ceil(H/8)` | type-0 (every expanded tile), read/refined by type-1 |
| coefficient ring | `viewer+0xda0` | 99 B | 65,000 | **only** type-1 code 1; read by type-0 codes 6/7 and type-1 code 3 |

Both survive across bodies for the whole session (allocated with the framebuffer
/ codec, not per body). Ring writes use a cursor that wraps `65000 -> 1`;
`last_read` is updated only by cache **reads**.

`SpatialRecord` (84 B, one per tile, global grid `grid_width = ceil(W/8)`):

```
+00  u8   param          DCT multiplier threshold for this tile
+01  i8[64] Y            luma coefficients in SCAN order (low byte of i16)
+41  u8   Cr_count       =1 from the type-0 block decoder
+42  i8   Cr_DC
+43  u8   Cb_count       =1
+44  i8   Cb_DC
+48  u32  generation     body counter; incremented once per type-0 body
+4c  i32  source_record  -1, or the tile this one was copied from
+50  u32  source_framebuffer_offset
```

Ring entry (99 B) is **signed coefficient bytes in scan order, ordered
Y64 / Cr15 / Cb20** — note that the ring order is not the plane order
(`Y / Cb / Cr`) used by the block decoder. Every expanded type-0 tile first
resets its record (`generation = current`, `source_record = -1`); the DCT
handler then fills `param`, `Y[64]` and the chroma DCs.

### 2.20.2 Type-0 payload corrections

- Code 1 copies the **left/previous** tile and code 2 the **above** tile. Earlier
  revisions of this document had these swapped.
- Record index is global and includes the rect origin:
  `(rect_y>>3)*grid_width + (rect_x>>3) + local_y*grid_width + local_x`.
- Code 6 reads a **16-bit big-endian absolute** ring index; code 7 reads
  `last_read + 1`. Neither writes a ring entry, and neither is present in any
  body this server produced in *polling* mode — which is exactly why polling
  accumulated correctly with the cache unimplemented.

### 2.20.3 Type-1 bodies

Type-1 is **refinement of the type-0 spatial records**, not an independently
decodable frame. Applying one without the preceding type-0 bodies is meaningless.

```
body[0] = 1
body[1] = cr_ac_count   clamp 64
body[2] = cb_ac_count   clamp 64
body[3:] = one MSB-first bitstream, codes and payloads interleaved
```

Per tile, row-major over the rect, **2-bit code**:

| code | meaning |
|---|---|
| 0 | no-op |
| 1 | refine the record, render, and **insert** the 99-byte entry into the ring |
| 2 | copy 8x8 pixels from `record.source_record`, but only if that record's `generation` matches |
| 3 | ring read: one bit selects `last_read+1`, else 16 bits of absolute index |

Terminator: three literal `mvs` bytes, bit-aligned at the end (same convention as
type-0's `0x6d` markers).

Refinement (code 1) per luma coefficient `j = 1..n-1` where `n = read(6)+1`:

```
width = (j < param ? 1 : 3) if param >= 15 else (j < param ? 3 : 4)
old = i8(record.Y[j])
if width == 1            value = step(old)          # +/-1 or 0, see below
else if old == 0         value = predictor_symbol()
else if old > 0          value = i8(old + read(width))
else                     value = i8(old - read(width))
step(old): old>0 -> old+bit; old<0 -> old-bit; old==0 -> 0 unless bit, then +/-1 on the next bit
```

Chroma: `Cr_DC` and `Cb_DC` use `step()`; the ACs are ordinary JPEG run/size
with signed extension, `count = cr_ac_count` / `cb_ac_count`, starting at
coefficient 1. The quantiser table used for **both** chroma planes is the
**luma AC** table (`state+0x68`, counts at `0x4c34c0` read as 17 bytes with the
unused index 0 dropped, symbols at `0x4c34e0`, 162 symbols — the standard JPEG
Annex K luma AC table).

### 2.20.4 Arming push correctly

```
SetEncodings(0x3f3)
0x09 AutoFrameBufferUpdate  version=1  selected_screen=0  rect=(0,0,W,H)
0x03 FBUpdateRequest non-incremental (0,0,W,H)   # once
```

`selected_screen` must be **0**. With `0xffffffff` (the convenience helper's
default) the server sends metadata plus the type-2 quant body and then stalls.
After that the server free-runs; extra `0x03` requests make it serve those
instead of pushing.

### 2.20.5 Validation

- `artifacts/push-pC/manifest.txt`, 89 bodies (51 type-0, 37 type-1, 1 quant):
  our decoder reports 20,082 ring insertions, `last_read` 20,073, cursor 20,082,
  matching the independent Python parser exactly.
- `artifacts/push-codex-manifest.txt`, 222 bodies: 34,608 insertions.
- Command 6/7 coverage in push deltas: 18,768 + 7,727 tiles out of 214,732
  (~12 %), present in 45 of 51 type-0 delta bodies; **zero** in polling bodies.
- Independent pixel oracle (`artifacts/push-codex-render.py`, native block
  decoder / cache read / insert / IDCT): the Rust decoder agrees to
  **max channel difference 1, zero pixels above 1**, on 4.4 % of pixels (the
  documented native-vs-portable IDCT rounding), checked at bodies 12, 45, 78 and
  the final frame.
- The rendered sequence is a coherent desktop with no shear or stale bands.

### 2.20.6 Remaining latency

Polling MVS already delivers keystrokes in ~85–98 ms p50 on the lab LAN, and the
floor is one server capture interval, not the request loop (during a repaint,
applied rects arrive 6–9 ms apart). Push removes the request leg; whether it
beats that floor is **not yet measured** — see STATUS.md.

## 2.21 Decoder throughput (2026-11-06)

No wire change: bodies, commands, spatial records and the coefficient ring are
decoded exactly as §2.20 describes. What changed is how fast, because at
full 2560x1600 resolution the decoder could not keep up with a scroll. Numbers
and profiling live in `docs/findings/2026-11-06-mvs-decode-throughput.md`.

Sixty-odd percent of the decode was the IDCT and the per-pixel colour
conversion. Observable rules that follow from the work:

- **The transform is fixed point, not f64.** libjpeg's `islow` butterfly with
  18-bit cosine constants and i64 accumulators, descaled once at the end
  (`2*CONST_BITS - 3`, giving the native scale-64 samples). 13-bit constants
  (what libjpeg uses) are not enough here: this codec's dequantised range is a
  full i16 product, not the 12-bit range `jidctint` assumes, and 13-bit
  constants drifted sample values by up to 385 at the extremes. The rendered
  channel output still matches the f64 basis within 1, which is the project's
  existing tolerance against the framework; the sample-level envelope at the
  i16 extreme is 8 and is asserted by the unit tests.
- **A plane with no AC coefficients is `8 * dequantised DC` everywhere.** That is
  the DC shortcut; it is exact, not an approximation. Chroma in type-0 bodies is
  DC-only, so most tiles transform one plane, not three.
- **Flat chroma is detected on the AC ranges**, `65..128` and `129..192`.
  `64..` includes the two DC words, which are normally non-zero, so checking it
  made the shortcut dead code for a while.
- **Pixel conversion is hoisted, then vectorised by the compiler.** With flat
  chroma the six chroma terms are constants for the whole tile, and the loop is
  plain SSE2 `paddsw`/`packssdw` arithmetic at four pixels per instruction. The
  arithmetic is `pmulhw`-shaped (`i16` truncation after every add), so the
  scalar and vector paths must agree byte for byte; the differential test in
  `mvs.rs` checks that against the f64 reference.
- **Tiles are written straight into the framebuffer.** CopyPrevious/CopyAbove
  are `fb -> fb` row copies via `copy_within`, which is a memmove and therefore
  safe when source and destination overlap; no staging tile is involved. This is
  presentation only: the spatial record writes (`generation`,
  `source_record`, `source_framebuffer_offset`) are unchanged.
- **Fill runs fill row spans.** A white desktop is tens of thousands of
  consecutive fill tiles, so a run is filled as one span per tile row instead of
  one tile at a time.
