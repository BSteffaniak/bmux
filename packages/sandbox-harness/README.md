# bmux_sandbox_harness

Reusable sandbox runtime harness for bmux examples and tests.

This crate starts an isolated in-process bmux server with dedicated
`BMUX_CONFIG_DIR`, `BMUX_RUNTIME_DIR`, `BMUX_DATA_DIR`, and `BMUX_STATE_DIR`
paths so callers can run demos without touching their normal local runtime.
