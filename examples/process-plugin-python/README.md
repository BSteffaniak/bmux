# Python process plugin example

This example demonstrates a minimal `runtime = "process"` plugin entrypoint
implemented in Python with BMUX service-codec-compatible payload responses.

It focuses on framed stdio transport (`BMUXPRC1 + u32-be length + payload`) and
error handling flow while emitting BMUX codec-compatible response payloads.

When activated it also draws attach UI: before answering the lifecycle request
it writes a `Publish` frame (variant 5: protocol version, kind, state flag, JSON
payload) carrying its presentation, a one-row bar, on
`example.process.python/presentation`, which `plugin.toml` declares as a
forwarded state event. See "Presentations from plugins outside the attach
client" in `docs/plugins.md`.

Requests are encoded with a tag byte followed by the variant name as a
length-prefixed string; the example dispatches on that name.
