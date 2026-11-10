#!/usr/bin/env python3
"""Minimal Apple Screen Sharing client (type 33 / RSA1 + SRP6a).

M0: connect, authenticate, and print ServerInit.
The reusable `authenticate()` is also used by the record-layer probes.

Usage:
    python3 tools/probe/apple_client.py --host mm.local --user tai --password "$SS_PASSWORD"

SRP recipe (recovered by matching the real client's M1 against a fake server
with a known verifier):
    P'  = PBKDF2-HMAC-SHA512(password, salt, iterations, dkLen=128)
    x   = SHA512(salt || SHA512(b":" || P'))
    k   = SHA512(PAD(N) || PAD(g))
    u   = SHA512(PAD(A) || PAD(B))
    S   = (B - k*g^x)^(a + u*x) mod N
    K   = SHA512(PAD(S))
    M1  = SHA512( SHA512(N) XOR SHA512(PAD(g)) || SHA512(b"") || salt || A || B || K )
    M2  = SHA512(A || M1 || K)
"""

from __future__ import annotations

import argparse
import hashlib
import os
import socket
import struct
import sys

H = hashlib.sha512


def log(msg: str) -> None:
    print(msg, flush=True)


def recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"closed after {len(buf)}/{n}")
        buf.extend(chunk)
    return bytes(buf)


def u16(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 2], "big"), o + 2


def u32(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 4], "big"), o + 4


def u64(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 8], "big"), o + 8


def der_tlv(data: bytes, off: int) -> tuple[int, bytes, int]:
    tag = data[off]
    off += 1
    length = data[off]
    off += 1
    if length & 0x80:
        n = length & 0x7F
        length = int.from_bytes(data[off : off + n], "big")
        off += n
    return tag, data[off : off + length], off + length


def parse_rsa_spki(der: bytes) -> tuple[int, int]:
    tag, seq, _ = der_tlv(der, 0)
    assert tag == 0x30, f"SPKI outer tag {tag:#x}"
    tag, _alg, off = der_tlv(seq, 0)
    assert tag == 0x30
    tag, bits, _ = der_tlv(seq, off)
    assert tag == 0x03
    assert bits[0] == 0, "unexpected unused bits"
    tag, rsaseq, _ = der_tlv(bits, 1)
    assert tag == 0x30
    tag, nbytes, off = der_tlv(rsaseq, 0)
    assert tag == 0x02
    tag, ebytes, _ = der_tlv(rsaseq, off)
    assert tag == 0x02
    return int.from_bytes(nbytes, "big"), int.from_bytes(ebytes, "big")


def pkcs1_v15_encrypt(n: int, e: int, msg: bytes) -> bytes:
    k = (n.bit_length() + 7) // 8
    ps_len = k - 3 - len(msg)
    if ps_len < 8:
        raise ValueError("message too long for RSA key")
    ps = bytearray()
    while len(ps) < ps_len:
        b = os.urandom(1)[0]
        if b:
            ps.append(b)
    em = b"\x00\x02" + bytes(ps) + b"\x00" + msg
    return pow(int.from_bytes(em, "big"), e, n).to_bytes(k, "big")


def build_identity(username: str) -> bytes:
    u = username.encode()
    payload_len = 4 + len(u) + 2 + 1
    return struct.pack(">II", payload_len, len(u)) + u + b"\x00\x00" + b"\x00"


def parse_challenge(payload: bytes):
    o = 0
    version, o = u32(payload, o)
    if version != 2:
        o = 0
        step = payload[o]
        o += 1
        x, o = u32(payload, o)
        plen, o = u32(payload, o)
        log(f"challenge alternate framing step={step} x={x} plen={plen}")
    else:
        x, o = u16(payload, o)
        plen, o = u32(payload, o)
        log(f"challenge version={version} x={x} plen={plen}")
    inner = payload[o : o + plen]
    p = 0
    control = inner[p]
    p += 1
    ln, p = u16(inner, p)
    N = inner[p : p + ln]
    p += ln
    ln, p = u16(inner, p)
    g = int.from_bytes(inner[p : p + ln], "big")
    p += ln
    ln = inner[p]
    p += 1
    salt = inner[p : p + ln]
    p += ln
    ln, p = u16(inner, p)
    B = inner[p : p + ln]
    p += ln
    iterations, p = u64(inner, p)
    ln, p = u16(inner, p)
    options = inner[p : p + ln]
    p += ln
    log(
        f"challenge: control={control} N={len(N)} g={g} salt={len(salt)} "
        f"B={len(B)} iterations={iterations} options={options.decode()}"
    )
    return N, g, salt, B, iterations, options


def srp_client(N_bytes: bytes, g: int, salt: bytes, B_bytes: bytes, iterations: int, password: str):
    N = int.from_bytes(N_bytes, "big")
    pad = lambda x: x.to_bytes(len(N_bytes), "big")

    p_prime = hashlib.pbkdf2_hmac("sha512", password.encode(), salt, iterations, 128)
    x = int.from_bytes(H(salt + H(b":" + p_prime).digest()).digest(), "big")

    a = int.from_bytes(os.urandom(64), "big")
    A = pow(g, a, N)
    A_bytes = pad(A)
    B = int.from_bytes(B_bytes, "big")
    u = int.from_bytes(H(A_bytes + B_bytes).digest(), "big")
    k = int.from_bytes(H(pad(N) + pad(g)).digest(), "big")

    S = pow((B - k * pow(g, x, N)) % N, a + u * x, N)
    K = H(pad(S)).digest()

    hn = H(N_bytes).digest()
    hg = H(pad(g)).digest()
    hi = H(b"").digest()
    m1 = H(bytes(a ^ c for a, c in zip(hn, hg)) + hi + salt + A_bytes + B_bytes + K).digest()
    m2 = H(A_bytes + m1 + K).digest()
    log(f"SRP: A={len(A_bytes)}B M1={m1.hex()}")
    return A_bytes, m1, m2, K


def build_packet2(A: bytes, m1: bytes, options: bytes, client_random: bytes) -> bytes:
    inner = (
        struct.pack(">H", len(A)) + A
        + bytes([len(m1)]) + m1
        + struct.pack(">H", len(options)) + options
        + bytes([len(client_random)]) + client_random
    )
    payload = (
        struct.pack(">H", 0x0100) + b"RSA1"
        + struct.pack(">H", 2)
        + struct.pack(">H", len(inner) + 4)
        + struct.pack(">H", 0)
        + struct.pack(">H", len(inner))
        + inner
    )
    return struct.pack(">I", len(payload)) + payload


def authenticate(s: socket.socket, user: str, password: str) -> dict:
    """Run type-33 auth on a connected socket and return session parameters."""
    banner = recv_exact(s, 12)
    log(f"server banner: {banner!r}")
    s.sendall(b"RFB 003.889\n")

    count = recv_exact(s, 1)[0]
    if count == 0:
        reason_len = int.from_bytes(recv_exact(s, 4), "big")
        raise RuntimeError(f"server refused: {recv_exact(s, reason_len)!r}")
    types = recv_exact(s, count)
    log(f"security types: {list(types)}")
    if 33 not in types:
        raise RuntimeError("server does not offer type 33")

    # Selector + RSA1 key request must be one write.
    key_req = struct.pack(">I", 10) + struct.pack(">H", 0x0100) + b"RSA1" + b"\x00\x00" + b"\x00\x00"
    s.sendall(bytes([33]) + key_req)
    total = int.from_bytes(recv_exact(s, 4), "big")
    kp = recv_exact(s, total)
    ver, o = u16(kp, 0)
    der_len, o = u32(kp, o)
    der = kp[o : o + der_len]
    n, e = parse_rsa_spki(der)
    log(f"server RSA key: version={ver} der={der_len} bits={n.bit_length()} e={e}")

    identity = build_identity(user)
    ciphertext = pkcs1_v15_encrypt(n, e, identity)
    body = ciphertext + b"\x00" * (640 - len(ciphertext))
    pkt1 = (
        struct.pack(">I", 10 + len(body))
        + struct.pack(">H", 0x0100)
        + b"RSA1"
        + struct.pack(">H", 2)
        + struct.pack(">H", len(ciphertext))
        + body
    )
    s.sendall(pkt1)
    log(f"sent packet-1 ({len(pkt1)} bytes)")

    total = int.from_bytes(recv_exact(s, 4), "big")
    challenge = recv_exact(s, total)
    N, g, salt, B, iterations, options = parse_challenge(challenge)
    A, m1, m2_expected, K = srp_client(N, g, salt, B, iterations, password)

    s.sendall(build_packet2(A, m1, options, os.urandom(16)))
    log("sent packet-2")

    total = int.from_bytes(recv_exact(s, 4), "big")
    proof = recv_exact(s, total)
    _ver, o = u32(proof, 0)
    _x, o = u16(proof, o)
    plen, o = u32(proof, o)
    inner = proof[o : o + plen]
    m2len = inner[0]
    m2 = inner[1 : 1 + m2len]
    if m2 != m2_expected:
        raise RuntimeError(f"server M2 mismatch: {m2.hex()} != {m2_expected.hex()}")
    log("server M2 verified")

    result = int.from_bytes(recv_exact(s, 4), "big")
    log(f"SecurityResult: {result}")
    if result != 0:
        raise RuntimeError(f"authentication failed: {result}")

    s.sendall(b"\xc1")
    hdr = recv_exact(s, 24)
    width, height = struct.unpack(">HH", hdr[:4])
    pixel_format = hdr[4:20]
    name_len = int.from_bytes(hdr[20:24], "big")
    name = recv_exact(s, name_len)
    log(f"ServerInit: {width}x{height} pixel_format={pixel_format.hex()} name={name!r}")
    return {
        "K": K,
        "width": width,
        "height": height,
        "pixel_format": pixel_format,
        "name": name,
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--user", default="tai")
    ap.add_argument("--password", default=os.environ.get("SS_PASSWORD", ""))
    ap.add_argument("--timeout", type=float, default=15.0)
    args = ap.parse_args()

    with socket.create_connection((args.host, args.port), timeout=args.timeout) as s:
        s.settimeout(args.timeout)
        info = authenticate(s, args.user, args.password)
        log(f"M0 OK: {info['width']}x{info['height']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
