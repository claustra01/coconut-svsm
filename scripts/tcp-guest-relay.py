#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Read /dev/socktrawl in the guest and forward CTCP frames as UDP datagrams."""

import argparse
import os
import socket
import time
from tcp_telemetry import FRAME, decode_frame


def frames(batch):
    if len(batch) % FRAME.size:
        raise ValueError("partial CTCP frame from guest driver")
    for offset in range(0, len(batch), FRAME.size):
        raw = batch[offset:offset + FRAME.size]
        decode_frame(raw)
        yield raw


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", default="/dev/socktrawl")
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, default=4050)
    parser.add_argument("--interval", type=float, default=1.0)
    args = parser.parse_args()
    if args.interval <= 0:
        parser.error("interval must be positive")
    destination = (socket.gethostbyname(args.host), args.port)
    fd = os.open(args.device, os.O_RDONLY | os.O_NONBLOCK)
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            while True:
                try:
                    batch = os.read(fd, 4096)
                except BlockingIOError:
                    time.sleep(args.interval)
                    continue
                if not batch:
                    raise EOFError("guest telemetry device closed")
                for raw in frames(batch):
                    sender.sendto(raw, destination)
    finally:
        os.close(fd)


if __name__ == "__main__":
    main()
