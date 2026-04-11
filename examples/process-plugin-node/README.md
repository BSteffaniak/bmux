# Node process plugin example

This example demonstrates a minimal `runtime = "process"` plugin entrypoint
implemented in Node.js with BMUX service-codec-compatible payload responses.

It reads a framed request from stdin (`BMUXPRC1 + u32-be length + payload`) and
writes a framed response to stdout using BMUX codec enum/field encoding.

Files:

- `plugin.toml` - example manifest snippet
- `plugin.js` - minimal protocol handler with BMUX codec-compatible responses
