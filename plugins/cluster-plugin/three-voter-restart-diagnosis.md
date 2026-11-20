# Three-voter restart failure: diagnostic record

Test: `three_admitted_voters_form_membership_and_survive_one_node_restart` in
`packages/cli/tests/cluster_peer_auth_transport_integration.rs`.

Two full-suite runs (`/tmp/bmux-nextest21` and `/tmp/bmux-nextest22`) failed
**after** enrollment, replicated three-voter reads, loss of voter C, successful
reads on the surviving nodes, and restoration of C's admitted listener at its
original endpoint. The failing assertion is the membership read on restarted C.
It did not report the transient `NotLeader` condition for which the test waits.

* In run 22, at `03:37:00.549983Z`, C logged `failed ensuring persistent
  cluster consensus runtime` followed by `failed reading cluster identity:
  service protocol error: scope is closed or no longer retained`.
* Its subsequent read returned `RuntimeUnavailable`: the local consensus node
  was not active. Run 21 records the same bootstrap error at `02:43:50.051868Z`.
* `ClusterPlugin::activate_with_async` starts a background bootstrap with a
  caller cloned from the lifecycle context. `ensure_persistent_consensus_runtime`
  calls `local_consensus_member` through that port before `start_endpoint` or
  node registration. On failure the task logs and exits. A working listener
  cannot make this failed bootstrap a voting node.
* A different failure in run 19 occurred **before restart**: a surviving
  membership read got an early EOF during an authenticated peer challenge.
  It is not evidence of a failed storage read on restart.

On Nov 20, the unchanged focused cargo test passed in 73.97 s. This is not
proof of a fixed full suite. The lifecycle-caller clone, background bootstrap,
and activation-port setup admission path are present at `3320b28b58b6`.

The remaining boundary to isolate is the host transition from activation setup
to activation-owned calls
(`packages/plugin/src/loader/scoped/activation_ports.rs`):
`CallScope::new` checks setup admission, and `BoundHostServicePort::invoke`
later
admits the actual call. The retained logs don't identify the exact throwing
admission site. A deterministic loader test should place setup retirement at
that handoff and distinguish rejection before dispatch from rejection inside
the selected dispatch, while demonstrating that real deactivation still fences
all calls. Consult the loader owner before changing this shared mechanism.

Do not retry arbitrary service calls, replace authenticated storage with a
local read, bypass peer challenge, treat an inactive node as a follower, or
extend the test deadline. None addresses the observed failure.
