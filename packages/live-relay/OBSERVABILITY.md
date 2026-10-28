# Relay observability v1

Source interface (not a deployment or runtime validation claim):

- `GET /v1/diagnostics` requires the **independent metrics bearer key** and
  `X-Bmux-Diagnostics-Version: 1`. Missing/wrong credentials return 401;
  missing/unsupported explicit version returns 406. Responses use `no-store`.
  Publisher credentials cannot read this endpoint or `/metrics`; metrics
  credentials do not grant publisher authority. No browser CORS is enabled.
- `/health` remains the existing minimal public readiness response. Existing
  publisher/viewer WebSocket ACK, repair, status, and close envelopes are unchanged.
- Authenticated publisher start advertises `X-Bmux-Publisher-Observability: 1`.
  Updated publishers then offer `bmux-observe-v1`; only selected connections
  receive `observability_version: 1` and closed `reason` labels on repair
  messages. Legacy connections retain the original envelopes. This advertisement
  and subprotocol do not grant access to independent metrics diagnostics.
- Diagnostics carry package version, BMWP binary version, optional compile-time
  `BMUX_BUILD_REVISION` (null when not supplied), and volatile progress. Build
  revision is build-supplied metadata, not a runtime artifact attestation.
- Random connection IDs are private correlation IDs, unrelated to epoch,
  generation, publisher identity, fence, keys, origins, or remote addresses.
  They appear only in stderr structured lifecycle/repair records and authenticated
  diagnostics, never in metric labels. One repair warning per bounded reason per
  connection; successful frames do not generate INFO records.
- `bmux_relay_events_total{event,reason}` uses closed implementation-owned labels
  for ingest rejection, viewer repair requests, disconnects, and HTTP budget
  failures; absent combinations are zero. Existing aggregate counters remain.
  No payload, decoder text, key, generation, publisher ID, or fence is a label.
- Revisions are decimal strings. Ages are monotonic elapsed milliseconds, or
  null before a milestone. `idle` means no outstanding work, not fresh content;
  `sending`, `awaiting_ack`, `repair_required`, and `stalled` describe local work.
  Send/ACK deadlines are 10 seconds; a connection may be removed at its deadline
  before a poll observes `stalled`. Watch updates still coalesce, not queue.
- Publisher `received_revision` means admitted locally; `reply_revision` means
  ACK successfully written to the socket, **not** received by the publisher.
  Viewer `ack_revision` means a valid application ACK observed by the relay,
  not proof of actual browser paint. Cross-process publisher ACK timeout/browser
  paint evidence belongs to the respective workers' diagnostics. No new wire
  fields are injected into unnegotiated connections. The admitted milestone is process-global history, not a
  claim about current authority after pause/expiry/restart; connection revisions
  must not be compared across independent streams.
- Publisher clients may offer `Sec-WebSocket-Protocol: bmux-observe-v1`.
  Only when the server selects that exact subprotocol does an ingest rejection
  produce `{"type":"repair","reason":"<closed_ingest_reason>","observability_version":1}`.
  ACKs remain unchanged. Clients with no selected subprotocol receive exactly
  `{"type":"repair"}` as before. Publisher workers must check the handshake
  selection before relying on the reason; merely offering it is not negotiation.
  No publisher-side implementation is modified in this lane.
- Diagnostics retain at most 64 recent ingest-rejection/disconnect records in a
  volatile oldest-evicted ring, including only random connection ID, role, bounded
  event/reason, and monotonic age. No authority identity or content is retained.
- Exact new metric family: `bmux_relay_events_total{event,reason}`.
  Existing metric families remain `bmux_relay_frames_total`,
  `bmux_relay_repairs_total`, `bmux_relay_viewers`,
  `bmux_relay_upload_bytes_total`, `bmux_relay_delivery_bytes_total`,
  `bmux_relay_viewer_egress_throttled_total`, and
  `bmux_relay_aggregate_egress_throttled_total`.
- `Relay::ingest_detailed` provides closed `IngestReason` outcomes;
  `Relay::ingest` preserves its existing string-error API. Canonical decoder
  errors distinguish missing/mismatched delta bases, version/limit/unsupported,
  malformed data, resources, zero/stale revisions, oversize/poisoned authority.
  No parallel parser or raw decoder message is exposed. The decoder cannot
  always identify a malformed full vs delta; this is reported as `malformed`,
  never guessed. Transport failures before admission remain `transport_error`.

Fault tests added in `src/observability_tests.rs` and `src/observability.rs`
cover lost viewer ACK, stale ACK/revision, missing delta baseline, malformed
full
repair/recovery, independent auth/version selection/redaction, ingress
throttling,
idle/stalled/send-backpressure classification, and closed metric cardinality.
Existing egress-throttle tests now assert reason metrics as well. No cargo
builds
or test execution performed in this lane, per coordinator instruction.
Coordinator
must run compilation, clippy, focused relay tests and required workspace checks.

## Log backpressure

Relay records use a dedicated best-effort writer thread, a 64-record queue,
1024-byte record limit, and process-wide admission cap of eight records per
second. Producers use nonblocking admission: slow, failed, or contended sinks
drop logs rather than block runtime workers. `bmux_relay_log_dropped_total`
exposes suppression. Metrics and bounded diagnostic history remain independent
of log delivery; critical persistence failure still exits fail-closed even when
the final log is dropped. External log retention remains operator-managed.
