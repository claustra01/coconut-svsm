#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Receive CTCP UDP records and print JSON lines."""

import argparse
import socket
import struct
import sys
from tcp_telemetry import emit_frame


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4050)
    args = parser.parse_args()
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
        receiver.bind((args.bind, args.port))
        print(f"listening on UDP {args.bind}:{args.port}", file=sys.stderr)
        while True:
            raw, _ = receiver.recvfrom(65535)
            try:
                emit_frame(raw)
            except (ValueError, struct.error) as error:
                print(f"invalid CTCP datagram: {error}", file=sys.stderr)


if __name__ == "__main__":
    main()
