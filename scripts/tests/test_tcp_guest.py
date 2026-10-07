# SPDX-License-Identifier: MIT OR Apache-2.0
import importlib.util
import unittest
from pathlib import Path
from test_tcp_telemetry import RECORD

spec = importlib.util.spec_from_file_location(
    "relay", Path(__file__).resolve().parents[1] / "tcp-guest-relay.py")
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class GuestBatch(unittest.TestCase):
    def test_split_batch(self):
        self.assertEqual(list(relay.frames(RECORD * 3)), [RECORD] * 3)
        self.assertEqual(list(relay.frames(b"")), [])

    def test_reject_partial_batch(self):
        with self.assertRaises(ValueError):
            list(relay.frames(RECORD + b"x"))
