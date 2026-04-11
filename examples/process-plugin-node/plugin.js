#!/usr/bin/env node
const MAGIC = Buffer.from("BMUXPRC1", "ascii");

function encodeU32Leb128(value) {
  const out = [];
  let n = value >>> 0;
  do {
    let byte = n & 0x7f;
    n >>>= 7;
    if (n !== 0) {
      byte |= 0x80;
    }
    out.push(byte);
  } while (n !== 0);
  return Buffer.from(out);
}

function encodeI32ZigZag(value) {
  const zigzag = ((value << 1) ^ (value >> 31)) >>> 0;
  return encodeU32Leb128(zigzag);
}

function decodeU32Leb128(buffer, offset = 0) {
  let result = 0;
  let shift = 0;
  let index = offset;
  while (index < buffer.length) {
    const byte = buffer[index];
    result |= (byte & 0x7f) << shift;
    index += 1;
    if ((byte & 0x80) === 0) {
      return { value: result >>> 0, nextOffset: index };
    }
    shift += 7;
    if (shift > 28) {
      throw new Error("varint too large");
    }
  }
  throw new Error("truncated varint");
}

function encodeString(value) {
  const bytes = Buffer.from(value, "utf8");
  return Buffer.concat([encodeU32Leb128(bytes.length), bytes]);
}

function encodeFrame(payloadBuffer) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(payloadBuffer.length, 0);
  return Buffer.concat([MAGIC, len, payloadBuffer]);
}

function decodeFrame(buffer) {
  if (buffer.length < MAGIC.length + 4) {
    throw new Error("truncated frame header");
  }
  if (!buffer.subarray(0, MAGIC.length).equals(MAGIC)) {
    throw new Error("missing BMUXPRC1 prefix");
  }
  const len = buffer.readUInt32BE(MAGIC.length);
  const start = MAGIC.length + 4;
  const end = start + len;
  if (buffer.length !== end) {
    throw new Error("invalid frame length");
  }
  return buffer.subarray(start, end);
}

function encodeCommandSuccessResponse() {
  // ProcessInvocationResponse::Command {
  //   protocol_version: 1,
  //   status: 0,
  //   outcome: Some(PluginCommandOutcome { effects: [] }),
  // }
  return Buffer.from([0x00, 0x01, 0x00, 0x01, 0x00]);
}

function encodeErrorResponse(details, statusCode) {
  // ProcessInvocationResponse::Error variant index = 4.
  return Buffer.concat([
    encodeU32Leb128(4),
    encodeU32Leb128(1),
    encodeString(details),
    Buffer.from([0x01]),
    encodeI32ZigZag(statusCode),
  ]);
}

function handleRequest(decodedPayload) {
  let requestVariant;
  try {
    requestVariant = decodeU32Leb128(decodedPayload, 0).value;
  } catch {
    return encodeErrorResponse("invalid BMUX service-codec request payload", 2);
  }

  if (requestVariant === 0) {
    return encodeCommandSuccessResponse();
  }
  return encodeErrorResponse("unsupported request kind in node example", 2);
}

const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk));
process.stdin.on("end", () => {
  try {
    const requestFrame = Buffer.concat(chunks);
    const payload = decodeFrame(requestFrame);
    const responsePayload = handleRequest(payload);
    process.stdout.write(encodeFrame(responsePayload));
  } catch (error) {
    process.stderr.write(`node process plugin example failed: ${error.message}\n`);
    process.exitCode = 2;
  }
});
