#!/usr/bin/env python3
"""Probe the Apple record layer: prelude -> rekey -> AES-128-CBC/SHA-1.

This is the M1 stepping stone. It authenticates, sends the native prelude,
decrypts the server rekey, activates the record layer, sends SetEncodings and a
framebuffer request, then decrypts and prints the first server records.

Usage:
    python3 tools/probe/apple_session.py --host mm.local
"""

from __future__ import annotations

import argparse
import hashlib
import os
import socket
import struct
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from apple_client import authenticate, log, recv_exact  # noqa: E402

# Exact native ViewerInfo + SetEncryption(1) + SetMode from the capture.
VIEWER_PRELUDE = bytes.fromhex(
    "2100003e"
    "0001000000020000000300000000000000000000000d0000000700000008"
    "b00008039000000000004000000000000000000000000000000000000000"
    "0000"
    "120000010001000100000001"
    "0a000001"
)
SET_ENCRYPTION_2 = bytes.fromhex("1200000200010000")
SET_ENCODINGS = bytes.fromhex(
    "0200000a"
    "00000006" "00000010" "ffffff11" "00000450" "0000044c"
    "ffffff21" "0000044d" "00000451" "00000453" "00000455"
)


def aes_ecb_decrypt(key: bytes, data: bytes) -> bytes:
    r = subprocess.run(
        ["openssl", "enc", "-d", "-aes-128-ecb", "-K", key.hex(), "-nopad"],
        input=data,
        capture_output=True,
    )
    if r.returncode != 0:
        raise RuntimeError(r.stderr.decode(errors="replace"))
    return r.stdout


def aes_cbc(key: bytes, iv: bytes, data: bytes, decrypt: bool) -> bytes:
    cmd = ["openssl", "enc", "-aes-128-cbc", "-K", key.hex(), "-iv", iv.hex(), "-nopad"]
    if decrypt:
        cmd.insert(2, "-d")
    r = subprocess.run(cmd, input=data, capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.decode(errors="replace"))
    return r.stdout


class RecordLayer:
    def __init__(self, key: bytes, iv: bytes, label: str):
        self.key = key
        self.iv = iv
        self.seq = 0
        self.label = label

    def encrypt(self, body: bytes) -> bytes:
        filler_len = (-(2 + len(body) + 20)) % 16
        plain = struct.pack(">H", len(body)) + body + os.urandom(filler_len)
        tag = hashlib.sha1(struct.pack(">I", self.seq) + plain).digest()
        ct = aes_cbc(self.key, self.iv, plain + tag, decrypt=False)
        self.iv = ct[-16:]
        self.seq += 1
        return struct.pack(">H", len(ct)) + ct

    def decrypt(self, ct: bytes) -> tuple[bytes, bool]:
        pt = aes_cbc(self.key, self.iv, ct, decrypt=True)
        self.iv = ct[-16:]
        if len(pt) < 22:
            raise ValueError(f"short record plaintext {len(pt)}")
        body_len = int.from_bytes(pt[:2], "big")
        body = pt[2 : 2 + body_len]
        tag = pt[-20:]
        expected = hashlib.sha1(struct.pack(">I", self.seq) + pt[:-20]).digest()
        ok = tag == expected
        self.seq += 1
        return body, ok


def read_ciphertext_record(s: socket.socket) -> bytes:
    n = int.from_bytes(recv_exact(s, 2), "big")
    return recv_exact(s, n)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--user", default="tai")
    ap.add_argument("--password", default=os.environ.get("SS_PASSWORD", ""))
    ap.add_argument("--records", type=int, default=8)
    args = ap.parse_args()

    with socket.create_connection((args.host, args.port), timeout=15) as s:
        s.settimeout(15)
        info = authenticate(s, args.user, args.password)
        width, height = info["width"], info["height"]
        K = info["K"]
        wrap = hashlib.sha256(K).digest()[:16]
        log(f"wrap key (SHA256(K)[:16]) = {wrap.hex()}")

        # Cleartext prelude.
        s.sendall(VIEWER_PRELUDE)
        log(f"sent ViewerInfo + SetEncryption(1) + SetMode ({len(VIEWER_PRELUDE)} bytes)")

        # The server answers with a FramebufferUpdate carrying encoding 0x44f.
        hdr = recv_exact(s, 4)
        log(f"cleartext message: type={hdr[0]} pad={hdr[1]} rects={int.from_bytes(hdr[2:4],'big')}")
        if hdr[0] != 0:
            log(f"unexpected cleartext message {hdr.hex()}")
            return 1
        nrects = int.from_bytes(hdr[2:4], "big")
        for _ in range(nrects):
            rect = recv_exact(s, 12)
            x, y, w, h, enc = struct.unpack(">HHHHI", rect)
            log(f"rect x={x} y={y} w={w} h={h} enc=0x{enc:x}")
            if enc == 0x44F:
                payload = recv_exact(s, 36)
                gen = int.from_bytes(payload[:4], "big")
                enc_key = payload[4:20]
                enc_iv = payload[20:36]
                log(f"rekey generation={gen} enc_key={enc_key.hex()} enc_iv={enc_iv.hex()}")
                key = aes_ecb_decrypt(wrap, enc_key)
                iv = aes_ecb_decrypt(wrap, enc_iv)
                log(f"rekey decrypted key={key.hex()} iv={iv.hex()}")
                send_rl = RecordLayer(key, iv, "client")
                recv_rl = RecordLayer(key, iv, "server")
            else:
                log("unknown rect encoding; cannot skip safely")
                return 1

        # Confirm encryption and activate the record layer.
        s.sendall(SET_ENCRYPTION_2)
        log("sent SetEncryption(2)")

        # Encrypted preface: SetEncodings, then a framebuffer request.
        s.sendall(send_rl.encrypt(SET_ENCODINGS))
        req = struct.pack(">BBHHHH", 3, 0, 0, 0, width, height)
        s.sendall(send_rl.encrypt(req))
        log(f"sent encrypted SetEncodings + FramebufferUpdateRequest ({width}x{height})")

        # Decrypt the first server records.
        for i in range(args.records):
            try:
                ct = read_ciphertext_record(s)
            except EOFError:
                log("server closed")
                break
            body, ok = recv_rl.decrypt(ct)
            log(f"server record {i}: ct={len(ct)} body={len(body)} sha1={'OK' if ok else 'BAD'} head={body[:32].hex()}")
            if body:
                log(f"  type=0x{body[0]:02x}")
            if not ok:
                log("integrity failure; stopping")
                break

    return 0


if __name__ == "__main__":
    sys.exit(main())
