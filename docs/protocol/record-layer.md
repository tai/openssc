# Record layer — Apple Screen Sharing

Status: **VERIFIED** by decrypting live `mm.local` sessions on 2026-11-03.
Evidence: `tools/probe/apple_session.py`, `tools/probe/apple_framebuffer.py`,
`artifacts/mitm-auth-full.log` (SHA-1 verified on every record).

## 1. Activation

After `SecurityResult = 0`, `ClientInit`, and `ServerInit`, the client sends a
cleartext prelude:

```
ViewerInfo (0x21)                    # 66 bytes
SetEncryption(command=1) (0x12)      # 12 bytes
SetMode (0x0a)                       # 4 bytes, optional
```

Exact native bytes (82 bytes, captured):

```
2100003e0001000000020000000300000000000000000000000d0000000700000008
b000080390000000000040000000000000000000000000000000000000000000
120000010001000100000001
0a000001
```

The server answers with a cleartext `FramebufferUpdate` containing one rect with
`x=y=w=h=0` and encoding **0x44f** (`EncodeEncryptionInfo`):

```
u32 generation          # 1 for the first rekey
byte encrypted_key[16]
byte encrypted_iv[16]
```

Decrypt each 16-byte block independently with **AES-128-ECB** using the
authentication wrap key:

- type 33: `SHA-256(SRP K)[0:16]`
- type 30: `MD5(DH shared)[0:16]`

The plaintexts are the record-layer **key** and **IV**.

The client then sends `SetEncryption(command=2)`:

```
12 00 00 02 00 01 00 00
```

and both directions switch to the record layer.

## 2. Record framing

```
u16 ciphertext_len || ciphertext[ciphertext_len]
```

`ciphertext_len` is a non-zero multiple of 16. Each direction is a **single
persistent AES-128-CBC stream**: the last ciphertext block of record N is the
IV for record N+1. The initial IV is the rekey IV; the same key/IV pair is used
for both directions in the captured sessions.

Plaintext:

```
u16 body_len || body[body_len] || filler || byte integrity[20]
```

- `filler_len = (-(2 + body_len + 20)) mod 16`
- filler bytes are random; receivers must ignore them.
- `integrity = SHA-1( u32_be(seq) || plaintext[0 : len-20] )`
- `seq` starts at 0 and increments once per record, never resets.
- One record carries one higher-level message for control traffic. Large
  server payloads (Zlib framebuffer) **span consecutive records**; concatenate
  the `body` fields in order.

## 3. Encryption profile

Captured sessions use **AES-128-CBC + plain SHA-1**, not ChaCha20-Poly1305 and
not HMAC. The `conf+int=ChaCha20-Poly1305` token in the SRP options string is
advertised but does not key this record layer.

Client preferences `encryptRFBDataStream` / `encryptKeystrokesOnly` (set to 0 in
`~/Library/Containers/com.apple.ScreenSharing/.../com.apple.ScreenSharing.plist`)
did **not** disable the record layer against `mm.local`; the server still
rekeyed and the native client still encrypted.

## 4. First encrypted client messages

Native order after activation:

1. `SetDisplayConfiguration` (0x1d)
2. `SetEncodings` (0x02)
3. `FramebufferUpdateRequest` (0x03)

For a minimal client, step 1 can be omitted; the server still streams Zlib
frames at the ServerInit geometry. A full static `SetDisplayConfiguration` is
required for dynamic resizing.
