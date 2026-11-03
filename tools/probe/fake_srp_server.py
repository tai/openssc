#!/usr/bin/env python3
"""Fake Apple Screen Sharing SRP (security type 33) server.

Purpose: recover the exact SRP/KDF formulas used by the real client by acting
as the server with a known RSA key and a known SRP verifier.

It performs:
  - RFB 003.889 banner + security-type advertisement
  - RSA1 key request/response (2048-bit RSA, SPKI DER)
  - RSA1 packet-1: decrypt the identity envelope (username)
  - SRP challenge with a chosen N/g/salt/B/iterations/options
  - RSA1 packet-2: parse A, M1, options, client_random
  - compute the expected M1 for candidate formulas and compare
  - send M2 + SecurityResult, then read ClientInit and emit a minimal ServerInit

Usage:
    openssl genrsa -out artifacts/fake-rsa.pem 2048
    openssl rsa -in artifacts/fake-rsa.pem -pubout -outform DER -out artifacts/fake-rsa-pub.der
    python3 tools/probe/fake_srp_server.py --port 5902 --candidate user_colon
    open "vnc://tai:<password>@127.0.0.1:5902"
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import socket
import struct
import subprocess
import sys
import threading
import time

H = hashlib.sha512

# RFC 5054 4096-bit MODP group (also observed on the wire from mm.local).
N_HEX = (
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1"
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD"
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245"
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D"
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F"
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D"
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B"
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9"
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510"
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64"
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7"
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B"
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C"
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31"
    "43DB5BFCE0FD108E4B82D120A92108011A723C12A787E6D7"
    "88719A10BDBA5B2699C327186AF4E23C1A946834B6150BDA"
    "2583E9CA2AD44CE8DBBBC2DB04DE8EF92E8EFC141FBECAA6"
    "287C59474E6BC05D99B2964FA090C3A2233BA186515BE7ED"
    "1F612970CEE2D7AFB81BDD762170481CD0069127D5B05AA9"
    "93B4EA988D8FDDC186FFB7DC90A6C08F4DF435C934063199"
    "FFFFFFFFFFFFFFFF"
)
N_BYTES = bytes.fromhex(N_HEX)
N = int(N_HEX, 16)
G = 5
OPTS = b"mda=SHA-512,replay_detection,conf+int=ChaCha20-Poly1305,kdf=SALTED-SHA512-PBKDF2"


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def pad(x: int) -> bytes:
    return x.to_bytes(len(N_BYTES), "big")


def recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"closed after {len(buf)}/{n}")
        buf.extend(chunk)
    return bytes(buf)


def recv_rfb_packet(sock: socket.socket) -> bytes:
    total = int.from_bytes(recv_exact(sock, 4), "big")
    return recv_exact(sock, total)


def u16(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 2], "big"), o + 2


def u32(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 4], "big"), o + 4


def u64(b: bytes, o: int) -> tuple[int, int]:
    return int.from_bytes(b[o : o + 8], "big"), o + 8


def build_srp_frame(version: int, payload: bytes) -> bytes:
    """u32 total_len || u32 version || u16 (payload+4) || u32 payload_len || payload."""
    body = struct.pack(">I", version) + struct.pack(">H", len(payload) + 4) + struct.pack(">I", len(payload)) + payload
    return struct.pack(">I", len(body)) + body


def build_key_response(der: bytes) -> bytes:
    # Observed on the wire: total_len = n+7, u16 0x0001, u32 n, DER, one zero byte.
    body = struct.pack(">H", 1) + struct.pack(">I", len(der)) + der + b"\x00"
    return struct.pack(">I", len(body)) + body


def decrypt_rsa_identity(key_path: str, ciphertext: bytes) -> bytes:
    res = subprocess.run(
        [
            "openssl",
            "pkeyutl",
            "-decrypt",
            "-inkey",
            key_path,
            "-pkeyopt",
            "rsa_padding_mode:pkcs1",
        ],
        input=ciphertext,
        capture_output=True,
    )
    if res.returncode != 0:
        raise RuntimeError(f"openssl decrypt failed: {res.stderr.decode(errors='replace')}")
    return res.stdout


def parse_identity(pt: bytes) -> tuple[str, bytes]:
    o = 0
    payload_len, o = u32(pt, o)
    user_len, o = u32(pt, o)
    user = pt[o : o + user_len]
    o += user_len
    empty_len, o = u16(pt, o)
    empty_opaque = pt[o : o + 1]
    o += 1
    log(f"identity: payload_len={payload_len} user={user!r} empty_len={empty_len}")
    return user.decode("utf-8", "replace"), pt


def parse_packet2(p: bytes) -> tuple[bytes, bytes, bytes, bytes]:
    # `p` is the RSA1 payload (the caller already stripped the u32 length).
    o = 0
    ver, o = u16(p, o)
    alg = p[o : o + 4]
    o += 4
    auth, o = u16(p, o)
    aux, o = u16(p, o)
    pre, o = u16(p, o)
    inner_len, o = u16(p, o)
    log(f"packet2: ver={ver:#x} alg={alg!r} auth={auth} aux={aux} pre={pre} inner_len={inner_len}")
    end = o + inner_len
    ln, o = u16(p, o)
    A = p[o : o + ln]
    o += ln
    ln = p[o]
    o += 1
    M1 = p[o : o + ln]
    o += ln
    ln, o = u16(p, o)
    opts = p[o : o + ln]
    o += ln
    ln = p[o]
    o += 1
    rnd = p[o : o + ln]
    o += ln
    log(f"A={len(A)} M1={M1.hex()} opts={opts!r} random={rnd.hex()} consumed={o}/{end}")
    return A, M1, opts, rnd


def derive_password(salt: bytes, iterations: int, password: str, dklen: int) -> bytes:
    return hashlib.pbkdf2_hmac("sha512", password.encode(), salt, iterations, dklen)


def x_candidate(name: str, salt: bytes, user: str, password: str, iterations: int) -> bytes:
    if name == "user_colon_128":
        p = derive_password(salt, iterations, password, 128)
        return H(salt + H(user.encode() + b":" + p).digest()).digest()
    if name == "colon_128":
        p = derive_password(salt, iterations, password, 128)
        return H(salt + H(b":" + p).digest()).digest()
    if name == "user_colon_64":
        p = derive_password(salt, iterations, password, 64)
        return H(salt + H(user.encode() + b":" + p).digest()).digest()
    if name == "colon_64":
        p = derive_password(salt, iterations, password, 64)
        return H(salt + H(b":" + p).digest()).digest()
    if name == "user_colon_plain":
        return H(salt + H(user.encode() + b":" + password.encode()).digest()).digest()
    if name == "colon_plain":
        return H(salt + H(b":" + password.encode()).digest()).digest()
    if name == "user_raw_128":
        p = derive_password(salt, iterations, password, 128)
        return H(salt + H(user.encode() + p).digest()).digest()
    if name == "salt_p_128":
        p = derive_password(salt, iterations, password, 128)
        return H(salt + p).digest()
    raise ValueError(f"unknown candidate {name}")


def srp_expected(
    candidate: str,
    user: str,
    password: str,
    salt: bytes,
    iterations: int,
    A_bytes: bytes,
    B_bytes: bytes,
    b: int,
    v: int,
    identity_mode: str,
    g_hash_mode: str,
) -> tuple[bytes, bytes]:
    k = int.from_bytes(H(pad(N) + pad(G)).digest(), "big")
    A = int.from_bytes(A_bytes, "big")
    B = int.from_bytes(B_bytes, "big")
    u = int.from_bytes(H(pad(A) + pad(B)).digest(), "big")
    S = pow((A * pow(v, u, N)) % N, b, N)
    K = H(pad(S)).digest()

    hn = H(N_BYTES).digest()
    hg = H(bytes([G]) if g_hash_mode == "raw" else pad(G)).digest()
    hi = H(b"" if identity_mode == "empty" else user.encode()).digest()
    m1 = H(bytes(a ^ c for a, c in zip(hn, hg)) + hi + salt + A_bytes + B_bytes + K).digest()
    m2 = H(A_bytes + m1 + K).digest()
    return m1, m2


def handle(
    conn: socket.socket,
    addr,
    args,
    rsa_pub_der: bytes,
    rsa_key_path: str,
    salt: bytes,
    b: int,
    v: int,
    B_bytes: bytes,
    candidates: list[str],
) -> None:
    try:
        conn.settimeout(20.0)
        log(f"connection from {addr}")
        conn.sendall(b"RFB 003.889\n")
        log(f"client banner {recv_exact(conn, 12)!r}")
        conn.sendall(bytes([6, 30, 33, 36, 31, 32, 35]))  # match the real server's set
        sel = recv_exact(conn, 1)
        log(f"selector {sel.hex()}")

        pkt = recv_rfb_packet(conn)
        auth = int.from_bytes(pkt[6:8], "big")
        log(f"first packet len={len(pkt)} auth={auth} head={pkt[:20].hex()}")
        if auth == 0:
            log("sending RSA public key")
            conn.sendall(build_key_response(rsa_pub_der))
            pkt = recv_rfb_packet(conn)
            auth = int.from_bytes(pkt[6:8], "big")
            log(f"packet-1 len={len(pkt)} auth={auth}")

        if len(pkt) < 10:
            log("short packet-1")
            return
        inner_len = int.from_bytes(pkt[8:10], "big")
        body = pkt[10 : 10 + inner_len]
        pt = decrypt_rsa_identity(rsa_key_path, body[:256])
        user, _ = parse_identity(pt)

        # Challenge
        inner = (
            b"\x00"
            + struct.pack(">H", len(N_BYTES)) + N_BYTES
            + struct.pack(">H", 1) + bytes([G])
            + bytes([len(salt)]) + salt
            + struct.pack(">H", len(B_bytes)) + B_bytes
            + struct.pack(">Q", args.iterations)
            + struct.pack(">H", len(OPTS)) + OPTS
        )
        conn.sendall(build_srp_frame(2, inner))
        log(f"sent SRP challenge salt={salt.hex()} B={B_bytes[:8].hex()}...")

        if args.dump_raw:
            conn.settimeout(3.0)
            raw = bytearray()
            try:
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    raw.extend(chunk)
            except socket.timeout:
                pass
            log(f"RAW after challenge: {len(raw)} bytes")
            log(raw[:512].hex())
            return

        pkt2 = recv_rfb_packet(conn)
        A, M1, opts, rnd = parse_packet2(pkt2)
        A_bytes = A

        matched = []
        for cand in candidates:
            for ident in ("user", "empty"):
                for gmode in ("raw", "padded"):
                    xb = x_candidate(cand, salt, user, args.password, args.iterations)
                    vv = pow(G, int.from_bytes(xb, "big"), N)
                    m1e, m2e = srp_expected(
                        cand, user, args.password, salt, args.iterations,
                        A_bytes, B_bytes, b, vv, ident, gmode,
                    )
                    if m1e == M1:
                        matched.append((cand, ident, gmode, m2e))
                        log(f"*** M1 MATCH candidate={cand} identity={ident} g={gmode}")
        if not matched:
            log("no M1 match")
            conn.sendall(struct.pack(">I", 1))
            return

        cand, ident, gmode, m2 = matched[0]
        inner = b"\x40" + m2 + b"\x10" + os.urandom(16) + b"\x00\x00" + struct.pack(">I", 0)
        conn.sendall(build_srp_frame(2, inner))
        conn.sendall(struct.pack(">I", 0))
        log(f"sent M2 + SecurityResult 0 (candidate={cand})")
        try:
            ci = recv_exact(conn, 1)
            log(f"ClientInit {ci.hex()}")
            # Minimal ServerInit: 1920x1080, 32bpp/24 depth, true-colour, name "fake"
            pf = bytes([32, 24, 0, 1]) + struct.pack(">HHH", 255, 255, 255) + bytes([16, 8, 0, 0, 0, 0])
            name = b"fake"
            conn.sendall(struct.pack(">HH", 1920, 1080) + pf + struct.pack(">I", len(name)) + name)
            time.sleep(2)
        except Exception as exc:  # noqa: BLE001
            log(f"post-auth: {exc!r}")
    except Exception as exc:  # noqa: BLE001
        log(f"handler error: {exc!r}")
    finally:
        conn.close()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5902)
    ap.add_argument("--user", default="tai")
    ap.add_argument("--password", default=os.environ.get("SS_PASSWORD", ""))
    ap.add_argument("--iterations", type=int, default=46948)
    ap.add_argument("--salt-hex", default=None)
    ap.add_argument(
        "--candidates",
        default="user_colon_128,colon_128,user_colon_64,colon_64,user_colon_plain,colon_plain,user_raw_128,salt_p_128",
    )
    ap.add_argument("--rsa-key", default="artifacts/fake-rsa.pem")
    ap.add_argument("--rsa-pub", default="artifacts/fake-rsa-pub.der")
    ap.add_argument("--once", action="store_true")
    ap.add_argument("--dump-raw", action="store_true")
    args = ap.parse_args()

    rsa_pub_der = open(args.rsa_pub, "rb").read()
    salt = bytes.fromhex(args.salt_hex) if args.salt_hex else os.urandom(32)
    candidates = [c for c in args.candidates.split(",") if c]
    b = int.from_bytes(os.urandom(64), "big")

    # The server's B must be built from one verifier. For the matching test we
    # recompute the verifier for the first candidate; if it is wrong the client's
    # M1 will not match, and the run can be repeated with another candidate.
    xb = x_candidate(candidates[0], salt, args.user, args.password, args.iterations)
    v = pow(G, int.from_bytes(xb, "big"), N)
    k = int.from_bytes(H(pad(N) + pad(G)).digest(), "big")
    B = (k * v + pow(G, b, N)) % N
    B_bytes = pad(B)

    log(f"listening on {args.bind}:{args.port}, N={len(N_BYTES)}B, salt={salt.hex()}, B={B_bytes[:8].hex()}...")
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind((args.bind, args.port))
        srv.listen(2)
        while True:
            conn, addr = srv.accept()
            t = threading.Thread(
                target=handle,
                args=(conn, addr, args, rsa_pub_der, args.rsa_key, salt, b, v, B_bytes, candidates),
                daemon=True,
            )
            t.start()
            t.join()
            if args.once:
                break
    return 0


if __name__ == "__main__":
    sys.exit(main())
