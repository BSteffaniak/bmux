#!/usr/bin/env python3
import json
import struct
import sys

MAGIC = b"BMUXPRC1"


def read_frame(stdin_bytes: bytes) -> bytes:
    if len(stdin_bytes) < len(MAGIC) + 4:
        raise ValueError("truncated frame header")
    if not stdin_bytes.startswith(MAGIC):
        raise ValueError("missing BMUXPRC1 prefix")
    payload_len = struct.unpack(">I", stdin_bytes[len(MAGIC) : len(MAGIC) + 4])[0]
    start = len(MAGIC) + 4
    end = start + payload_len
    if len(stdin_bytes) != end:
        raise ValueError("invalid frame length")
    return stdin_bytes[start:end]


def encode_frame(payload: bytes) -> bytes:
    return MAGIC + struct.pack(">I", len(payload)) + payload


def handle_request(payload: bytes) -> dict:
    try:
        request = json.loads(payload.decode("utf-8"))
    except Exception:
        return {
            "Error": {
                "protocol_version": 1,
                "details": "invalid request payload",
                "status": 2,
            }
        }

    if "Command" in request:
        return {
            "Command": {
                "protocol_version": 1,
                "status": 0,
                "outcome": {"effects": []},
            }
        }

    return {
        "Error": {
            "protocol_version": 1,
            "details": "unsupported request kind in python example",
            "status": 2,
        }
    }


def main() -> int:
    try:
        request_bytes = sys.stdin.buffer.read()
        payload = read_frame(request_bytes)
        response = handle_request(payload)
        encoded = encode_frame(json.dumps(response).encode("utf-8"))
        sys.stdout.buffer.write(encoded)
        sys.stdout.buffer.flush()
        return 0
    except Exception as error:
        sys.stderr.write(f"python process plugin example failed: {error}\n")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
