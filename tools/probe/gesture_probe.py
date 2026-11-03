#!/usr/bin/env python3
"""Send an Apple gesture (type 0x17) and capture the resulting framebuffer.

Usage:
    python3 tools/probe/gesture_probe.py --host mm.local --kind swipe --motion 1 --swipe-mask 1 --out artifacts/gesture.png
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import os
import socket
import struct
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(__file__))
from apple_client import authenticate, log, recv_exact  # noqa: E402
from apple_session import SET_ENCODINGS, SET_ENCRYPTION_2, VIEWER_PRELUDE, RecordLayer, aes_ecb_decrypt  # noqa: E402


def png_chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", binascii.crc32(tag + data) & 0xFFFFFFFF)


def write_png(path: str, width: int, height: int, bgra: bytes) -> None:
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        off = y * width * 4
        line = bgra[off : off + width * 4]
        rgb = bytearray(width * 3)
        rgb[0::3] = line[2::4]
        rgb[1::3] = line[1::4]
        rgb[2::3] = line[0::4]
        rows.extend(rgb)
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    data = b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", ihdr) + png_chunk(b"IDAT", zlib.compress(bytes(rows), 6)) + png_chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(data)


def recv_record(s: socket.socket) -> bytes:
    n = int.from_bytes(recv_exact(s, 2), "big")
    return recv_exact(s, n)


def gesture_swipe(motion: int, swipe_mask: int, progress: float, px: float, py: float,
                  vx: float, vy: float, vz: float, flags: int, flavor: int, x: int, y: int) -> bytes:
    return struct.pack(
        ">BBHHH II ffffff II HH",
        0x17, 0, 0x30, 1, 6,
        motion, swipe_mask,
        progress, px, py, vx, vy, vz,
        flags, flavor,
        x, y,
    )


def gesture_begin_end(subtype: int, x: int, y: int, extra: int = 0) -> bytes:
    # subtype 1 = begin, 2 = end; total 16 bytes
    return struct.pack(">BBHHHIHH", 0x17, 0, 0x0C, 1, subtype, extra, x, y)


def gesture_scroll(dx: float, dy: float, dz: float, natural: int, phase: int, mask: int, x: int, y: int) -> bytes:
    return struct.pack(
        ">BBHHH fff I I I HH",
        0x17, 0, 0x20, 1, 8,
        dx, dy, dz, natural, phase, mask, x, y,
    )


def capture_frame(s: socket.socket, send_rl: RecordLayer, recv_rl: RecordLayer, width: int, height: int):
    s.sendall(send_rl.encrypt(struct.pack(">BBHHHH", 3, 0, 0, 0, width, height)))
    buf = bytearray()
    dec = None
    zremain = 0
    pixels = bytearray()
    for recno in range(5000):
        body, ok = recv_rl.decrypt(recv_record(s))
        if not ok:
            raise RuntimeError(f"integrity failure at record {recno}")
        buf.extend(body)
        while True:
            if dec is not None:
                n = min(len(buf), zremain)
                chunk = bytes(buf[:n])
                del buf[:n]
                zremain -= n
                pixels.extend(dec.decompress(chunk))
                if dec.eof or zremain == 0:
                    return bytes(pixels)
                break
            if len(buf) < 4:
                break
            if buf[0] != 0:
                buf.clear()
                break
            nrects = int.from_bytes(buf[2:4], "big")
            if nrects == 0:
                del buf[:4]
                continue
            if len(buf) < 16:
                break
            x, y, w, h, enc = struct.unpack(">HHHHI", bytes(buf[4:16]))
            if enc == 6:
                if len(buf) < 20:
                    break
                clen = int.from_bytes(buf[16:20], "big")
                dec = zlib.decompressobj()
                zremain = clen
                payload = bytes(buf[20 : 20 + zremain])
                del buf[: 20 + len(payload)]
                zremain -= len(payload)
                pixels.extend(dec.decompress(payload))
                if dec.eof or zremain == 0:
                    return bytes(pixels)
                break
            if enc in (0x451, 0x453):
                if len(buf) < 18:
                    break
                plen = int.from_bytes(buf[16:18], "big")
                total = 18 + plen
                if len(buf) < total:
                    break
                del buf[:total]
                continue
            if enc == 0x450:
                if len(buf) < 24:
                    break
                clen = int.from_bytes(buf[20:24], "big")
                total = 24 + clen
                if len(buf) < total:
                    break
                del buf[:total]
                continue
            log(f"unsupported encoding 0x{enc:x}")
            return b""
    return b""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--user", default="tai")
    ap.add_argument("--password", default=os.environ.get("SS_PASSWORD", ""))
    ap.add_argument("--kind", choices=["swipe", "scroll"], default="swipe")
    ap.add_argument("--motion", type=int, default=1)
    ap.add_argument("--swipe-mask", type=int, default=1)
    ap.add_argument("--progress", type=float, default=1.0)
    ap.add_argument("--px", type=float, default=0.5)
    ap.add_argument("--py", type=float, default=0.5)
    ap.add_argument("--vx", type=float, default=0.0)
    ap.add_argument("--vy", type=float, default=0.0)
    ap.add_argument("--vz", type=float, default=0.0)
    ap.add_argument("--flags", type=int, default=0)
    ap.add_argument("--flavor", type=int, default=0)
    ap.add_argument("--dx", type=float, default=0.0)
    ap.add_argument("--dy", type=float, default=0.0)
    ap.add_argument("--dz", type=float, default=0.0)
    ap.add_argument("--phase", type=int, default=0)
    ap.add_argument("--mask", type=int, default=0)
    ap.add_argument("--natural", type=int, default=0)
    ap.add_argument("--delay", type=float, default=0.7)
    ap.add_argument("--sequence", action="store_true", help="send scroll begin/changed/changed/end phases")
    ap.add_argument("--series", action="store_true", help="send gesture begin + gesture + end")
    ap.add_argument("--out", default="artifacts/gesture.png")
    args = ap.parse_args()

    with socket.create_connection((args.host, args.port), timeout=15) as s:
        s.settimeout(20)
        info = authenticate(s, args.user, args.password)
        width, height = info["width"], info["height"]
        wrap = hashlib.sha256(info["K"]).digest()[:16]
        s.sendall(VIEWER_PRELUDE)
        hdr = recv_exact(s, 4)
        nrects = int.from_bytes(hdr[2:4], "big")
        key = iv = None
        for _ in range(nrects):
            _x, _y, _w, _h, enc = struct.unpack(">HHHHI", recv_exact(s, 12))
            if enc != 0x44F:
                raise RuntimeError(f"expected rekey, got 0x{enc:x}")
            p = recv_exact(s, 36)
            key = aes_ecb_decrypt(wrap, p[4:20])
            iv = aes_ecb_decrypt(wrap, p[20:36])
        s.sendall(SET_ENCRYPTION_2)
        send_rl = RecordLayer(key, iv, "client")
        recv_rl = RecordLayer(key, iv, "server")
        s.sendall(send_rl.encrypt(SET_ENCODINGS))

        if args.kind == "swipe":
            g = gesture_swipe(args.motion, args.swipe_mask, args.progress, args.px, args.py,
                              args.vx, args.vy, args.vz, args.flags, args.flavor, width // 2, height // 2)
            if args.series:
                b = gesture_begin_end(1, width // 2, height // 2)
                e = gesture_begin_end(2, width // 2, height // 2)
                log(f"sending begin ({len(b)}), swipe ({len(g)}), end ({len(e)})")
                s.sendall(send_rl.encrypt(b))
                time.sleep(0.05)
                s.sendall(send_rl.encrypt(g))
                time.sleep(0.05)
                s.sendall(send_rl.encrypt(e))
            else:
                log(f"sending {args.kind} gesture ({len(g)} bytes): {g.hex()}")
                s.sendall(send_rl.encrypt(g))
        elif args.sequence:
            # CGSGesturePhase: began=1, changed=2, ended=4. Send one record per phase.
            for phase, dy in ((1, 0.0), (2, args.dy), (2, args.dy), (4, 0.0)):
                g = gesture_scroll(args.dx, dy, args.dz, args.natural, phase, args.mask, width // 2, height // 2)
                log(f"scroll phase={phase} dy={dy} ({len(g)} bytes)")
                s.sendall(send_rl.encrypt(g))
                time.sleep(0.05)
        else:
            g = gesture_scroll(args.dx, args.dy, args.dz, args.natural, args.phase, args.mask, width // 2, height // 2)
            log(f"sending {args.kind} gesture ({len(g)} bytes): {g.hex()}")
            s.sendall(send_rl.encrypt(g))
        time.sleep(args.delay)

        pixels = capture_frame(s, send_rl, recv_rl, width, height)
        log(f"captured {len(pixels)} pixel bytes")
        if len(pixels) == width * height * 4:
            write_png(args.out, width, height, pixels)
            log(f"wrote {args.out}")
        else:
            log("incomplete frame")
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
