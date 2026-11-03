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

Next step is to disassemble the encoder (`EncodeMVS`/`EncodePartialUpdateMVS`)
in the lab build of `screensharingd` rather than guessing field order, then port
the decoder into `rfb-proto` and validate against a `screencapture` reference.

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
