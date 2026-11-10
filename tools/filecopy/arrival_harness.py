#!/usr/bin/env python3
"""File-arrival harness for the file-copy milestone (NOT product code).

The file-copy UX is drag-based:
  * local -> remote: a file dropped on the openssc window is sent;
  * remote -> local: files are saved to a receive directory locally.

This harness verifies the *arrival* half of those checks, independent of the
protocol that will move the bytes:
  * local -> remote: the file exists on mm.local with the same size + sha1;
  * remote -> local: the file exists in a known local directory with the same
    size + sha1.

It also has a simulation command (`send-remote`) that copies a file to the far
end over ssh, so the verifier can be exercised before the transfer protocol
exists. That command is NOT the product path: the product will move bytes over
the RFB file-copy channel.

Commands
--------
  hash FILE...                     print local size/sha1
  remote-hash PATH...              print size/sha1 for paths on --host
  send-remote FILE REMOTE_DIR      simulation: copy FILE into REMOTE_DIR on
                                   --host over ssh, print the far-end sha1
  wait-local DIR (--sha1 H | --file F) [--timeout SEC]
                                   poll a local directory until a file with
                                   that sha1 appears (remote->local check)
  wait-remote DIR (--sha1 H | --file F) [--timeout SEC]
                                   poll a directory on --host until a file with
                                   that sha1 appears (local->remote check)
  selfcheck                        local + remote verifier self-test using a
                                   random 4 KiB file; cleans up after itself

Exit status: 0 = match, 2 = timeout/no match, 1 = usage/transport error.
`--timeout` defaults to 10 s; polling interval is 250 ms.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

POLL_SECONDS = 0.25


def run(cmd, stdin: bytes | None = None) -> bytes:
    result = subprocess.run(
        cmd, input=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False
    )
    if result.returncode != 0:
        shown = " ".join(str(c) for c in cmd)
        sys.stderr.write(
            f"command failed ({result.returncode}): {shown}\n"
            f"{result.stderr.decode(errors='replace')}"
        )
        raise SystemExit(1)
    return result.stdout


def ssh(args: argparse.Namespace, command: str) -> bytes:
    return run(
        ["ssh", "-o", "BatchMode=yes", f"{args.user}@{args.host}", command]
    )


def sha1_bytes(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def sha1_file(path: str) -> str:
    digest = hashlib.sha1()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def report_local(path: str) -> str:
    digest = sha1_file(path)
    print(f"file={path} size={os.path.getsize(path)} sha1={digest}")
    return digest


def remote_stat(args: argparse.Namespace, path: str) -> tuple[int, str]:
    script = (
        f"p={shlex.quote(path)}; "
        f"if [ ! -f \"$p\" ]; then exit 3; fi; "
        f"stat -f %z \"$p\"; shasum -a 1 \"$p\" | cut -d' ' -f1"
    )
    out = ssh(args, script).decode().split()
    return int(out[0]), out[1]


def wait_local(args: argparse.Namespace, directory: str, expected: str) -> int:
    deadline = time.time() + args.timeout
    seen = 0
    while True:
        for root, _dirs, files in os.walk(directory):
            for name in files:
                path = os.path.join(root, name)
                seen += 1
                try:
                    if sha1_file(path) == expected:
                        print(
                            f"arrived local: file={path} "
                            f"size={os.path.getsize(path)} sha1={expected}"
                        )
                        return 0
                except OSError:
                    continue
        if time.time() >= deadline:
            print(
                f"wait-local: no file with sha1={expected} under {directory} "
                f"after {args.timeout:.1f}s ({seen} file(s) scanned)"
            )
            return 2
        time.sleep(POLL_SECONDS)


def remote_list_hashes(args: argparse.Namespace, directory: str) -> list[str]:
    """sha1 of every regular file under a remote directory (flat find)."""
    quoted = shlex.quote(directory)
    script = (
        f"if [ ! -d {quoted} ]; then exit 0; fi; "
        f"cd {quoted} && find . -type f -exec shasum -a 1 {{}} + 2>/dev/null"
    )
    out = ssh(args, script).decode(errors="replace")
    hashes = []
    for line in out.splitlines():
        parts = line.split(None, 1)
        if len(parts) == 2 and len(parts[0]) == 40:
            hashes.append(parts[0])
    return hashes


def wait_remote(args: argparse.Namespace, directory: str, expected: str) -> int:
    deadline = time.time() + args.timeout
    while True:
        hashes = remote_list_hashes(args, directory)
        if expected in hashes:
            quoted = shlex.quote(directory)
            script = (
                f"cd {quoted} && find . -type f -exec shasum -a 1 {{}} + "
                f"2>/dev/null | awk '$1==\"{expected}\" {{ $1=\"\"; sub(/^ +/,\"\"); print; exit }}'"
            )
            rel = ssh(args, script).decode(errors="replace").strip()
            path = os.path.join(directory, rel[2:] if rel.startswith("./") else rel)
            size, digest = remote_stat(args, path)
            print(f"arrived remote: file={args.host}:{path} size={size} sha1={digest}")
            return 0
        if time.time() >= deadline:
            print(
                f"wait-remote: no file with sha1={expected} under "
                f"{args.host}:{directory} after {args.timeout:.1f}s "
                f"({len(hashes)} file(s) scanned)"
            )
            return 2
        time.sleep(POLL_SECONDS)


def resolve_expected(args: argparse.Namespace) -> str:
    if args.file:
        return sha1_file(args.file)
    if args.sha1:
        return args.sha1.lower()
    raise SystemExit("give --sha1 HEX or --file PATH")


def send_remote(args: argparse.Namespace, path: str, remote_dir: str) -> int:
    """Simulation only: copy a file to the far end over ssh."""
    if not os.path.isfile(path):
        raise SystemExit(f"not a file: {path}")
    name = os.path.basename(path)
    quoted_dir = shlex.quote(remote_dir)
    ssh(args, f"mkdir -p {quoted_dir}")
    remote_path = os.path.join(remote_dir, name)
    with open(path, "rb") as handle:
        run(
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                f"{args.user}@{args.host}",
                f"cat > {shlex.quote(remote_path)}",
            ],
            stdin=handle.read(),
        )
    size, digest = remote_stat(args, remote_path)
    print(f"simulated send: file={args.host}:{remote_path} size={size} sha1={digest}")
    return 0 if digest == sha1_file(path) else 2


def selfcheck(args: argparse.Namespace) -> int:
    root = tempfile.mkdtemp(prefix="file-arrival-selfcheck-")
    local_dir = os.path.join(root, "local")
    os.makedirs(local_dir)
    payload = os.urandom(4096)
    source = os.path.join(root, "selfcheck.bin")
    with open(source, "wb") as handle:
        handle.write(payload)
    expected = sha1_bytes(payload)
    print(f"selfcheck: file={source} size={len(payload)} sha1={expected}")

    shutil.copy2(source, os.path.join(local_dir, "selfcheck.bin"))
    local = wait_local(args, local_dir, expected)
    if local != 0:
        print("selfcheck: FAIL (local verifier)")
        return 2

    remote_dir = f"/tmp/file-arrival-selfcheck-{os.getpid()}"
    send = send_remote(args, source, remote_dir)
    if send != 0:
        print("selfcheck: FAIL (simulated remote send)")
        return 2
    remote = wait_remote(args, remote_dir, expected)
    run(
        [
            "ssh",
            "-o",
            "BatchMode=yes",
            f"{args.user}@{args.host}",
            f"rm -rf {shlex.quote(remote_dir)}",
        ]
    )
    shutil.rmtree(root, ignore_errors=True)
    if remote != 0:
        print("selfcheck: FAIL (remote verifier)")
        return 2
    print("selfcheck: PASS (local and remote verifiers agree on size + sha1)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="File-arrival harness for drag-based file copy; not product code."
    )
    parser.add_argument("--host", default="mm.local")
    parser.add_argument("--user", default="tai")
    parser.add_argument(
        "--timeout", type=float, default=10.0, help="wait timeout in seconds"
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("hash", help="print local size/sha1")
    p.add_argument("files", nargs="+")

    p = sub.add_parser("remote-hash", help="print size/sha1 for remote paths")
    p.add_argument("paths", nargs="+")

    p = sub.add_parser("send-remote", help="SIMULATION: copy a file to the far end")
    p.add_argument("file")
    p.add_argument("remote_dir")

    for name, help_text in (
        ("wait-local", "poll a local directory for an arriving file"),
        ("wait-remote", "poll a remote directory for an arriving file"),
    ):
        p = sub.add_parser(name, help=help_text)
        p.add_argument("directory")
        p.add_argument("--sha1", help="expected sha1")
        p.add_argument("--file", help="local file whose sha1 is expected")

    sub.add_parser("selfcheck", help="run the verifier self-test")

    args = parser.parse_args()
    if args.command == "hash":
        hashes = [report_local(path) for path in args.files]
        return 0 if hashes else 2
    if args.command == "remote-hash":
        for path in args.paths:
            size, digest = remote_stat(args, path)
            print(f"file={args.host}:{path} size={size} sha1={digest}")
        return 0
    if args.command == "send-remote":
        return send_remote(args, args.file, args.remote_dir)
    if args.command == "wait-local":
        return wait_local(args, args.directory, resolve_expected(args))
    if args.command == "wait-remote":
        return wait_remote(args, args.directory, resolve_expected(args))
    if args.command == "selfcheck":
        return selfcheck(args)
    return 1


if __name__ == "__main__":
    sys.exit(main())
