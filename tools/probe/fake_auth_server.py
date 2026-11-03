#!/usr/bin/env python3
"""Fake Apple Screen Sharing security endpoint.

Purpose: learn what the *real* Screen Sharing client sends during the
version/security negotiation without involving the production server.

It speaks just enough RFB to:
  1. send the 003.889 banner,
  2. advertise a configurable security-type list,
  3. record the client's chosen type,
  4. optionally send Oakley Group 2 DH parameters (type 30/31/32),
  5. dump whatever the client sends next.

Usage:
    python3 tools/probe/fake_auth_server.py --port 5901 --types 30,31,32,33,35,36
    open "vnc://tai@127.0.0.1:5901"
"""

from __future__ import annotations

import argparse
import os
import socket
import struct
import sys
import threading
import time

# RFC 2409 Oakley Group 2 (1024-bit MODP), used verbatim by Apple's DH message.
GROUP2_PRIME = int(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1"
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD"
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245"
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED"
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381"
    "FFFFFFFFFFFFFFFF",
    16,
)
GENERATOR = 2


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"closed after {len(buf)}/{n}")
        buf.extend(chunk)
    return bytes(buf)


def recv_available(sock: socket.socket, timeout: float) -> bytes:
    sock.settimeout(timeout)
    out = bytearray()
    try:
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            out.extend(chunk)
    except socket.timeout:
        pass
    return bytes(out)


def make_dh_public(private: int) -> bytes:
    return pow(GENERATOR, private, GROUP2_PRIME).to_bytes(128, "big")


def handle(conn: socket.socket, addr, types: list[int], send_dh: bool, wait: float) -> None:
    try:
        conn.settimeout(10.0)
        log(f"connection from {addr}")
        conn.sendall(b"RFB 003.889\n")
        banner = recv_exact(conn, 12)
        log(f"client banner: {banner!r}")
        conn.sendall(bytes([len(types)]) + bytes(types))
        log(f"advertised types: {types}")
        choice = recv_exact(conn, 1)[0]
        log(f"CLIENT CHOSE TYPE: {choice} (0x{choice:02x})")

        if send_dh and choice in (30, 31, 32):
            private = int.from_bytes(os.urandom(32), "big")
            pub = make_dh_public(private)
            msg = struct.pack(">HH", GENERATOR, 128) + GROUP2_PRIME.to_bytes(128, "big") + pub
            conn.sendall(msg)
            log(f"sent DH params: gen={GENERATOR} keylen=128 prime=group2 pub={pub[:8].hex()}...")
            data = recv_available(conn, wait)
            log(f"client sent {len(data)} bytes")
            if data:
                log(f"client bytes: {data.hex()}")
        else:
            data = recv_available(conn, wait)
            log(f"client sent {len(data)} bytes: {data.hex()}")
    except Exception as exc:  # noqa: BLE001 - lab tool
        log(f"handler error: {exc!r}")
    finally:
        conn.close()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5901)
    ap.add_argument("--types", default="30,31,32,33,35,36")
    ap.add_argument("--no-dh", action="store_true")
    ap.add_argument("--wait", type=float, default=8.0)
    ap.add_argument("--once", action="store_true")
    args = ap.parse_args()

    types = [int(x, 0) for x in args.types.split(",") if x]
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind((args.bind, args.port))
        srv.listen(1)
        log(f"listening on {args.bind}:{args.port}, types={types}")
        while True:
            conn, addr = srv.accept()
            t = threading.Thread(
                target=handle,
                args=(conn, addr, types, not args.no_dh, args.wait),
                daemon=True,
            )
            t.start()
            t.join()
            if args.once:
                break
    return 0


if __name__ == "__main__":
    sys.exit(main())
