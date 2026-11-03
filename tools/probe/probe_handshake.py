#!/usr/bin/env python3
"""Minimal RFB handshake probe for the Apple Screen Sharing server.

This intentionally stops before authentication. It records the exact bytes of
the version/security negotiation so Apple's security-type set can be mapped.

Usage:
    python3 tools/probe/probe_handshake.py --host mm.local
    python3 tools/probe/probe_handshake.py --host mm.local --client-version 003.008
"""

from __future__ import annotations

import argparse
import socket
import struct
import sys


def recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"connection closed after {len(buf)}/{n} bytes")
        buf.extend(chunk)
    return bytes(buf)


def hexdump(data: bytes) -> str:
    return " ".join(f"{b:02x}" for b in data)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--timeout", type=float, default=5.0)
    ap.add_argument(
        "--client-version",
        default=None,
        help="Version to send (default: echo the server banner, e.g. 003.889)",
    )
    ap.add_argument(
        "--choose",
        type=int,
        default=None,
        help="After listing security types, send this type and dump the reply",
    )
    args = ap.parse_args()

    with socket.create_connection((args.host, args.port), timeout=args.timeout) as s:
        s.settimeout(args.timeout)
        banner = recv_exact(s, 12)
        print(f"server banner : {banner!r}  [{hexdump(banner)}]")

        version = args.client_version
        if version is None:
            text = banner.decode("ascii", "replace").strip()
            version = text.split(" ", 1)[1]
        wire = f"RFB {version}\n".encode("ascii")
        if len(wire) != 12:
            print(f"error: version must produce 12 bytes, got {wire!r}", file=sys.stderr)
            return 2
        s.sendall(wire)
        print(f"client version: {wire!r}")

        count = recv_exact(s, 1)[0]
        if count == 0:
            reason_len = struct.unpack(">I", recv_exact(s, 4))[0]
            reason = recv_exact(s, reason_len)
            print(f"server refused: {reason.decode('utf-8', 'replace')!r}")
            return 1

        types = recv_exact(s, count)
        print(f"security types: {count} -> {list(types)}  [{hexdump(types)}]")
        for t in types:
            print(f"  type {t} (0x{t:02x})")

        if args.choose is not None:
            print(f"choosing security type {args.choose} (0x{args.choose:02x})")
            s.sendall(bytes([args.choose]))
            # Read whatever the server sends next, without assuming a length.
            s.settimeout(2.0)
            try:
                reply = s.recv(4096)
                print(f"reply ({len(reply)} bytes): {hexdump(reply)}")
                print(f"reply repr: {reply!r}")
            except socket.timeout:
                print("reply: <timeout; server may be waiting for auth data>")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
