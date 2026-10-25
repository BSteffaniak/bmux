# Playbook plugin API

Generated typed BPDL contracts for the `bmux.playbook` execution service.
This crate contains no executor, process ownership, terminal acquisition or
runtime state implementation.

`playbook-commands` provides preparation, start, pause/resume, single-action
stepping, stop and fresh restart. `playbook-state` provides retained status,
revision-based snapshot polling and settlement inspection. Every operation after
preparation identifies the attempt generation; stale generations are rejected.

Preparation explicitly selects source and allowed root, native executable and
attempts root, and requires consent to executable deck input. `request_id` is a
UUID for idempotent preparation and `lookup-preparation` reconciliation in the
current owner lifetime; different fields under the same identity are rejected.
`expected_closure_sha256` must be the approved full-include-closure identity
from
`bmux_playbook::parser::source_closure_sha256`. The service verifies and freezes
the exact same bounded bytes before admission; a changed include is rejected.
An unknown lookup is not successful cleanup and must not cause automatic replay.
Host enforcement of
`bmux.playbook.execute` remains necessary: the consent field does not grant
permissions. Read operations use `bmux.playbook.read`.

An attempt snapshot carries explicit isolated configuration/runtime/data/state
paths and session identity for the normal BMUX committed-display attachment.
Lifecycle responses and diagnostics are not a terminal display feed. Stop is a
request, not successful cleanup; fresh restart requires acknowledged sandbox
cleanup and worker settlement. Pause/step operates on complete actions, never
pretends arbitrary executable side effects can be sought or rewound.

The service schema is `bpdl/playbook-plugin.bpdl`; use its generated clients
rather than handwritten request/response transport wrappers. Execution ownership
and worker protocol implementation belong to `bmux_playbook_plugin`.
