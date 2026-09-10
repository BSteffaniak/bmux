# bmux-plugin-runtime

Host-owned, domain-agnostic runtime primitives for BMUX plugin scheduling and concurrency.

## Activation-owned background work

Bundled plugins receive `HostAsyncHandle` through `activate_with_async`. The loader
binds `spawn_background` to one activation's `BackgroundTasks` scope. It uses the
existing host runtime, retains task outcomes, limits registrations to 32, and
rejects new work once shutdown begins. Names identify failures in diagnostics.
This in-process facility is not a serialized lifecycle field, dynamic-library ABI,
or an addition to `HostRuntimeApi`.

Tasks observe cooperative cancellation. They must await any blocking effects they
started before returning; cancellation is not permission to detach a service call.
Synchronous service bridges belong on the host blocking pool, with bounded
concurrency (the catalog observer permits one effect at a time).

The host first cancels all activation scopes, then drains them while all service
providers remain alive, and only then runs synchronous deactivation in reverse order. A
10-second host drain timeout retains ownership and returns an error; task errors
and panics also block successful shutdown. Retrying does not erase prior failures.
Thread-local kernel guards are entered only around synchronous lifecycle calls,
never retained across a drain await.

Optional event providers are awaited through `EventBus::subscribe_when_registered`.
Registration notifications replace startup sleeps. Consumers reconcile authoritative
state after subscribing and after broadcast lag; channel closure prompts a fresh
subscription. Domain-specific reconciliation remains in implementation plugins.

The windows catalog observer uses this path for contexts and workspaces. Existing
unscoped `HostAsyncHandle::spawn` APIs remain for compatibility; other existing
plugin tasks are not implicitly migrated or claimed to have scoped ownership.
