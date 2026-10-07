# SPDX-License-Identifier: MIT OR Apache-2.0
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tcp_telemetry import decode_frame

RECORD = bytes.fromhex(
    "4354435001010038 0000000000000007 000000000000000b 000000000000000d"
    " 01000000 c0000201 04d2 c6336402 01bb 0000000000000011"
)


class WireFormat(unittest.TestCase):
    def test_known_record(self):
        self.assertEqual(decode_frame(RECORD), {
            "version": 1, "event": "tcp_connection", "sequence": 7,
            "observed_tsc": 11, "socket_gva": "0xd", "state": 1,
            "source_ip": "192.0.2.1", "source_port": 1234,
            "destination_ip": "198.51.100.2", "destination_port": 443,
            "dropped_events": 17,
        })

    def test_bad_header(self):
        for offset in (0, 4, 5, 7):
            bad = bytearray(RECORD)
            bad[offset] ^= 1
            with self.assertRaises(ValueError):
                decode_frame(bad)


if __name__ == "__main__":
    unittest.main()
