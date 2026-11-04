"""Synthetic framing checks; no captured desktop contents required."""
import unittest
from mvs_commands import Bits, parse_commands, read_run


def packed(bits):
    bits += '0' * (-len(bits) % 8)
    return int(bits, 2).to_bytes(len(bits) // 8, 'big')


class CommandsTest(unittest.TestCase):
    def test_run_boundaries(self):
        for encoded, expected in [('0', 0), ('10000', 1), ('11110', 15),
                                  ('11111'+'00000000', 16),
                                  ('11111'+'11111111'+'00000001', 271),
                                  ('11111'+'10000000'+'10000000'+'00000001', 16400)]:
            self.assertEqual(read_run(Bits(packed(encoded))), expected)

    def test_separate_payload(self):
        # Leading flag, solid x1, above x5, command marker, arbitrary payload.
        commands = packed('0'+'100'+'0'+'001'+'10011'+format(ord('m'), '08b'))
        body = bytes([0, 15, 25]) + (6+len(commands)).to_bytes(3, 'big') + commands + b'payload'
        flag, parsed, consumed, offset = parse_commands(body, 6)
        self.assertEqual((flag, parsed, consumed), (0, [(4, 1), (1, 5)], 13))
        self.assertEqual(body[offset:], b'payload')
        with self.assertRaises(ValueError):
            parse_commands(body, 5)
        with self.assertRaises(ValueError):
            parse_commands(body[:offset-1], 6)

    def test_truncation(self):
        with self.assertRaises(ValueError):
            read_run(Bits(b'\xff'))
        with self.assertRaises(ValueError):
            parse_commands(b'', 1)


if __name__ == '__main__':
    unittest.main()
