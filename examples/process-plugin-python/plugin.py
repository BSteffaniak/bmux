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


PRESENTATION_KIND = "example.process.python/presentation"

# A one-row bar split off the bottom of the attach viewport, drawn by every
# attach client. The JSON is bmux_plugin::surface::PublishedPresentation; only
# the fields the bar uses are stated.
PRESENTATION = (
    '{"revision":1,'
    '"layouts":[{"id":{"owner_plugin_id":"example.process.python","local_id":"bar"},'
    '"order":0,"operation":{"Split":{"edge":"Bottom","extent":{"Cells":1}}}}],'
    '"surfaces":[{"id":{"owner_plugin_id":"example.process.python","local_id":"bar",'
    '"retained_id":"70726f63-6573-732e-7079-74686f6e0001"},"revision":1,'
    '"target":{"Layout":{"owner_plugin_id":"example.process.python","local_id":"bar"}},'
    '"opaque":true,"ops":[{"TextRun":{"x":0,"y":0,"text":" from a python process ",'
    '"style":{"reverse":true}}}]}]}'
)


def encode_bytes(value: bytes) -> bytes:
    return encode_u32_leb128(len(value)) + value


def encode_publish(kind: str, state: bool, payload: bytes) -> bytes:
    # ProcessInvocationResponse::Publish (variant 5) { protocol_version: 1,
    #   kind, state, payload }, written before the invocation's response.
    return (
        encode_u32_leb128(5)
        + encode_u32_leb128(1)
        + encode_string(kind)
        + bytes([1 if state else 0])
        + encode_bytes(payload)
    )


def encode_lifecycle_success_response() -> bytes:
    # ProcessInvocationResponse::Lifecycle { protocol_version: 1, status: 0 }.
    return bytes([0x01, 0x01, 0x00])


def request_variant_name(payload: bytes) -> str:
    """The request's variant name: requests are encoded with a tag byte, then
    the variant name as a length-prefixed string."""
    if len(payload) < 2:
        raise ValueError("truncated request")
    length, start = decode_u32_leb128(payload, 1)
    return payload[start : start + length].decode("utf-8")


def handle_request(payload: bytes) -> list[bytes]:
    """The frames answering one request: publications, then the response."""
    try:
        variant = request_variant_name(payload)
    except Exception:
        return [encode_error_response("invalid BMUX service-codec request payload", 2)]

    if variant == "Command":
        return [encode_command_success_response()]
    if variant == "Lifecycle":
        # Lifecycle (activation): publish the presentation, then succeed.
        return [
            encode_publish(PRESENTATION_KIND, True, PRESENTATION.encode("utf-8")),
            encode_lifecycle_success_response(),
        ]

    return [encode_error_response("unsupported request kind in python example", 2)]


def main() -> int:
    try:
        request_bytes = sys.stdin.buffer.read()
        payload = read_frame(request_bytes)
        for frame in handle_request(payload):
            sys.stdout.buffer.write(encode_frame(frame))
        sys.stdout.buffer.flush()
        return 0
    except Exception as error:
        sys.stderr.write(f"python process plugin example failed: {error}\n")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
