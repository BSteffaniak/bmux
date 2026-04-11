#!/usr/bin/env python3
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


def encode_u32_leb128(value: int) -> bytes:
    out = bytearray()
    current = value & 0xFFFFFFFF
    while True:
        byte = current & 0x7F
        current >>= 7
        if current:
            byte |= 0x80
        out.append(byte)
        if not current:
            break
    return bytes(out)


def encode_i32_zigzag(value: int) -> bytes:
    zigzag = ((value << 1) ^ (value >> 31)) & 0xFFFFFFFF
    return encode_u32_leb128(zigzag)


def decode_u32_leb128(payload: bytes, offset: int = 0) -> tuple[int, int]:
    result = 0
    shift = 0
    index = offset
    while index < len(payload):
        byte = payload[index]
        result |= (byte & 0x7F) << shift
        index += 1
        if byte & 0x80 == 0:
            return result, index
        shift += 7
        if shift > 28:
            raise ValueError("varint too large")
    raise ValueError("truncated varint")


def encode_string(value: str) -> bytes:
    payload = value.encode("utf-8")
    return encode_u32_leb128(len(payload)) + payload


def encode_command_success_response() -> bytes:
    # ProcessInvocationResponse::Command {
    #   protocol_version: 1,
    #   status: 0,
    #   outcome: Some(PluginCommandOutcome { effects: [] }),
    # }
    return bytes([0x00, 0x01, 0x00, 0x01, 0x00])


def encode_error_response(details: str, status_code: int) -> bytes:
    # ProcessInvocationResponse::Error variant index = 4.
    return (
        encode_u32_leb128(4)
        + encode_u32_leb128(1)
        + encode_string(details)
        + bytes([0x01])
        + encode_i32_zigzag(status_code)
    )


def handle_request(payload: bytes) -> bytes:
    try:
        request_variant, _ = decode_u32_leb128(payload, 0)
    except Exception:
        return encode_error_response("invalid BMUX service-codec request payload", 2)

    if request_variant == 0:
        return encode_command_success_response()

    return encode_error_response("unsupported request kind in python example", 2)


def main() -> int:
    try:
        request_bytes = sys.stdin.buffer.read()
        payload = read_frame(request_bytes)
        encoded = encode_frame(handle_request(payload))
        sys.stdout.buffer.write(encoded)
        sys.stdout.buffer.flush()
        return 0
    except Exception as error:
        sys.stderr.write(f"python process plugin example failed: {error}\n")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
