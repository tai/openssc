#!/usr/bin/env python3
"""M1: authenticate, enter the record layer, decode a Zlib framebuffer to PNG.

Usage:
    python3 tools/probe/apple_framebuffer.py --host mm.local --out artifacts/remote.png
"""

from __future__ import annotations

import argparse
import binascii
import hashlib
import os
import socket
import struct
import sys
import zlib

sys.path.insert(0, os.path.dirname(__file__))
from apple_client import authenticate, log, recv_exact  # noqa: E402
from apple_session import (  # noqa: E402
    SET_ENCODINGS,
    SET_ENCRYPTION_2,
    VIEWER_PRELUDE,
    RecordLayer,
    aes_ecb_decrypt,
)


def png_chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", binascii.crc32(tag + data) & 0xFFFFFFFF)


def write_png(path: str, width: int, height: int, bgra: bytes) -> None:
    assert len(bgra) == width * height * 4, f"{len(bgra)} != {width*height*4}"
    rows = bytearray()
    for y in range(height):
        rows.append(0)  # filter type none
        off = y * width * 4
        line = bgra[off : off + width * 4]
        # little-endian 32bpp, red shift 16 / green 8 / blue 0 -> B,G,R,0
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
    if n == 0 or n % 16:
        raise ValueError(f"bad record length {n}")
    return recv_exact(s, n)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--port", type=int, default=5900)
    ap.add_argument("--user", default="tai")
    ap.add_argument("--password", default=os.environ.get("SS_PASSWORD", ""))
    ap.add_argument("--out", default="artifacts/remote.png")
    ap.add_argument("--max-records", type=int, default=5000)
    args = ap.parse_args()

    with socket.create_connection((args.host, args.port), timeout=15) as s:
        s.settimeout(20)
        info = authenticate(s, args.user, args.password)
        width, height = info["width"], info["height"]
        K = info["K"]
        wrap = hashlib.sha256(K).digest()[:16]

        s.sendall(VIEWER_PRELUDE)
        hdr = recv_exact(s, 4)
        if hdr[0] != 0:
            raise RuntimeError(f"unexpected cleartext message {hdr.hex()}")
        nrects = int.from_bytes(hdr[2:4], "big")
        key = iv = None
        for _ in range(nrects):
            rect = recv_exact(s, 12)
            x, y, w, h, enc = struct.unpack(">HHHHI", rect)
            if enc != 0x44F:
                raise RuntimeError(f"expected rekey, got enc=0x{enc:x}")
            payload = recv_exact(s, 36)
            key = aes_ecb_decrypt(wrap, payload[4:20])
            iv = aes_ecb_decrypt(wrap, payload[20:36])
        log(f"record layer key={key.hex()} iv={iv.hex()}")

        s.sendall(SET_ENCRYPTION_2)
        send_rl = RecordLayer(key, iv, "client")
        recv_rl = RecordLayer(key, iv, "server")

        s.sendall(send_rl.encrypt(SET_ENCODINGS))
        req = struct.pack(">BBHHHH", 3, 0, 0, 0, width, height)
        s.sendall(send_rl.encrypt(req))
        log(f"requested framebuffer {width}x{height}")

        buf = bytearray()
        dec = None
        zremain = 0
        pixels = bytearray()
        done = False
        for recno in range(args.max_records):
            body, ok = recv_rl.decrypt(recv_record(s))
            if not ok:
                raise RuntimeError(f"integrity failure at record {recno}")
            buf.extend(body)

            while True:
                if dec is not None:
                    if not buf:
                        break
                    n = min(len(buf), zremain)
                    chunk = bytes(buf[:n])
                    del buf[:n]
                    zremain -= n
                    try:
                        out = dec.decompress(chunk)
                    except zlib.error as exc:
                        log(f"zlib error at record {recno}: {exc}")
                        log(f"  fed head={chunk[:16].hex()} tail={chunk[-16:].hex()} len={len(chunk)} zremain={zremain}")
                        raise
                    pixels.extend(out)
                    if recno % 20 == 0:
                        log(f"  record {recno}: out={len(out)} total={len(pixels)} zremain={zremain} eof={dec.eof}")
                    if dec.eof or zremain == 0:
                        log(f"zlib complete after {recno+1} records, {len(pixels)} bytes, eof={dec.eof}, zremain={zremain}")
                        done = True
                    break

                if len(buf) < 4:
                    break
                if buf[0] != 0:
                    log(f"non-FBU message type=0x{buf[0]:02x}, unconsumed={len(buf)}")
                    buf.clear()
                    break
                nrects = int.from_bytes(buf[2:4], "big")
                if nrects == 0:
                    del buf[:4]
                    continue
                if len(buf) < 16:
                    break
                x, y, w, h, enc = struct.unpack(">HHHHI", bytes(buf[4:16]))
                log(f"message: {nrects} rect(s), first x={x} y={y} w={w} h={h} enc=0x{enc:x}")
                if enc == 6:
                    if len(buf) < 20:
                        break
                    clen = int.from_bytes(buf[16:20], "big")
                    log(f"  zlib payload length={clen}")
                    dec = zlib.decompressobj()
                    zremain = clen
                    payload = bytes(buf[20:20 + zremain])
                    del buf[:20 + len(payload)]
                    zremain -= len(payload)
                    out = dec.decompress(payload)
                    pixels.extend(out)
                    if dec.eof or zremain == 0:
                        log(f"zlib complete immediately, {len(pixels)} bytes, eof={dec.eof}, zremain={zremain}")
                        done = True
                    break
                if enc in (0x451, 0x453):
                    if len(buf) < 18:
                        break
                    plen = int.from_bytes(buf[16:18], "big")
                    total = 18 + plen
                    if len(buf) < total:
                        break
                    log(f"  metadata enc=0x{enc:x} payload={plen} bytes: {bytes(buf[18:18+min(plen,40)]).hex()}")
                    del buf[:total]
                    continue
                if enc == 0:
                    need = 16 + w * h * 4
                    if len(buf) < need:
                        break
                    pixels.extend(buf[16:need])
                    del buf[:need]
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
                log(f"unsupported encoding 0x{enc:x}; stopping")
                done = True
                break

            if done:
                break

        log(f"decoded {len(pixels)} pixel bytes (expected {width*height*4})")
        if len(pixels) != width * height * 4:
            log("incomplete framebuffer")
            return 1
        os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
        write_png(args.out, width, height, bytes(pixels))
        log(f"wrote {args.out}")
        # sample a few pixels
        for px in (0, width // 2, width * height // 2):
            off = px * 4
            log(f"pixel {px}: {pixels[off:off+4].hex()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
