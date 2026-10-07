#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Read the published TCP ring through a local QEMU QMP socket."""

import argparse
import json
import socket
import struct
import tempfile
import time
from pathlib import Path

from tcp_telemetry import FRAME, decode_frame, emit_frame

RING_SIZE = 16384
HEADER_SIZE = 64
SLOT_SIZE = 72
SLOT_COUNT = 128


def read_records(snapshot: bytes, after: int):
    if len(snapshot) != RING_SIZE:
        raise ValueError("short shared ring snapshot")
    header = struct.unpack_from("<8sIIII", snapshot)
    if header != (b"CTCPRNG1", RING_SIZE, SLOT_COUNT, SLOT_SIZE, FRAME.size):
        raise ValueError("no TCP shared ring at the supplied GPA")
    records = []
    for index in range(SLOT_COUNT):
        offset = HEADER_SIZE + index * SLOT_SIZE
        begin = struct.unpack_from("<Q", snapshot, offset)[0]
        end = struct.unpack_from("<Q", snapshot, offset + 8 + FRAME.size)[0]
        if begin <= after or begin != end:
            continue
        raw = snapshot[offset + 8:offset + 8 + FRAME.size]
        event = decode_frame(raw)
        if event["sequence"] == begin:
            records.append((begin, raw))
    return sorted(records)


class Qmp:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.connect(path)
        self.reader = self.socket.makefile("rb")
        greeting = json.loads(self.reader.readline())
        if "QMP" not in greeting:
            raise RuntimeError("not a QMP socket")
        self.execute("qmp_capabilities")

    def execute(self, command, arguments=None):
        request = {"execute": command}
        if arguments is not None:
            request["arguments"] = arguments
        self.socket.sendall(json.dumps(request).encode() + b"\n")
        while True:
            line = self.reader.readline()
            if not line:
                raise EOFError("QEMU disconnected")
            reply = json.loads(line)
            if "error" in reply:
                raise RuntimeError(reply["error"])
            if "return" in reply:
                return reply["return"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qmp", required=True, help="local QMP Unix socket")
    parser.add_argument("--gpa", required=True, type=lambda s: int(s, 0),
                        help="gpa printed in 'TCP shared ring' at first observation")
    parser.add_argument("--interval", type=float, default=0.2)
    args = parser.parse_args()
    if args.interval <= 0:
        parser.error("interval must be positive")
    qmp = Qmp(args.qmp)
    last = 0
    # pmemsave writes on the QEMU host. This collector runs on that same host,
    # under an account whose temporary directory QEMU can access.
    with tempfile.TemporaryDirectory(prefix="svsm-tcp-") as directory:
        path = Path(directory) / "ring.bin"
        while True:
            qmp.execute("pmemsave", {"val": args.gpa, "size": RING_SIZE,
                                     "filename": str(path)})
            for sequence, raw in read_records(path.read_bytes(), last):
                emit_frame(raw)
                last = sequence
            time.sleep(args.interval)


if __name__ == "__main__":
    main()
