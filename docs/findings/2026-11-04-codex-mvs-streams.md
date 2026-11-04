# 2026-11-04 — MVS: two-stream split and the block-decoder ABI

Collaboration session (Codex + OpenCode). This supersedes the single-stream
model in `docs/protocol/codecs.md` §2.7–2.18.

Evidence:
- `artifacts/codex-mvs-commands.txt` — pure-Python command parser totals
- `tools/probe/mvs_commands.py` — independent command parser (Codex)
- `tools/probe/mvs_block.py` — full payload walk using the framework's own
  readers and block decoder (OpenCode)

## 1. Two independent bit streams

**Type-0 (partial) body layout.** Type 1 (full) uses a different header (see
§3 of the protocol notes); everything below is specific to type 0, which is
what the captured frames use:

```
+0x00 u8   type_of_update (0 partial, 1 full, 2 quant tables)
+0x01 u8   p1
+0x02 u8   p2
+0x03 u8[3] data_off            BE offset from body start
+0x06 ...   stream A = body[6 : data_off]      COMMANDS ONLY
data_off .. stream B = body[data_off :]        ALL PAYLOADS
```

The decoder keeps two readers, in the same state block:

| Reader | State fields | Used for |
|---|---|---|
| A | `state+0x98` (`[rsp+0xc8]`) | one leading flag bit, then every command + run |
| B | `state+0xa8` (`[rsp+0x38]`) | every command payload |

`state+0xb0` is the payload cursor and points at **`body + data_off`**. Reading
payloads from stream A (the previous model) is what made every parser
desynchronise at the first solid/bitmap command.

**Stream A grammar**: one leading flag bit, then repeated
`[3-bit command][run]`. Run is `1 flag bit` (0 = single tile) else `4 bits`
giving `v+1` (1..15); `v == 15` escapes to `16 + 8-bit byte` (further
continuation groups exist for very long runs).

**Stream B payload widths** (all reads go through the B reader):

| code | meaning | payload |
|---|---|---|
| 0 | fill | 0 |
| 1 | copy tile above | 0 |
| 2 | copy previous tile | 0 |
| 3 | two-colour 1-bit bitmap | 8-bit row mask, then one 8-bit row per **clear** mask bit |
| 4 | flagged colour / bitmap | 2 flag bits (b1, b2); if `b2 == 0` read `1 + b1` colours of `Y8 Cb6 Cr6` (20 bits each); if `b1 == 1` also the 8-bit row mask + one 8-bit row per clear bit |
| 5 | DCT block | variable, decoded by `0x7ff922480b28` |
| 6 | tile-cache index | 16 bits (hi byte, lo byte) |
| 7 | cache, previous+1 | 0 |

Codes **3, 6 and 7 do not occur in this capture**; their widths above are
derived statically from the handlers (`0x3ff9223ba4b9`, `0x3ba819`,
`0x3babd6`) and are not yet confirmed by a frame that exercises them.

Code 4 detail (corrected by Codex): `b1=0` is a solid (`20` bits when `b2=0`,
`0` when `b2=1`, i.e. reuse); `b1=1` is a bitmap, and when `b2=0` it carries
**two** colours (40 bits) before the mask, so the bitmap path costs
`40 + 8 + 8*(8-popcount(mask))`.

## 2. Full-frame result — VERIFIED

On `artifacts/mvs-4.bin` (2560x1600, white desktop capture):

```
tiles        64,000        exactly 320 x 200 tiles of 8x8
commands      1,534
DCT blocks    2,352        (from the framework block decoder, rc=0 each)
DCT bits    127,230
stream A     12,361 / 12,376 bits
stream B    145,378 / 145,392 bits
```

Both streams end with a **bit-aligned `0x6d` marker**, not a byte-aligned one,
so a byte scan (`bytes.find`) finds nothing at the end and must not be used:

| stream | used bits | marker byte read at that bit | padding |
|---|---|---|---|
| A | 12,361 | `0x6d` (from `0x36 0x80` starting at bit 1 of the byte) | 7 bits, zero |
| B | 145,378 | `0x6d` (from `0x9b 0x40` starting at bit 2 of the byte) | 6 bits, zero |

The walker asserts `br(commands.reader,8) == 0x6d` and
`br(payload.reader,8) == 0x6d` at the exact bit offsets, then that the remaining
bits are zero. (An earlier revision of this note wrongly reported "no marker"
because it searched byte-aligned.)

## 3. Block decoder ABI — VERIFIED

`0x7ff922480b28` is callable in-process through `ctypes` once the state block is
laid out correctly:

```
rc = block(state,            # c_void_p  state base; bit reader at +0xa8
           out,              # c_void_p  0x180-byte tile buffer
           ref,              # c_void_p  0x180-byte reference tile
           p1, p2,           # c_int     body[1], body[2]
           r9_ptr,           # c_void_p  side buffer (must be c_void_p!)
           stack1, stack2, stack3)   # c_void_p
```

- `rc == 0` on success; the reader fields advance by the block's bit length.
  **This establishes bit consumption only.** Passing zeroed dummy predictor and
  quantisation buffers makes the decoder consume the right number of bits but
  does not produce correct pixels, so nothing here validates the image itself.
- The bit reader at `0x7ff922480895` and run reader at `0x7ff92248090c` take
  **`state + 0xa8`**, while the gamma decoder at `0x7ff9223bc1bf` takes the
  **state base**. Mixing these up segfaults.
- The previous confusion about a 32-bit pointer store was the `sub rsp,8` in the
  caller: `[rsp+0x68]` after the subtraction is the earlier `[rsp+0x60]`, not
  the slot written by `mov [rsp+0x68], ecx`.

## 4. What remains

- The block decoder is driven through the framework, not reimplemented; a
  portable Rust decoder still needs its grammar.
- Structural checks (tile count, command count, both stream ends) pass on this
  single capture. A second capture with different content should be validated
  before treating the payload widths as fully settled.

## 5. Block coefficient VALUES (Codex, validated 2026-11-04)

`tools/probe/mvs_block_values.py` decodes the actual coefficients, not just the
bit consumption, and `--validate-native` matches the framework's 192-word i16
array on **all 2,352 captured blocks × zero and random seeded signed-16
references (4,704/4,704 exact)**, plus 6,400/6,400 RGB-helper pixels.

Facts needed for the Rust port:

- **Scan is the transposed JPEG zigzag** (table at `0x7ff9224c2340`), applied to
  the 64 coded coefficients.
- **Dequantisation** reorders by **transpose, not zigzag**:
  `qscaled[((j&7)<<3)|(j>>3)] = raw[j] << 4`, with native low-16 wrap.
- The `0x180` byte buffer is **three 64-word i16 planes** — `Y` at `+0`,
  `Cb` at `+0x80`, `Cr` at `+0x100` — not a 96-byte 4:2:0 sample tile. The
  wrapper runs the 8×8 IDCT three times.
- **This block syntax only codes chroma DC**; all chroma AC coefficients are
  zero.
- Colour conversion (`rgb_from_idct`) reproduces native `pmulhw` saturation.
- The native IDCT is not yet ported: a mathematical IDCT ×64 approximates it
  (DC=1 with Q16 → 127 native vs 128 ideal).

**Quantisation tables must come from the type-2 update**, not the framework
defaults: `artifacts/mvs-3.bin` carries `Y = body[1:65]`, `Cb/Cr = body[65:129]`
(observed Y starting 12,9,9,11 and chroma 15,15,18,36 versus the defaults
16,11,11,14 / 19,19,24,47). Handler `0x7ff9223b8a20` confirms the mapping and
the same transpose + `<<4`. Using the defaults for `mvs-4.bin` renders wrong
values.

### 5.1 Coefficient-value validation fixture

Chaining `decode_block` over `artifacts/mvs-4.bin` using the recorded block
starts (reference = previous block's 192-word output) gives a compact Rust-port
target:

```
blocks = 2352
bits   = 127230
FNV-1a over all 192x2352 i16 words (LE, xorshift-free)
       = 0x7913a407
```

Reproduce with `tools/probe/mvs_block_values.py` (`decode_block`) and the
`artifacts/mvs-block-sizes.txt` starts; a Rust port must reproduce the same
checksum before any pixel work is trusted.

Still to transcribe for the port (from `decode_block`): the mapping of the
`code != 1` short coefficient tokens (0/2/3) to values, and the `scale`
multiplier application order relative to `SCAN`.
