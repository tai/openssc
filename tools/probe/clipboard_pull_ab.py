#!/usr/bin/env python3
"""Dump the RAW server->viewer 0x1f ClipboardSend for a specific remote
clipboard state, then decode EVERY pasteboard item fully (UTI, aliases, data
length, decoded content).

Throwaway probe for the "pull from remote" A/B experiment. Does not touch
product code. One run = one state = one fetch with flags=0 (full data).

Usage:
    python3 tools/probe/clipboard_pull_ab.py --out artifacts/pull-ab/<slug>
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
import zlib

sys.path.insert(0, os.path.dirname(__file__))
from apple_client import authenticate, log, recv_exact  # noqa: E402
from apple_session import (  # noqa: E402
    RecordLayer,
    SET_ENCODINGS,
    SET_ENCRYPTION_2,
    VIEWER_PRELUDE,
    read_ciphertext_record,
)

AUTO_PASTEBOARD = bytes([0x15, 0, 0, 1, 0, 0, 0, 0])


def clipboard_fetch(flags: int = 0) -> bytes:
    return bytes([0x0B, flags, 0, 0, 0, 0, 0, 0])


def aes_ecb_decrypt(key: bytes, data: bytes) -> bytes:
    r = subprocess.run(
        ["openssl", "enc", "-d", "-aes-128-ecb", "-K", key.hex(), "-nopad"],
        input=data,
        capture_output=True,
    )
    if r.returncode != 0:
        raise RuntimeError(r.stderr.decode(errors="replace"))
    return r.stdout


def be16(b: bytes, o: int) -> int:
    return int.from_bytes(b[o : o + 2], "big")


def be32(b: bytes, o: int) -> int:
    return int.from_bytes(b[o : o + 4], "big")


def describe_data(data: bytes) -> str:
    """Best-effort human rendering of one flavour's payload."""
    if not data:
        return "(empty = promise/no data)"
    if data.startswith(b"file://"):
        return f"url-string {data.decode('utf-8', 'replace')!r}"
    if data.startswith(b"bplist00"):
        inner = f"/tmp/pull-ab.plist-{os.getpid()}.bin"
        with open(inner, "wb") as f:
            f.write(data)
        r = subprocess.run(["plutil", "-p", inner], capture_output=True, timeout=5)
        os.unlink(inner)
        if r.returncode == 0:
            return f"binary-plist {r.stdout.decode(errors='replace')!r}"
        return f"binary-plist (malformed?) {data.hex()}"
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return f"binary {len(data)} bytes, head: {data[:24].hex()}"
    # utf-16 archives never decode as utf-8 cleanly, so this is a text flavour
    if text.startswith("file:"):
        return f"url-string {text!r}"
    return f"text {text!r}"


def decode_utf16(data: bytes) -> str:
    if data[:2] in (b"\xff\xfe", b"\xfe\xff"):
        return data.decode("utf-16", "replace")
    # No BOM: macOS utf16 clipboard data is host-endian; big-endian on the wire.
    units = [int.from_bytes(data[i : i + 2], "big") for i in range(0, len(data) - 1, 2)]
    return "".join(map(chr, units))


def parse_archive(archive: bytes) -> list[dict]:
    """Port of rfb-proto pasteboard_items: u32 item_count, then per item
    u32 uti_len, uti, u32 reserved, u32 alias_count, [namelen name valuelen
    value]*, u32 data_len, data."""
    items: list[dict] = []
    if not archive:
        return items
    p = 0
    count = be32(archive, p)
    p += 4
    for _ in range(count):
        if p + 4 > len(archive):
            break
        uti_len = be32(archive, p)
        p += 4
        uti = archive[p : p + uti_len].decode("utf-8", "replace")
        p += uti_len
        _reserved = be32(archive, p)
        p += 4
        alias_count = be32(archive, p)
        p += 4
        aliases = []
        for _ in range(alias_count):
            nlen = be32(archive, p)
            p += 4
            name = archive[p : p + nlen].decode("utf-8", "replace")
            p += nlen
            vlen = be32(archive, p)
            p += 4
            value = archive[p : p + vlen].decode("utf-8", "replace")
            p += vlen
            aliases.append((name, value))
        dlen = be32(archive, p)
        p += 4
        data = archive[p : p + dlen]
        p += dlen
        items.append({"uti": uti, "aliases": aliases, "data": data})
    return items


def wait_for_clipboard_send(rl, deadline_s: float) -> tuple[bytes, bytes] | None:
    """Read records until a 0x1f ClipboardSend arrives. Returns (raw_body,
    decompressed_archive). Skips FB updates / misc / filecopy traffic."""
    pending = bytearray()
    end = time.time() + deadline_s
    while time.time() < end:
        s.settimeout(max(0.5, end - time.time()))
        try:
            ct = read_ciphertext_record(s)
        except (TimeoutError, socket.timeout):
            break
        body, ok = rl.decrypt(ct)
        if not ok:
            log("WARN: sha1 tag mismatch on inbound record")
        pending.extend(body)
        # split complete messages out of `pending`
        while pending:
            t = pending[0]
            if t == 0x14:
                need = 8
            elif t == 0x02:
                need = 1
            elif t == 0x03:
                if len(pending) < 8:
                    break
                need = 8 + be32(pending, 4)
            elif t == 0x01:
                if len(pending) < 6:
                    break
                need = 6 + 6 * be16(pending, 4)
            elif t == 0x1F:
                if len(pending) < 16:
                    break
                need = 16 + be32(pending, 12)
            elif t == 0x22:
                if len(pending) < 6:
                    break
                need = 6 + be32(pending, 2)
            else:
                log(f"unexpected record message type 0x{t:02x}, pending={pending[:32].hex()}")
                pending.clear()
                break
            if len(pending) < need:
                break
            msg = bytes(pending[:need])
            del pending[:need]
            if t == 0x14:
                log(f"0x14 MiscStatus flags={be16(msg,4)} cmd={be16(msg,6)}")
            elif t == 0x1F:
                log(f"got 0x1f: total={len(msg)} raw={msg[:16].hex()}")
                raw, archive = decode_send(msg)
                if archive is not None:
                    return raw, archive
                return raw, b""
            time.sleep(0.001)
    return None


def decode_send(msg: bytes) -> tuple[bytes, bytes | None]:
    flag = msg[2]
    uncompressed = be32(msg, 8)
    compressed = msg[16:]
    log(f"0x1f hdr: flag={flag} uncompressed={uncompressed} compressed={len(compressed)}")
    raw = msg
    if uncompressed == 0:
        log("promised/no-data send (uncompressed_size == 0)")
        return raw, b""
    try:
        out = zlib.decompressobj(wbits=15).decompress(compressed)
    except zlib.error:
        out = zlib.decompressobj(wbits=-15).decompress(compressed)
    if len(out) != uncompressed:
        log(f"WARN: inflated {len(out)} bytes, header says {uncompressed}")
    return raw, out


def run(host: str, out_prefix: str, flags: int, wait_s: float) -> int:
    global s
    s = socket.create_connection((host, 5900), timeout=15)
    s.settimeout(15)
    info = authenticate(s, "tai", os.environ.get("SS_PASSWORD", ""))
    K = info["K"]
    wrap = hashlib.sha256(K).digest()[:16]

    s.sendall(VIEWER_PRELUDE)
    hdr = recv_exact(s, 4)
    if hdr[0] != 0:
        log(f"unexpected cleartext message {hdr.hex()}")
        return 1
    for _ in range(int.from_bytes(hdr[2:4], "big")):
        rect = recv_exact(s, 12)
        x, y, w, h, enc = struct.unpack(">HHHHI", rect)
        if enc != 0x44F:
            continue
        payload = recv_exact(s, 36)
        enc_key, enc_iv = payload[4:20], payload[20:36]
        key = aes_ecb_decrypt(wrap, enc_key)
        iv = aes_ecb_decrypt(wrap, enc_iv)

    send_rl = RecordLayer(key, iv, "client")
    recv_rl = RecordLayer(key, iv, "server")

    def send(body: bytes) -> None:
        s.sendall(send_rl.encrypt(body))
        log(f"sent record: {len(body)} bytes body")

    s.sendall(SET_ENCRYPTION_2)  # cleartext, activates the record layer
    send(SET_ENCODINGS)
    send(AUTO_PASTEBOARD)
    time.sleep(0.3)
    log(f" ClipboardFetch flags={flags}")
    send(clipboard_fetch(flags))

    result = wait_for_clipboard_send(recv_rl, wait_s)
    if result is None:
        log("NO 0x1f ClipboardSend received within window")
        return 1
    raw, archive = result
    os.makedirs(os.path.dirname(out_prefix) or ".", exist_ok=True)
    with open(out_prefix + ".bin", "wb") as f:
        f.write(raw)
    if archive:
        with open(out_prefix + ".archive", "wb") as f:
            f.write(archive)
    log(f"raw saved: {out_prefix}.bin ({len(raw)} bytes)")

    if archive:
        log(f"archive: {len(archive)} bytes (uncompressed)")
        try:
            items = parse_archive(archive)
        except Exception as e:
            log(f"archive PARSE FAILED: {e}; head hex:")
            log(archive[:128].hex())
            return 2
        log(f"item_count = {len(items)}")
        if not items:
            log(f"archive head hex: {archive[:64].hex()}")
        for i, it in enumerate(items):
            d = it["data"]
            log(
                f"item[{i}] uti={it['uti']!r} data_len={len(d)} "
                f"data_head={d[:24].hex()}"
            )
            for name, value in it["aliases"]:
                log(f"    alias {name} = {value}")
            uti = it["uti"]
            if "utf16" in uti:
                log(f"    content: {describe_data(d)}")
                log(f"    utf16-decoded: {decode_utf16(d)!r}")
            elif "utf8" in uti or "text" in uti:
                log(f"    content: {describe_data(d)}")
            elif "url" in uti or "alias" in uti or "bookmark" in uti:
                log(f"    content: {describe_data(d)}")
            else:
                log(f"    content: {describe_data(d)}")
    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="mm.local")
    ap.add_argument("--out", required=True, help="output prefix, e.g. artifacts/pull-ab/file-pathname-flags0")
    ap.add_argument("--flags", type=int, default=0)
    ap.add_argument("--wait", type=float, default=5.0)
    args = ap.parse_args()
    rc = run(args.host, args.out, args.flags, args.wait)
    sys.exit(rc)
