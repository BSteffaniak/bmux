# bmux-plugin-runtime

Host-owned, domain-agnostic runtime primitives for BMUX plugin scheduling and
concurrency.

## Shared instance / invocation scopes (source-stage integration)

`InstanceScope::new(PluginRuntimeScopeId)` mints an instance. Register the
loaded
image as an instance resource before calling `begin_activation(policy)`. An
activation starts in `Activating`; `mark_active()` enables normal invocation
creation, but does not claim readiness. `begin_invocation()` allocates a fresh
`RuntimeInvocationId`. `InvocationScope::begin_child()` gives nested operations
a
distinct identity without reacquiring an exclusive top-level invocation slot.
The runtime has no SDK dependency; adapters translate IDs losslessly with
`from_uuid` / `as_uuid`, never treating a deserialized ID as admission
authority.

Each scope supports `try_admit`, `close`, `cancelled`, `spawn`, `drain`,
`register_resource`, `release_resource`, `fail_cleanup`, `resolve_cleanup`, and
`finish`. Metadata locking is shared across the instance tree: admission checks
and increments ancestors atomically against closure. Callback capabilities own
no raw state pointers. `CallbackCapability::try_admit()` returns shared state
access protected by an `AdmissionGuard`; no runtime callback mutex is held while
plugin code runs. Callback implementations must not hold their own exclusive
locks across recursive host calls.

Close cancels the entire subtree; drain observes the current scope's tasks and
all descendant admissions. Child scopes still require explicit retirement.
Retire deepest invocations first, then the activation; run external resource
cleanup and release each owner before finishing its scope. Retire instance
resources (the loaded image last) only after child removal. Never unload merely
because `drain()` returned: live resources or child scopes still block `finish`.
Dropping a drain observer retains task handles and all owned state. Dropping all
instance observers intentionally retains an unfinished instance rather than
implicitly unloading code. Hosts must retain scope handles for recovery and
explicitly finish successfully retired instances to break that safety retention.
An explicit `resolve_cleanup()` acknowledges repaired failures only after work
and resources have settled; an ordinary retry never clears failures.

Bounds: 32 background tasks per scope, 1024 retained resources per scope, 1024
invocation nodes per instance, 1024 simultaneous direct admissions per scope,
and nesting depth 32. Full/exclusive admission returns an error rather than
blocking a callback or creating an unbounded queue. Deadline policy, readiness,
authorization, ABI handle lookup and stream/final-result protocols are adapter
responsibilities, not claims made by these lifetime primitives.

The new scope tests are authored but unexecuted during the user-directed initial
source stage. Compilation, formatting, linting and runtime validation remain
integration gates.

### Lifecycle callback ports

Trusted host wiring may call
`ActivationScope::begin_lifecycle_invocation(LifecycleInvocationKind) ->
Result<InvocationScope, ScopeError>`.
`Activation` requires Activating instance and activation; `Cleanup` requires a
Closing/CleanupRequired activation and zero admitted activation work. This mints
one explicit lifecycle root at a time, within the existing instance-node bound.
It does not change ancestor phases or reopen ordinary service admission. The
returned ordinary Rust handle is authority, not its serialized invocation ID;
never expose lifecycle-root minting as a plugin/payload operation.

Bind the lifecycle root to the host port before invoking author lifecycle code.
Keep its operation admission through author-future destruction, close callback
admission, drain, release callback resources and retire nested children, then
finish the root. Only then mark activation Active (retained lifecycle roots
block
publication) or finish the closing activation. Nested callback children inherit
only that root's phase exception, never permission to bypass a closed
invocation.
Cleanup starts with a fresh cancellation channel because the parent is already
cancelled; repeating parent close cancels and fences all current cleanup
children.
Parent failure and cancellation are not cleared by minting cleanup authority.
A new cleanup root requires an explicit host mint after the old root is
finished;
stale handles never regain authority. Dropped observers retain the child and
block
parent finish, just like ordinary invocation ownership.

## Activation-owned background work

Bundled plugins receive `HostAsyncHandle` through `activate_with_async`. The
loader
binds `spawn_background` to one activation's `BackgroundTasks` scope. It uses
the
existing host runtime, retains task outcomes, limits registrations to 32, and
rejects new work once shutdown begins. Names identify failures in diagnostics.
This in-process facility is not a serialized lifecycle field, dynamic-library
ABI,
or an addition to `HostRuntimeApi`.

Tasks observe cooperative cancellation. They must await any blocking effects
they
started before returning; cancellation is not permission to detach a service
call.
Synchronous service bridges belong on the host blocking pool, with bounded
concurrency (the catalog observer permits one effect at a time).

The host first cancels all activation scopes, then drains them while all service
providers remain alive, and only then runs synchronous deactivation in reverse
order. A
10-second host drain timeout retains ownership and returns an error; task errors
and panics also block successful shutdown. Retrying does not erase prior
failures.
Thread-local kernel guards are entered only around synchronous lifecycle calls,
never retained across a drain await.

Optional event providers are awaited through
`EventBus::subscribe_when_registered`.
Registration notifications replace startup sleeps. Consumers reconcile
authoritative
state after subscribing and after broadcast lag; channel closure prompts a fresh
subscription. Domain-specific reconciliation remains in implementation plugins.

The windows catalog observer uses this path for contexts and workspaces.
Existing
unscoped `HostAsyncHandle::spawn` APIs remain for compatibility; other existing
plugin tasks are not implicitly migrated or claimed to have scoped ownership.
