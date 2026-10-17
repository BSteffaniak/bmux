# Retired JavaScript live relay

The Cloudflare Worker, local Node authority harness, Node dependency manifests,
and Worker-only test suite have been retired. There is no runnable relay here.

The sole implementation is the Rust executable in
[`packages/live-relay`](../../packages/live-relay). Its socket and durability
checks live with that crate. The wire/control contract is documented in
[`docs/live-stream-protocol.md`](../../docs/live-stream-protocol.md); deployment
and local Rust/WSS validation are documented in
[`docs/live-deployment.md`](../../docs/live-deployment.md).

Removing these sources does **not** authorize infrastructure deletion or a live
apply. Existing Worker infrastructure retirement remains subject to the reviewed
infrastructure plan/apply gate. Do not restore polling or the former local Node
harness as an alternate channel authority.
