# SPDX-License-Identifier: MIT OR Apache-2.0
import importlib.util
import struct
import unittest
from pathlib import Path
from test_tcp_telemetry import RECORD

spec = importlib.util.spec_from_file_location(
    "relay", Path(__file__).resolve().parents[1] / "tcp-shmem-relay.py")
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class RingReader(unittest.TestCase):
    def snapshot(self):
        data = bytearray(relay.RING_SIZE)
        struct.pack_into("<8sIIII", data, 0, b"CTCPRNG1", relay.RING_SIZE,
                         relay.SLOT_COUNT, relay.SLOT_SIZE, 56)
        return data

    def add(self, data, sequence, end=None):
        offset = 64 + ((sequence - 1) % relay.SLOT_COUNT) * 72
        frame = bytearray(RECORD)
        struct.pack_into("!Q", frame, 8, sequence)
        struct.pack_into("<Q", data, offset, sequence)
        data[offset+8:offset+64] = frame
        struct.pack_into("<Q", data, offset+64, sequence if end is None else end)

    def test_wrap_sort_filter_and_incomplete_slot(self):
        data = self.snapshot()
        for sequence in (127, 128, 129):
            self.add(data, sequence)
        self.add(data, 130, end=0)
        self.assertEqual([seq for seq, _ in relay.read_records(data, 0)], [127, 128, 129])
        self.assertEqual([seq for seq, _ in relay.read_records(data, 128)], [129])

    def test_reject_wrong_memory(self):
        with self.assertRaises(ValueError):
            relay.read_records(bytes(relay.RING_SIZE), 0)
