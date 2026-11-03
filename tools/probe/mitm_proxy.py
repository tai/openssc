#!/usr/bin/env python3
"""Transparent TCP proxy for RFB handshake analysis.

Client -> this proxy -> real macOS Screen Sharing server. Logs both directions
with timestamps and hex so Apple's auth exchange can be reconstructed.

Usage:
    python3 tools/probe/mitm_proxy.py --listen-port 5901 --target mm.local:5900
    open "vnc://tai:<password>@127.0.0.1:5901"

The proxy does not terminate TLS or crypto; it forwards bytes unchanged. After
Apple's auth completes the payload may be encrypted (depending on client
preferences), but the handshake itself is visible.
"""

from __future__ import annotations

import argparse
import socket
import sys
import threading
import time

_lock = threading.Lock()
_start = time.time()


def log(msg: str) -> None:
    with _lock:
        elapsed = time.time() - _start
        print(f"[{elapsed:8.3f}] {msg}", flush=True)


def hexdump(data: bytes, limit: int) -> str:
    if limit and len(data) > limit:
        return data[:limit].hex() + f"...({len(data)} bytes total)"
    return data.hex()


def pump(src: socket.socket, dst: socket.socket, label: str, limit: int) -> None:
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            log(f"{label} {len(data):6d}  {hexdump(data, limit)}")
            dst.sendall(data)
    except OSError as exc:
        log(f"{label} error: {exc!r}")
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(client: socket.socket, addr, target_host: str, target_port: int, limit: int) -> None:
    log(f"client connected from {addr}; dialing {target_host}:{target_port}")
    try:
        server = socket.create_connection((target_host, target_port), timeout=10)
    except OSError as exc:
        log(f"upstream connect failed: {exc!r}")
        client.close()
        return

    client.settimeout(None)
    server.settimeout(None)
    t1 = threading.Thread(target=pump, args=(client, server, "C->S", limit), daemon=True)
    t2 = threading.Thread(target=pump, args=(server, client, "S->C", limit), daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    try:
        client.close()
    except OSError:
        pass
    try:
        server.close()
    except OSError:
        pass
    log("connection closed")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--listen-port", type=int, default=5901)
    ap.add_argument("--target", default="mm.local:5900")
    ap.add_argument("--hex-limit", type=int, default=512)
    ap.add_argument("--once", action="store_true")
    args = ap.parse_args()

    host, _, port_s = args.target.partition(":")
    port = int(port_s or "5900")

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind((args.bind, args.listen_port))
        srv.listen(5)
        log(f"listening on {args.bind}:{args.listen_port} -> {host}:{port}")
        while True:
            conn, addr = srv.accept()
            t = threading.Thread(target=handle, args=(conn, addr, host, port, args.hex_limit), daemon=True)
            t.start()
            if args.once:
                t.join()
                break
    return 0


if __name__ == "__main__":
    sys.exit(main())
