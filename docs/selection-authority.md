# ADR: exact tab activation and effective workspace selection

Status: authority decision accepted by the repository owner; implementation pending.

## Decision

A caller has one effective context/session selection. The effective workspace is
computed from that selected context's authoritative membership. Workspace
navigation history is not another input-target authority.

Global discovery activates an exact durable context ID. Workspace-local navigation
continues to resolve names, indexes, next, and previous within the current
workspace. Existing versioned `switch-tab` semantics are unchanged.

The workspaces implementation owns cross-workspace orchestration, exposed through
an explicitly new generated BPDL interface. The finder supplies an ID and does not
orchestrate two commands. No domain types or special cases are added to core,
`HostRuntimeApi`, neutral runtime support, or plugin host infrastructure.

## Evidence and implementation constraints

Current paths do not yet satisfy this decision:

* `finder-plugin::handle_response` invokes workspace-local `switch-tab` for
  results drawn from all workspaces.
* `tabs-plugin::switch_tab` filters contexts before resolving the ID.
* `contexts-plugin::select_context_local` mutates context selection before session
  selection. Its legacy recovery can allocate a replacement session.
* `workspaces-plugin::switch_workspace_for_client` selects a context and then
  writes two independent active/previous workspace maps. A later write failure
  can follow a successful input-target change.
* `pane-runtime-plugin::attach_session` changes membership before its final
  existence checks, can remove context mappings on failure, and writes the
  clients plugin's selected target independently of context selection.
* `attach_context` and `attach_retarget_context` also mutate context selection;
  their rollback is not a conditional transaction against concurrent selection.
* The contexts plugin owns `selected_by_client`; the clients plugin's follow
  state separately owns a context/session selected target. Merely deriving the
  workspace from one of them does not reconcile their authority.

Accordingly, adding an exact-target wrapper around the existing selection service
is not sufficient. A mutex only in workspaces would not serialize local tab
navigation or attach retargeting. Holding the context state write lock across a
session-service call is unsafe: attach reads context state and its failure path
can mutate it.

## Required commit contract

Before exposing the new finder action, introduce a plugin-owned, typed selection
commit contract shared by all selection producers. Define its sole authority,
state representation, and linearization point before writing the implementation.
It must provide:

1. A caller-scoped revision and conditional mutation, enforced by the authority.
2. An exact context and execution binding; membership and execution changes are
   validated against the prepared target at commit.
3. Authorization before the authoritative side effect, not just in the finder.
4. An explicit suspended target when transition or recovery cannot safely retain
   the previous target. Neither stale work nor a fallback receives input.
5. Terminal success only after the target and required state commit agree.
6. Explicit failure/recovery results for missing context, unavailable execution,
   changed target, denied access, and uncertain completion.
7. Serialization of selection producers for one caller, without coupling the
   selections of independent callers.

Preparing a target does not publish selection. Commit does not call arbitrary
cross-plugin code while holding authoritative state locks. Effects are performed
outside those locks using preconditions/reservations that protect the target and
caller revision. Cancellation and failed effects must resolve the reservation or
leave an explicit recovery state. A response lost after commit is not modeled as
proof that nothing happened.

The selection contract must not be a new handwritten IPC envelope. Its modeled
transport belongs in BPDL, its state and coordination in implementation plugins.
Existing direct selection writers must converge on it, not remain bypasses.

## Exact activation policy

* Resolve the context ID globally from authoritative state.
* Use its current workspace membership, not the label cached in the picker.
* A missing context never selects another tab.
* An unavailable restored execution does not automatically create a replacement.
  Recovery is explicit and distinct from activation. The older repair-on-select
  interface retains its documented semantics until separately versioned.
* Resolve workspace-only navigation using valid remembered state and its
  documented fallback, then use the same selection commit path.
* Publish selection notifications only from committed selection, with caller
  identity and a revision sufficient to reject stale projections.
* The finder remains a consumer of discovery and one activation operation.

## Persistence and migration

The existing `workspaces.active_by_client` and
`workspaces.previous_by_client` keys must not simply be relabeled as history or
silently ignored. Preserve their original bytes until migration has committed.

Define a versioned navigation-history representation with a single authoritative
storage write for related history fields. Import both old maps only when that
new record is absent. Validate all imported data; malformed or unsupported data
produces an error/recovery state. Commit the new record before acknowledging
migration. On interruption, either retry from the untouched legacy records or
load the completed new record. Never merge two competing authoritative records.

Legacy active selections may seed history or initial selection only through an
explicit validated transition. A connection ID is not a durable personal-view
identity. Migration must specify which existing keys are transient and which
arrangement identity is durable; it must not recreate a prior connection's input
selection for a different attachment merely because their workspace is the same.

Once migrated, workspace queries derive effective workspace from committed
selection. Navigation-history persistence failure must not be reported as an
ordinary unsuccessful activation after silently changing the input target.
Specify whether history is part of the required commit or a separately reported,
non-authoritative effect; do not return ambiguous success.

## Implementation sequence and acceptance tests

1. Model the selection authority/commit contract and enumerate every writer
   (context selection, session attach, context attach, retarget, close fallback,
   new-context selection, workspace navigation).
2. Implement the commit mechanism, authorization, conditional effects, and
   suspended/recovery behavior. Remove alternate authoritative writers.
3. Implement and test history migration and effective-workspace projection.
4. Add the exact activation BPDL interface and workspace-owned coordinator.
5. Route finder selection through its declared contract. Unsupported peers fail
   explicitly rather than falling back to two commands or changing old semantics.
6. Add restart integration coverage independently of context repair.

Tests must cover default/non-default/third-workspace activation, same-workspace
activation, workspace-local next/previous behavior, stale picker results,
concurrent move/delete, denied access, injected failure at every effect/commit
boundary, two independent callers, competing requests for one caller, cancellation,
interrupted migration, corrupt/unknown persistence, and restart with stable IDs,
membership, and valid restored execution bindings. Verify the execution is actually
restored before selection; successful repair-on-select is not a resurrection test.

Architecture guards should prevent finder-side two-command orchestration, new core
domain leakage, and alternate selection mutation paths. Run the required
`AGENTS.md` validations for each implementation phase.
