# Invocation compatibility boundaries

Bterm negotiates `core.service_contract.v2` before attachment. This is a wire
representation capability, not permission to execute a service. Older checked
representations are not interpreted as v2.

## Typed services

BPDL generates an identity for each interface and its reachable wire types.
Canonicalization includes the encoding/version domain and ordered fields and
variant cases. SHA-256 identities are fixed-size; invocation requests do not
carry entire schemas. Unrelated interfaces and unreachable records do not
invalidate the selected interface. Unknown or ambiguous references produce no
usable identity.

A checked request binds its interface ID and identity to its payload. The actual
provider compares that evidence against its generated interface contract before
calling service code. Routing metadata and a successful handshake are not
provider acceptance. Successful responses carry an acceptance marker which the
consumer removes before generated response decoding. Authorization remains at
the authoritative side effect.

## Commands and dynamic input

The argv bridge has one generated transport contract for protocol version 1.
Arguments remain opaque strings interpreted by the existing command executor;
there is no second per-command schema or argument parser. A checked connection
does not imply that every plugin command has a bespoke BPDL interface.

Dynamic input codecs belong to the plugin which owns their generated service
contract. Host dispatch uses neutral extension points rather than branching on
plugin identities. Surface-action payloads bind to their owning generated
interface when created. Unsupported representations fail explicitly; they do
not enable a global raw-call bypass.

Optional absence is distinct from incompatibility. A missing optional service
may retain baseline behavior; a present incompatible provider must not appear
as an absent service or silently seed default state.

## Output and retries

BMWP v5 identifies the positional presentation metadata including semantic
accessibility. Older binary headers are rejected explicitly; JSON presentation
retains its independent version.

Native output-buffer growth reuses the same invocation identity and completed
encoded outcome rather than executing the provider again. Retention is bounded
and admission backpressure occurs before execution. Conflicting identity reuse
fails explicitly. This process-local retention is not durable idempotency across
provider reload or restart; application mutations still own any stronger durable
retry guarantees. Startup claims, committed-frame input fences, and native
output
receipts remain separate ownership safeguards.
