#!/usr/bin/env python3
"""Parse type-0 MVS commands independently of the pixel payload (Python 3.9).

Evidence: 2026-11-04-codex-mvs-streams; see docs/findings/.
Usage: python3 tools/probe/mvs_commands.py artifacts/mvs-4.bin --tiles 64000
"""
import argparse
import collections
import json


class Bits:
    def __init__(self, data):
        self.data = data
        self.pos = 0

    def read(self, count):
        if count < 0 or self.pos + count > len(self.data) * 8:
            raise ValueError('truncated command stream')
        value = 0
        for _ in range(count):
            value = (value << 1) | ((self.data[self.pos // 8] >> (7 - self.pos % 8)) & 1)
            self.pos += 1
        return value


def read_run(bits):
    """Return additional tiles, following the real framework run reader."""
    if not bits.read(1):
        return 0
    short = bits.read(4)
    if short != 15:
        return short + 1
    value = 0
    for shift in (0, 7, 14):
        byte = bits.read(8)
        value |= (byte & 127) << shift
        if not byte & 128:
            return 16 + value
    raise ValueError('run exceeds three escape bytes')


def parse_commands(body, expected_tiles):
    if len(body) < 6 or body[0] != 0:
        raise ValueError('expected type-0 MVS body')
    offset = int.from_bytes(body[3:6], 'big')
    if not 6 <= offset <= len(body):
        raise ValueError('invalid payload offset')
    bits = Bits(body[6:offset])
    flag = bits.read(1)
    commands = []
    tiles = 0
    while tiles < expected_tiles:
        code = bits.read(3)
        count = read_run(bits) + 1
        if tiles + count > expected_tiles:
            raise ValueError('run exceeds tile grid')
        commands.append((code, count))
        tiles += count
    command_bits = bits.pos
    marker = bits.read(8)
    if marker != ord('m'):
        raise ValueError('missing command-stream m marker')
    return flag, commands, command_bits, offset


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('path')
    parser.add_argument('--tiles', type=int, required=True)
    args = parser.parse_args()
    with open(args.path, 'rb') as src:
        body = src.read()
    flag, commands, consumed, offset = parse_commands(body, args.tiles)
    counts = collections.Counter()
    for code, count in commands:
        counts[code] += count
    print(json.dumps(dict(flag=flag, tiles=sum(counts.values()), commands=len(commands),
                          command_bits=consumed, command_bytes=offset-6,
                          payload_bytes=len(body)-offset, tiles_by_code=dict(counts)), sort_keys=True))


if __name__ == '__main__':
    main()
