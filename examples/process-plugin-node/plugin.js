#!/usr/bin/env node
const MAGIC = Buffer.from("BMUXPRC1", "ascii");

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

function handleRequest(decodedPayload) {
  let request;
  try {
    request = JSON.parse(decodedPayload.toString("utf8"));
  } catch {
    return { Error: { protocol_version: 1, details: "invalid request payload", status: 2 } };
  }

  if (request.Command) {
    return {
      Command: {
        protocol_version: 1,
        status: 0,
        outcome: { effects: [] },
      },
    };
  }

  return {
    Error: {
      protocol_version: 1,
      details: "unsupported request kind in node example",
      status: 2,
    },
  };
}

const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk));
process.stdin.on("end", () => {
  try {
    const requestFrame = Buffer.concat(chunks);
    const payload = decodeFrame(requestFrame);
    const response = handleRequest(payload);
    const responsePayload = Buffer.from(JSON.stringify(response), "utf8");
    process.stdout.write(encodeFrame(responsePayload));
  } catch (error) {
    process.stderr.write(`node process plugin example failed: ${error.message}\n`);
    process.exitCode = 2;
  }
});
