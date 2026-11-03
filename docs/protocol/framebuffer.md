# Framebuffer and server metadata — Apple Screen Sharing

Status: **VERIFIED** (Zlib snapshot decoded to PNG end-to-end) on 2026-11-03.
Evidence: `tools/probe/apple_framebuffer.py`, `artifacts/remote.png`.

## 1. ServerInit

After `ClientInit = 0xC1`:

```
u16 width
u16 height
16-byte PIXEL_FORMAT
u32 name_len
byte name[name_len]
```

Observed on `mm.local`:

```
width  = 2560
height = 1600
pixel  = 20 18 00 01 00 ff 00 ff 00 ff 10 08 00 00 00
name   = 00 00 00 00 00 02 bf f6 e7 03 e0 00 ... 6d 6d
```

- 32 bpp, depth 24, little-endian, true colour.
- red max 255 / shift 16, green max 255 / shift 8, blue max 255 / shift 0.
- On the wire each pixel is therefore `B G R 0` (little-endian 32-bit).
- The name field carries Apple binary metadata, not text; clients must tolerate
  arbitrary bytes.

## 2. Server messages

### 2.1 FramebufferUpdate (type 0)

```
u8 type = 0
u8 pad
u16 n_rects
rect[n]:
  u16 x, y, w, h
  u32 encoding
  payload...
```

### 2.2 AppleDisplayLayout — encoding 0x451

Observed as a zero-size rect (`x=y=w=h=0`). Body starts with:

```
u16 payload_len       # e.g. 76
u16 version = 5
...
```

The payload carries scaled/backing geometry (`2560x1600` on `mm.local`).
A client should use it to update framebuffer size before applying later rects.
The first record in a native session is this layout message.

### 2.3 VendorKeysym — encoding 0x453

Observed 20-byte payload:

```
u16 payload_len = 0x14
u16 0x0001
u16 0x0004
u32 0x1008FD00
u32 0x1008FD01
u32 0x1008FD02
u32 0x1008FD03
```

Parse-and-ignore is safe.

### 2.4 Zlib framebuffer — encoding 6

The server chooses Zlib for a full frame after a non-incremental
`FramebufferUpdateRequest`.

Rect payload:

```
u32 compressed_len
byte zlib_stream[compressed_len]   # zlib header 0x78 0x01 in captures
```

`zlib_stream` is a single standard zlib stream; its decompressed size is
`w * h * 4` (32 bpp). It may span **many** record-layer records; concatenate
record bodies and feed the decompressor until `compressed_len` bytes have been
consumed. Do not feed the following RFB message (often `MiscStatus`, type 0x14)
into the zlib stream.

Observed on `mm.local`: 2560×1600, compressed length ≈ 3.8 MB, 16,384,000
decompressed bytes across ~119 records.

### 2.5 CursorImage — encoding 0x450

```
u32 cache_id
u32 compressed_len
byte zlib_payload[compressed_len]
```

`compressed_len = 0` is a SELECT of a cached cursor; otherwise STORE.
The decompressed payload is `w*h*4` BGRA followed by `w*h` alpha (separate
plane). Cursor shape only; position comes from the client's pointer.

## 3. Snapshot algorithm used by the Python probe

1. Authenticate (type 33), enter the record layer.
2. Send `SetEncodings` with the native list:
   `6, 16, 0xffffff11, 0x450, 0x44c, 0xffffff21, 0x44d, 0x451, 0x453, 0x455`.
3. Send `FramebufferUpdateRequest` (`type 3`, incremental 0, full geometry).
4. Decrypt records; parse FBU headers; skip 0x451/0x453/0x450.
5. On encoding 6, read `u32 compressed_len`, feed exactly that many bytes to
   `zlib.decompressobj()`.
6. Convert BGRX to RGB and write PNG.

The resulting screenshot matches the remote desktop pixel-for-pixel in layout
and colour (`artifacts/remote.png`).
