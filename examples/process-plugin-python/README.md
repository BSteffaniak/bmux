# Python process plugin example

This example demonstrates a minimal `runtime = "process"` plugin entrypoint
implemented in Python with BMUX service-codec-compatible payload responses.

It focuses on framed stdio transport (`BMUXPRC1 + u32-be length + payload`) and
error handling flow while emitting BMUX codec-compatible response payloads.
