#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Decode the 56-byte CTCP telemetry record used by the SVSM transports."""

import ipaddress
import json
import struct

FRAME = struct.Struct("!4sBBHQQQB3x4sH4sHQ")


def decode_frame(raw: bytes) -> dict:
    (magic, version, kind, length, sequence, observed_tsc, socket_gva, state,
     source_ip, source_port, destination_ip, destination_port, dropped) = FRAME.unpack(raw)
    if (magic, version, kind, length) != (b"CTCP", 1, 1, FRAME.size):
        raise ValueError("invalid CTCP frame")
    return {"version": version, "event": "tcp_connection", "sequence": sequence,
            "observed_tsc": observed_tsc, "socket_gva": f"0x{socket_gva:x}",
            "state": state, "source_ip": str(ipaddress.IPv4Address(source_ip)),
            "source_port": source_port,
            "destination_ip": str(ipaddress.IPv4Address(destination_ip)),
            "destination_port": destination_port, "dropped_events": dropped}


def emit_frame(raw: bytes) -> None:
    print(json.dumps(decode_frame(raw), separators=(",", ":")), flush=True)
