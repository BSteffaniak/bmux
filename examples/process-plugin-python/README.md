# Python process plugin example

This example demonstrates a minimal `runtime = "process"` plugin entrypoint
implemented in Python.

It focuses on framed stdio transport (`BMUXPRC1 + u32-be length + payload`) and
error handling flow. The payload encoding in this sample uses JSON to keep the
example dependency-free.
