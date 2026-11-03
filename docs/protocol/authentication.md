# Authentication — Apple Screen Sharing (`RFB 003.889`)

Status: **VERIFIED** against `mm.local` (macOS 12.7.6, `screensharingd` 3.9.8)
on 2026-11-03. Capture: `captures/20261103-baseline-handshake.pcap`,
`artifacts/mitm-auth-full.log`, `artifacts/fake-srp-3.log`.

## 1. Version and security types

The server speaks first:

```
S->C  "RFB 003.889\n"
C->S  "RFB 003.889\n"
S->C  u8 count || u8 types[count]
```

Observed types on `mm.local`: `06 1e 21 24 1f 20 23`
= **30, 33, 36, 31, 32, 35**.

The native client prefers **33** (`0x21`) and coalesces the selector with the
first authentication payload in one write. The server closes if the selector
and RSA1 key request arrive in separate TCP writes.

| Type | Meaning | Status |
|------|---------|--------|
| 30 | Apple DH (legacy ARD) | server sends 1024-bit Group 2 params |
| 31, 32 | DH variants | server sends the same 260-byte DH message |
| 33 | RSA1 + SRP-6a | **implemented and verified** |
| 35 | Kerberos GSS-API | client sends data first |
| 36 | Direct SRP | client sends data first |

## 2. Type 33 — RSA1 envelope

All integers are big-endian unless stated. The envelope is:

```
u32 total_len          # bytes after this field
u16 version            # 0x0100
char algorithm[4]      # "RSA1"
u16 auth_type          # 0 = public-key request, 2 = SRP/identity
u16 inner_len          # meaningful body length
byte body[inner_len]
```

### 2.1 Public-key request (`auth_type = 0`)

```
u32 10 | u16 0x0100 | "RSA1" | u16 0 | u16 0
```

Total on the wire (including the 1-byte security selector): 15 bytes.

Server response (observed 305 bytes):

```
u32 total_len          # DER length + 7
u16 version = 1
u32 der_len
byte der[der_len]      # DER SubjectPublicKeyInfo, RSA-2048, e=65537
byte 0x00              # trailing zero
```

RSA key is persisted server-side (same key across connections); the native
client caches it and may skip the request.

### 2.2 Packet-1 (`auth_type = 2`)

Body = 256-byte RSA ciphertext + 384 zero bytes (640 bytes total).
`inner_len = 256`; `total_len = 650`.

RSA padding: **PKCS#1 v1.5**. Plaintext identity envelope:

```
u32 payload_len        # remaining bytes (10 for "tai")
u32 username_len
byte username[username_len]   # UTF-8
u16 0
u8  0
```

### 2.3 SRP challenge (server → client)

Observed framing on macOS 12 `screensharingd` (note: **u32 version**, unlike
some third-party descriptions that show a u8 step):

```
u32 total_len          # 10 + payload_len
u32 version = 2
u16 x                  # 4 + payload_len
u32 payload_len
payload:
  u8  control = 0
  u16 N_len || N       # 512 bytes, RFC 5054 4096-bit MODP
  u16 g_len || g       # 1 byte, g = 5
  u8  salt_len || salt # 32 bytes
  u16 B_len || B       # 512 bytes
  u64 iterations       # observed 46948 (account-specific)
  u16 opts_len || opts # 80 bytes
```

Options string (verbatim, 80 bytes):

```
mda=SHA-512,replay_detection,conf+int=ChaCha20-Poly1305,kdf=SALTED-SHA512-PBKDF2
```

The ChaCha20-Poly1305 token does **not** key the record layer; the record layer
is AES-128-CBC (§ record-layer.md).

### 2.4 SRP recipe — confirmed byte-exact

Recovered by standing up a fake SRP server with a known verifier and matching
the real client's `M1` (`artifacts/fake-srp-3.log`):

```
H    = SHA-512
P'   = PBKDF2-HMAC-SHA512(password, salt, iterations, dkLen=128)
x    = H(salt || H(":" || P'))          # identity is EMPTY here
k    = H(PAD(N) || PAD(g))
u    = H(PAD(A) || PAD(B))
S    = (B - k*g^x)^(a + u*x) mod N
K    = H(PAD(S))                        # 64 bytes
M1   = H( H(N) XOR H(PAD(g)) || H("") || salt || A || B || K )
M2   = H( A || M1 || K )
wrap = SHA-256(K)[0:16]                 # AES-128 key for the first rekey
```

`PAD(x)` is fixed-width big-endian to the length of `N` (512 bytes).
Critically, `H(g)` uses the **padded** generator, and the SRP identity is
empty (not the username).

### 2.5 Packet-2 (client → server)

```
u32 total_len
u16 version = 0x0100
"RSA1"
u16 auth_type = 2
u16 aux = inner_len + 4
u16 preamble = 0
u16 inner_len
inner:
  u16 A_len || A        # 512 bytes
  u8  M1_len || M1      # 64 bytes
  u16 opts_len || opts  # echo the challenge options verbatim
  u8  random_len || random  # 16 bytes
```

The native client adds 384 bytes of non-zero trailing data after the bounded
`inner`; the server uses `inner_len` and ignores it. Our client sends only the
bounded inner and is accepted.

### 2.6 Final proof and result

```
u32 total_len
u32 version = 2
u16 x = 4 + payload_len
u32 payload_len
payload:
  u8  M2_len || M2      # 64 bytes
  u8  random_len || random  # 16 bytes
  u16 empty_len = 0
  u32 0
```

Then the standard RFB `u32 SecurityResult` (0 = success). The client sends
`ClientInit = 0xC1` (shared | 0x40 | 0x80) and receives `ServerInit`.

## 3. Type 30 (legacy DH) — observed, not yet implemented

Server message (260 bytes):

```
u16 generator = 2
u16 keylen = 128
byte modulus[128]     # RFC 2409 Oakley Group 2
byte server_public[128]
```

The client response is a 128-byte AES-128-CBC (zero IV) ciphertext over
`64-byte username || 64-byte password` followed by the 128-byte client public
value. Key = `MD5(shared)[0:16]`. See prior-art notes; verify before shipping.
