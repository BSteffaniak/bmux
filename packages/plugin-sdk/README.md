# bmux Plugin SDK

## Unified scoped author API (source-stage migration)

Implement one `PluginInstance` and provide a factory. Definitions are inert:
reading metadata does not initialize an instance, activate it, or register work.
The implementation library is an ordinary export-free Rust library:

```rust,ignore
use std::sync::Arc;
use bmux_plugin_sdk::{CommandOutcome, ComponentRole, NativeCommandContext,
    PluginCallContext, PluginCreateContext, PluginFuture, PluginInstance,
    PluginMetadata, PluginResult};

struct Hello;
impl PluginInstance for Hello {
    fn command(&self, call: PluginCallContext, context: NativeCommandContext)
        -> PluginFuture<'_, CommandOutcome>
    {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return CommandOutcome { exit_code: 1, error: Some("cancelled".into()) };
            }
            CommandOutcome { exit_code: 0, error: None }
        })
    }
}

bmux_plugin_sdk::define_plugin!(
    factory = |_scope: PluginCreateContext| -> PluginResult<Arc<dyn PluginInstance>> {
        Ok(Arc::new(Hello))
    },
    metadata = PluginMetadata {
        plugin_id: "example.hello".into(), version: "0.1.0".into(),
        roles: vec![ComponentRole::Backend], required_capabilities: vec![],
        contracts: vec![], manifest_toml: None,
    },
);
```

`define_plugin!` generates only `pub fn plugin_definition() ->
PluginDefinition`.
A static host calls `plugin_definition().create(host_context)` directly and
registers generated BPDL adapters through
`instance.register_typed_services(&mut activation_registry)`. The generated
service traits remain the typed fast path: static calls do not serialize.
Factories must create fresh scoped state without starting activation effects.
Shared factory configuration is fine; accidental singleton product state is not.

A **separate thin packaging cdylib** explicitly exports the descriptor:

```rust,ignore
bmux_plugin_sdk::export_plugin_entrypoint!(my_implementation::plugin_definition);
```

Do not invoke that macro in implementation crates. Packaging target generation
and installation remain host tooling responsibilities. No build mode feature
switch selects different behavior in the implementation.

### Exact author interfaces

- `PluginFactory::create(&self, PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>>`.
- `PluginDefinition::new(PluginMetadata, impl PluginFactory) -> PluginDefinition`;
  `create(&self, PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>>`.
- `PluginCreateContext { runtime_scope: RuntimeScopeId, instance: LoadedInstanceId, role: ComponentRole }`.
  Identity names alias the runtime's `PluginRuntimeScopeId`, `PluginInstanceId`,
  and `PluginActivationId` (as `ActivationId`), not a second identity model.
- `PluginLifecycleContext { scope: PluginCreateContext, activation: ActivationId, native: NativeLifecycleContext }`.
- `PluginCallContext { invocation_id: PluginInvocationId, cancellation: ServiceCancellation }`:
  the cancellation handle is live Rust state, not a serialized snapshot.
- `activate/deactivate(&self, PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>>`.
- `command(&self, PluginCallContext, NativeCommandContext) -> PluginFuture<'_, CommandOutcome>`.
- `invoke(&self, PluginCallContext, NativeServiceContext) -> PluginFuture<'_, PluginResult<ServiceResponse>>`.
- `invoke_streaming(&self, PluginCallContext, NativeStreamingServiceContext) -> PluginFuture<'_, PluginResult<ServiceResponse>>`.
- `event(&self, PluginEvent) -> PluginFuture<'_, PluginResult<()>>`.
- `extension(&self, PluginCallContext, String, Vec<u8>) -> PluginFuture<'_, PluginResult<Vec<u8>>>`.
- `register_typed_services(&self, &mut TypedServiceRegistry) -> PluginResult<()>`.

`PluginFuture` is a boxed `Send` Rust future; no future, trait object or runtime
handle crosses the ABI. Methods use `&self` without a host-held exclusive
implementation mutex. Generated typed adapters and portable dispatch must refer
to the same scoped provider implementation. The host owns admission, readiness,
registration publication/revocation, resource draining and dependency rollback.
An instance becoming unobserved never authorizes destroying detached work.

### Explicit native v2/v3 transport

`bmux_plugin_entry_v2() -> *const abi::NativePluginDescriptor` remains the
discovery
symbol. Current descriptors advertise version 3 through
`NativePluginDescriptorV3 { base: NativePluginDescriptor, declarations }`.
The v2 base layout is unchanged; the v3 header size covers the full extension.
`validate_descriptor_v3` validates the version/size before extension access.
`declarations(*mut u8, usize, *mut usize) -> AbiStatus` copies bounded JSON
`PluginDeclarations` without creating or activating an instance. Its
`modeled_service_contracts: Vec<DeclaredServiceContract>` contains validated
owned `capability`, `kind`, exact `interface_id`, and `contract` evidence bytes;
legacy aliases remain separately in `services`. Generated
`PluginContract::service_contract_descriptors()` is the source of modeled
identity, never alias-name inference. The hook defaults to an empty slice.
Check descriptor `size`, `version`, and capabilities before using later fields.
Version 2 accepts only `SCOPED_CAPABILITIES_V2 = 15` (events, services, live
cancellation, activation registrations); version 3 additionally supports
`CAP_KERNEL_REQUESTS = 16`, `CAP_RETAINED_LIFECYCLE = 32`, and
`CAP_ASYNC_ACTIVATION = 64`, yielding the frozen
`SCOPED_CAPABILITIES_V3_LEGACY = 127`. Selected-service availability-v1 adds
`CAP_SELECTED_SERVICE_AVAILABILITY_V1 = 128` (`SCOPED_CAPABILITIES = 255`).
Hosts select
these capabilities explicitly on the v3 invocation table after checking the
artifact descriptor. Unknown versions or bits reject explicitly. There is no
v1/v2 fallback for kernel or retained lifecycle authority.

Availability uses `NativeInvocationPortsV3Availability { base:
NativeInvocationPortsV3, availability_begin }`. Pass `&base.base`, version 3 and
the full extension size; the new bit and sufficient size are both mandatory.
The callback signature is `unsafe extern "C" fn(u64, u16, *const u8, usize,
*mut u64) -> AbiStatus`: bound opaque handle, contract version **1**, canonical
typed-stable encoded validated `ServiceAvailabilityKey`, byte length, child out.
Successful retained children encode `SelectedServiceAvailability` with the same
codec, retrieved/retired by existing read/ack without repeating lookup. Buffers
and admission use existing bounds. No positional or JSON decode fallback exists
for this new contract. Missing negotiation is `UnsupportedHostOperation`, never
`Absent`; unknown capability bits reject without fallback. Old v3 peers reject
the new bit; existing base tables/layouts/semantics remain unchanged. Hosts must
check actual admitted caller access and the authoritative selected registry,
not manifests. Presence is not authorization for a later call.

Recovery-v2 is separately negotiated with `CAP_RETAINED_HOST_LOOKUP_V2 = 512`.
The frozen v1 mask is `SCOPED_CAPABILITIES_V3_RETAINED_V1 = 511`; current
supported
mask is 1023. `NativeInvocationPortsV3RetainedHostV2` preserves every prior
prefix
and appends `retained_host_open_v2(owner:u64, version:u16,
*mut NativeRetainedHostPortV2) -> AbiStatus`. Select version 2 and full
extension
size before access. Open transfers one independent reference on success, none on
failure. Never downgrade/retry another open after a selected v2 failure.

`NativeRetainedHostPortV2 { base: NativeRetainedHostPortV1, lookup }` has base
version 2 and exactly the v2 size. Its new callback is
`lookup(owner:u64, request_id:u64, *const NativeRetainedRequestV1,
*const NativeRetainedReservationV1, *mut u64) -> AbiStatus`. On 64-bit targets,
tables v1/v2 are 64/72 bytes and the invocation v2 tail is 112 bytes; use target
`size_of` and prefix layout assertions. Old v1 allocations/semantics are frozen
and cannot claim recovery support.

`RetainedHostDispatch::lookup` defaults to `UnsupportedHostOperation`.
`RetainedHostPort::lookup` shares validation and returns an existing owner-local
`RetainedHostOperation` with the original validated bounds, or None for
NOT_FOUND.
NOT_FOUND never permits replay of begin. The actual host ledger compares all
original routing/payload facts and reservation fields, rejects conflicts, and
performs no producer admission, capacity reservation, effect or consumption.
Post-close lookup remains allowed. Ack tombstones retain mapping without
resurrecting consumed outcomes; pending/terminal receipts are unchanged. Product
UUID mappings remain in the product owner ledger, never a second SDK registry.

The independent retained-host transport adds `CAP_RETAINED_ASYNC_HOST_V1 = 256`
(`SCOPED_CAPABILITIES_V3_AVAILABILITY = 255` remains frozen; frozen v1 mask is
511).
`NativeInvocationPortsV3RetainedHost` preserves the availability prefix and adds
`retained_host_open(owner, version: u16, *mut NativeRetainedHostPortV1)`.
Version
1 writes exactly the v1 output allocation and transfers one owned reference on
success, none on failure. It opens only an actual bound capability. Unknown
bits,
short tables, and unsupported versions reject without fallback.

`abi::retained` owns the neutral fixed-width request/reservation/table types and
safe `RetainedHostPort` / `RetainedHostOperation` adapters. The reservation is
`{size:u32, version:u32, max_outcome_bytes:u64, operation_slots:u32,
reserved:u32}`
with reserved zero; v1 begin
reserves one operation slot and its entire terminal record before effects.
Lengths/capacities are u32; owner/request/operation/receipt identities are u64.
Begin retries use the same owner and request identity, never a new identity
after
an uncertain reply. The host owns deduplication/tombstones, not the SDK adapter.

Retained-table begin/control/read callbacks are bounded nonblocking admissions
and observations of asynchronously executing producers, not synchronous service
execution. Read is non-consuming and may return PENDING; explicit controls are
CANCEL=1, CLOSE=2, JOIN=3. Operation zero addresses only that port's owner.
Close
preserves observation/join/ack. Join requires actual producer/callback
completion.
Release BUSY consumes nothing; the host registry keeps execution/image/cleanup
pins through observer loss or uncertain cleanup. There is no invocation-port
resurrection or new HostRuntimeApi operation.

Terminal records are little-endian `{version:u32, outcome:u32,
observed_bytes:u64}`
followed only by success payload. Kinds are COMPLETED=1 (opaque bytes),
REJECTED=2, EFFECTS_UNRESOLVED=3, RESPONSE_LIMIT=4, CLOSED=5. Failure kinds have
zero payload; uncertainty kinds retain observed byte counts: no unbounded error
strings. CLOSED means rejection before effects,
not cancellation or disappearance of admitted work. Response overflow retains
an uncertainty record; the consumer never truncates it into success or retries.
Receipt belongs to the exact operation and is validated on explicit ack. Drop
never acknowledges/cancels; a failed final release leaves authoritative cleanup
with the host. Static hosts implement `RetainedHostDispatch` over that same
owner
ledger; product budgets and product identity models remain outside this module.

A retained lifecycle table belongs to the activation, not its completed setup
invocation. During setup it uses the setup scope; after activation settles each
service call mints fresh activation-owned admission. Closing the activation
rejects new calls. Child outcomes retain their admission until acknowledgement,
including after cancellation. The native adapter retains the table through
activation acknowledgement and closes/drains/releases it during cleanup; the
host retires the handle only after that release. Ordinary invocation tables
continue to close at terminal settlement.

`PluginInstance::activate_scoped_async` is the standard entry for an owned async
activation, preserving the supplied host services and calling the same author
`activate_with_async` implementation. `HostAsyncHandle` is the activation's
task scope, not a runtime handle: its spawns go through plug-rt and are torn
down with the activation. Native adapters own no runtime; activation work runs
on the host's runtime and cooperative tasks drain before cleanup.

The host services `activate` receives (`context.host_service_port`) are
activation-scoped: background work may keep them and call through them from
the moment it receives them until deactivation begins, from any thread, and
then they fail closed. A call that starts while the author is still inside
`activate` keeps that entry's ancestry for the whole call, even if `activate`
returns meanwhile; a call that starts afterwards runs on a fresh activation
invocation. Plugins should still prefer `rt::spawn` for such work so the
activation scope owns and tears it down.

### plug-rt: one runtime per host process

Plugins never build or reach a Tokio runtime. They spawn, run blocking work and
use timers through `bmux_plugin_sdk::rt` (`spawn`, `spawn_named`,
`spawn_blocking`, `JoinHandle`, `time::{sleep, timeout, interval, Instant}`).
Runtime-agnostic primitives (`tokio::sync`, `select!`, `join!`) are used
directly. Author source is identical in both packaging modes:

- Bundled builds re-export Tokio. Work spawned while the host polls plugin code
  joins the activation's task scope.
- Dynamic builds (`--cfg bmux_plugin_dylib`, set by `scripts/package-plugin.py`
  in the wrapper's own target directory, never a Cargo feature) route every
  call through the `PlugRtV1` table the host installs through the wrapper's
  `bmux_plug_rt_install_v1` export before creating instances. Plugin futures
  cross as `{data, poll, drop}` with real host wakers; libraries are never
  unloaded once installed.

Teardown stops admitting work and cancels, drains cooperative tasks until the
deadline, aborts what remains (dropping each future through its owner), waits
for running blocking jobs, and reports stragglers by name.

Net and process IO are not proxied into dynamic libraries yet. A plugin that
needs them, or still calls Tokio's runtime APIs directly, declares
`bundled_only = "<reason>"` in `plugin.toml`; the dynamic loader refuses it with
`PluginError::BundledOnlyPlugin`. A library not built in dynamic mode is refused
with `PluginError::DynamicRuntimeUnavailable`.

`NativeInvocationPorts` version 2 remains frozen. For a selected kernel grant,
the host supplies `NativeInvocationPortsV3 { base, kernel_begin }` through
`&base`, with `base.version = PORTS_VERSION_V3` and `base.size` equal to the
complete v3 table size. The extension callback has C signature
`kernel_begin(u64, *const u8, usize, *mut u64) -> AbiStatus`. It accepts opaque
`bmux_ipc`-encoded `Request` bytes and returns a retained terminal child whose
`ResponsePayload` bytes are retrieved/acknowledged through base `host_read` /
`host_ack`. Begin runs effects once; read/buffer growth never repeats them.
The host must run synchronous plugin execution on a blocking worker.

The one author API is `PluginHostServices::with_kernel` /
`execute_kernel_encoded`. The optional callback is safe, invocation-bound,
allocation-identity compared, and absent by default; absence returns
`UnsupportedHostOperation`. Both buffers are limited to `MAX_BUFFER_BYTES`.
The host enforces foundational-plugin authorization, deadlines and live
cancellation at the authoritative effect. Native cancellation closes kernel
admission; completion closes and drains callbacks before release. Captured
clones fail closed afterward. Serialized pointers and `HostKernelBridge` are
not used to supply this authority. Unscoped streaming is rejected before
effects.

The descriptor exposes `metadata`, `create`, `destroy`, `start`, `read`,
`cancel`, and `ack`. Inputs are bounded JSON, borrowed for that call and copied
before return. `NativeOperation` and `NativeOutcome` are dynamic adapter data,
not a second author service implementation. Operation IDs are nonzero,
strictly increasing host-issued integers within each opaque instance handle.
Reusing an ID is rejected, including after acknowledgement. This is transport
replay protection, not domain durable idempotency.

`start` admits but does not execute author work. `read` polls it and returns
`PENDING` or a retained final outcome. There are no wake callbacks: the host
must poll periodically, including after cooperative cancellation. A final
`BUFFER_TOO_SMALL` reports the required length; resize and call **read**, not
start. Repeated reads return the same owned outcome. `ack` disposes only final
outcomes. Each instance retains at most 128 operations and each encoded outcome
is bounded to 1 MiB (also the input/output buffer limit); saturation fails
explicitly. Outcomes are not expired silently. There are at most 256 retained
instances. Structured `CommandOutcome { exit_code, error }` crosses this
boundary
without shared thread-local state.

All output allocations belong to the host; the plugin only copies bytes into
caller-provided buffers. Nothing allocated by one side is freed by the other.
Pointers must be valid, correctly aligned where typed, and nonoverlapping until
return. Null byte pointers are accepted only with zero capacity. Only trusted
same-process host contexts may carry existing native bridge fields; never
accept executable pointers from remote/process transports or plugin payloads.

Polling and author callbacks run outside bookkeeping locks. Concurrent polling
of the same operation returns `BUSY`. Activation is required for ordinary
operations; deactivation requires all prior operations acknowledged. Destroy
requires an inactive, drained instance and no concurrently admitted ABI calls.
Failed lifecycle calls retain cleanup-required state for explicit deactivation.
Unwinding author panics are contained and permanently retain the uncertain
instance/library for recovery rather than unloading code; aborts and native
memory faults cannot be contained. Library ownership itself must be retained by
the loader until destruction/recovery is proven safe.

This source stage is uncompiled and unexecuted by user instruction. Full
repository validation and host-side callback table integration are deferred.
The same-process native callback implementation is source, not verified parity.

### Converged declaration hooks

`PluginDefinition::declarations()` decodes the embedded declaration into
`PluginDeclarations { services, commands, event_subscriptions,
event_publications, ready_signals, extensions }`. Corrupt declarations fail
before factory creation. `InstanceRegistrations { contributions, contracts }`
is collected through `PluginInstance::registrations()`; contracts are validated
and duplicates rejected. Native v2 retrieves this through
`NativeOperation::Registrations` / `NativeOutcome::Registrations`, with normal
retained read/ack semantics. Hosts publish only after activation policy admits
it.

Additional instance entry points:

- `activate_with_async(&self, PluginLifecycleContext, HostAsyncHandle) -> PluginFuture<'_, PluginResult<()>>`.
- `register_typed_services_with_context(&self, TypedServiceRegistrationContext<'_>, &mut TypedServiceRegistry) -> PluginResult<()>`.
- `register_contributions(&self, &mut ContributionRegistrar) -> PluginResult<()>`.
- `registration_contracts(&self) -> PluginResult<Vec<RegistrationContract>>`.
- `registrations(&self) -> PluginResult<InstanceRegistrations>`.
- `portable_services(&self) -> PluginResult<Vec<Arc<dyn PortableServiceProvider>>>`.
- `service_contract_for(&self, &str) -> Option<&'static [u8]>`.
- `stateful_participants(&self) -> PluginResult<Vec<StatefulPluginHandle>>`.

Context-aware registration delegates to the simple hook by default. Portable
and typed adapters must share actual provider objects. Stateful native handles
optimize the same typed BPDL contracts; they are not a portable pointer
protocol.
Readiness contracts remain activation-owned; global `ReadyTracker` is not a
substitute for owner-scoped publication.

For migration, an existing trait implementation can declare:

```rust,ignore
bmux_plugin_sdk::define_plugin!(
    rust_plugin = ExistingImplementation,
    manifest = include_str!("../plugin.toml"),
    roles = vec![bmux_plugin_sdk::ComponentRole::Backend],
);
```

This delegates **all existing author hooks** through `RustPluginAdapter<P>` to
one freshly created object, including async activation and host-context typed
registration. Generated services are merged into the single embedded manifest.
The direct factory form remains preferable for fully migrated implementations.
The bridge rejects busy mutable callbacks instead of deadlocking on reentrant
calls, and never uses process-global legacy instance cells. It requires `Sync`;
implementations that cannot satisfy this must migrate their state explicitly.
New optional legacy hooks for `portable_services`, `registration_contracts` and
`stateful_participants` delegate through the same instance. Remove old native
export macro calls when migrating: declaration does not require legacy exports.

The host must call the scoped/async activation and context-aware registration
hooks. Scoped native ports carry authority via retain/release callback handles,
never ambient globals. Async scheduling is negotiated separately through
plug-rt (see above); event/host-service/live-cancellation capability bits do
not claim to transport a Rust executor.

### Scoped callback ABI (source-stage v2 extension)

The descriptor now includes `start_scoped(instance:u64, operation:u64,
input:*const u8, len:usize, ports:*const NativeInvocationPorts) -> i32`.
`NativeInvocationPorts` has `size:u32, version:u32, capabilities:u64,
handle:u64`
and these `unsafe extern "C"` function pointers:

- `retain/release(handle:u64) -> i32`;
- `emit_event(handle:u64, data:*const u8, len:usize) -> i32`;
- `host_begin(handle:u64, request:*const NativeHostRequest, child:*mut u64) -> i32`;
- `host_read(handle:u64, child:u64, out:*mut u8, cap:usize, len:*mut usize) -> i32`;
- `host_ack(handle:u64, child:u64) -> i32`;
- `registration_begin(handle:u64, data:*const u8, len:usize, child:*mut u64) -> i32`.

Registration capability bit 8 selects M6 activation registrations. The bounded
payload is `RegistrationMutation::{Register, SetRetained, Emit, MarkReady}` with
contract, optional expected revision, and inert value bytes only. Results use
the same retained host_read/host_ack protocol. The host binds an
`ActivationRegistrationPort` before the hook, applies mutations through that
port, marks activation active before publishing, then withdraws and drains child
invocations/`withdraw_and_drain` before finishing. None of
publish/withdraw/finish
is plugin-callable through this callback. Static wiring adds the same adapter
using `PluginHostServices::with_registrations`; no HostRuntimeApi operations
are added. Instance/activation authority is never supplied by mutation payloads.

`NativeHostRequest` is `size:u32, kind:u32` then `capability`, `interface`,
`operation`, `payload` as `NativeSlice { pointer:*const u8, length:usize }`.
Kinds are query=0, command=1, event=2. No callback or Rust handle is JSON.
Legacy executable bridge fields are rejected *before* context deserialization.
`PluginCallContext.host_services: Option<PluginHostServices>` is a safe author
port; static wiring uses `PluginHostServices::from_bound(BoundHostServicePort)`.
Generated clients can use its `TypedDispatchClient` implementation for unary
nested services. Lifecycle uses `activate_scoped/deactivate_scoped(context,
Option<PluginHostServices>)`; their defaults delegate to ordinary hooks.

Capability bits are events=1, host services=2, live cancellation=4, activation
registrations=8. Tables are
copied after size/version checks and retained once; successful release occurs
only after admission closes and all admitted callbacks return. At most 32
callbacks are admitted concurrently. Events are ordered or explicitly rejected
on concurrent/reentrant emission; consumer errors are remembered even if author
code ignores them. Captured clones fail closed after final and cannot use a
released handle. Host release callbacks are outside SDK locks. A final outcome
is held in `Draining` until callbacks close/drain; acknowledgement cannot drop
pending/draining operations. Hosts poll, so no notification wake can be lost.

`host_begin` is synchronous and returns an already-terminal retained child,
matching M6's synchronous bound port. It binds caller/deadline/parent authority
and creates a distinct child identity host-side. `host_read` retrieves bounded
bytes without effects; one begin followed by size/read/ack avoids mutation
reexecution. Nested streaming host requests are not advertised by this unary
callback. Incoming plugin streaming uses `NativeOperation::Streaming` and an
invocation-ID checked event sink. `cancel` concurrently flips the same live
`ServiceCancellation` the provider sees; cancellation alone never drops work.

All callbacks must contain their own panics, be thread-safe/reentrant and keep
borrowed buffers valid until return. The loader retains executable code while
callbacks or failed cleanup require it. Native faults/abort remain uncontained.
Full host-side wiring and source validation are pending; capability bits
describe
implemented adapter mechanisms, not evidence of tested production parity.

## Legacy SDK migration reference

Everything you need to write a bmux plugin.

## Quick Start

A bmux plugin is a Rust crate with three files: a manifest, a library, and a
`Cargo.toml`.

### 1. `plugin.toml`

```toml
id      = "example.hello"
name    = "Hello Plugin"
version = "0.1.0"

[[commands]]
name          = "hello"
summary       = "Print a greeting"
expose_in_cli = true
```

### 2. `src/lib.rs`

```rust
use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct HelloPlugin;

impl RustPlugin for HelloPlugin {
    fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(ctx, {
            "hello" => {
                let name = ctx.arguments.first().map_or("world", String::as_str);
                println!("Hello, {name}!");
                Ok(EXIT_OK)
            },
        })
    }
}

bmux_plugin_sdk::define_plugin!(
    rust_plugin = HelloPlugin,
    manifest = include_str!("../plugin.toml"),
    roles = vec![bmux_plugin_sdk::ComponentRole::Backend],
);
```

### 3. `Cargo.toml`

```toml
[package]
name    = "my_plugin"
edition = "2024"
version = "0.1.0"

[dependencies]
bmux_plugin_sdk = { version = "..." }
```

The implementation is a plain export-free `rlib` with no static or dynamic
features. Hosts bundle it by calling `plugin_definition()`; dynamic artifacts
come only from the thin wrapper `scripts/package-plugin.py` generates.

## The `RustPlugin` Trait

Every plugin implements `RustPlugin`. All five methods have default
implementations -- override only what your plugin needs:

| Method           | Return type                       | When to override                          |
| ---------------- | --------------------------------- | ----------------------------------------- |
| `run_command`    | `Result<i32, PluginCommandError>` | Plugin provides CLI commands              |
| `invoke_service` | `ServiceResponse`                 | Plugin provides services to other plugins |
| `activate`       | `Result<i32, PluginCommandError>` | Plugin needs setup on activation          |
| `deactivate`     | `Result<i32, PluginCommandError>` | Plugin needs cleanup on deactivation      |
| `handle_event`   | `Result<i32, PluginCommandError>` | Plugin subscribes to system events        |

The trait requires `Default + Send + 'static`. Use `#[derive(Default)]` on your
struct.

## Retained attach presentations

Attach presentations use the domain-neutral retained layout and surface APIs in
`bmux_plugin`; the host does not call a plugin while painting a frame.

```rust,ignore
use bmux_plugin::layout::{
    LayoutEdge, LayoutExtent, PluginLayoutId, PluginLayoutRequest,
    PluginLayoutSnapshot, global_plugin_layout_registry,
};
use bmux_plugin::surface::{
    PluginSurface, PluginSurfaceId, PluginSurfaceRegion,
    PluginSurfaceSnapshot, global_plugin_surface_registry,
};
use bmux_plugin::{AttachInputEndpoint, ExtensionRect, RenderOp, RenderStyle};
use uuid::Uuid;

let owner = "example.presentation";
let allocation = PluginLayoutId::new(owner, "leading");
let layout = global_plugin_layout_registry().publisher(owner);
let surfaces = global_plugin_surface_registry().publisher(owner);

layout.publish(PluginLayoutSnapshot {
    revision: 1,
    requests: vec![PluginLayoutRequest::split(
        allocation.clone(),
        0,
        LayoutEdge::Left,
        LayoutExtent::Cells(20),
    )],
})?;

let endpoint = AttachInputEndpoint {
    capability: "example.presentation.input".into(),
    interface_id: "presentation-input".into(),
    operation: "handle".into(),
};
let surface = PluginSurface::layout(
    PluginSurfaceId::new(owner, "main", Uuid::from_u128(1)),
    1,
    allocation,
    vec![RenderOp::text_run(0, 0, "Item", RenderStyle::new())],
)
.interactive_region(
    PluginSurfaceRegion::new("item-0", ExtensionRect::new(0, 0, 20, 1))
        .endpoint(endpoint),
);
surfaces.publish(PluginSurfaceSnapshot {
    revision: 1,
    surfaces: vec![surface],
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Keep both publishers alive while the presentation is active. Their `Drop`
implementations remove all retained state for the owner, preventing stale
layout, pixels, and input regions after unload. For updates, reuse the same
layout, surface, retained, and region IDs; publish a complete snapshot with a
strictly higher revision. Changing one retained operation lets the compositor
damage only that operation's bounds.

Input regions carry typed generic endpoints. The attach host supplies semantic
pointer phases or focused key events with surface-local coordinates. The plugin
interprets those events and invokes its domain's generated BPDL client; do not
put domain commands or handwritten transport envelopes into the generic
presentation API.

Publications are validated and bounded. Handle publication errors rather than
retrying in the frame loop. There is no synchronous render callback for these
surfaces. The current global registries are in-process attach-host capability;
out-of-process companions require a retained, revisioned publication transport
before they can use this path.

## Writing Command Plugins

### Dispatching commands

Use `route_command!` to match on the command name and auto-generate the
unknown-command fallback:

```rust
fn run_command(&mut self, ctx: NativeCommandContext) -> Result<i32, PluginCommandError> {
    bmux_plugin_sdk::route_command!(ctx, {
        "list"   => handle_list(&ctx),
        "create" => handle_create(&ctx),
    })
}
```

Each arm must evaluate to `Result<i32, PluginCommandError>`. Unrecognised
commands automatically return `Err(PluginCommandError::unknown_command(...))`.

### Error handling

`PluginCommandError` implements `From` for common error types, so the `?`
operator works naturally:

```rust
fn handle_list(ctx: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let data = std::fs::read_to_string("config.toml")?;  // io::Error -> PluginCommandError
    let parsed: Config = toml::from_str(&data)?;          // toml::de::Error -> PluginCommandError
    println!("{parsed:?}");
    Ok(EXIT_OK)
}
```

Supported conversions: `String`, `&str`, `std::io::Error`, `serde_json::Error`,
`toml::de::Error`, `Box<dyn Error>`, `Box<dyn Error + Send + Sync>`.

For custom error codes, construct directly:

```rust
Err(PluginCommandError::new(EXIT_USAGE, "missing required argument"))
Err(PluginCommandError::unavailable("feature not supported on this platform"))
```

### Exit codes

| Constant           | Value | Meaning                          |
| ------------------ | ----- | -------------------------------- |
| `EXIT_OK`          | 0     | Success                          |
| `EXIT_ERROR`       | 1     | Generic failure                  |
| `EXIT_USAGE`       | 64    | Bad arguments or unknown command |
| `EXIT_UNAVAILABLE` | 70    | Plugin unavailable               |

### Arguments

Commands receive arguments as `ctx.arguments: Vec<String>`. The host CLI layer
handles parsing and validation based on the argument declarations in
`plugin.toml` -- the plugin receives the parsed values as strings.

## Writing Service Plugins

Service plugins handle inbound requests from other plugins or the host runtime.

### Dispatching services

Use `route_service!` to match on `(interface_id, operation)` pairs. Each handler
receives a typed request and returns a typed response:

```rust
fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "my-service/v1", "do_thing" => |req: DoThingRequest, _ctx| {
            let result = do_the_thing(&req.input)?;
            Ok(DoThingResponse { output: result })
        },
    })
}
```

The macro wraps each handler in `handle_service()`, which handles request
deserialization, response serialization, and error conversion. Unrecognised
operations return a standard "unsupported" error.

Request and response types must implement `serde::Deserialize` and
`serde::Serialize` respectively.

### Returning errors from service handlers

Service handler closures return `Result<Resp, ServiceResponse>`. Use `map_err`
to convert domain errors:

```rust
"my-service/v1", "create" => |req: CreateRequest, _ctx| {
    create_thing(&req.name)
        .map_err(|e| ServiceResponse::error("create_failed", e.to_string()))
},
```

### The `handle_service` function

If you need more control than `route_service!` provides, use `handle_service`
directly:

```rust
handle_service(&context, |req: MyRequest, ctx| {
    // Full access to ctx for host API calls, self for plugin state
    Ok(MyResponse { ... })
})
```

## The Prelude

`use bmux_plugin_sdk::prelude::*` imports the ~16 items most plugins need:

- **Trait:** `RustPlugin`
- **Context types:** `NativeCommandContext`, `NativeLifecycleContext`, `NativeServiceContext`
- **Error type:** `PluginCommandError`
- **Exit codes:** `EXIT_OK`, `EXIT_ERROR`, `EXIT_USAGE`, `EXIT_UNAVAILABLE`
- **Service types:** `ServiceKind`, `ServiceResponse`
- **Events:** `PluginEvent`
- **Helpers:** `handle_service`, `decode_service_message`, `encode_service_message`

Types not in the prelude (import individually when needed):

- Host service DTOs: `SessionSelector`, `StorageGetRequest`, `ContextCreateRequest`, etc.
- Manifest types: `PluginCommand`, `PluginService`, `PluginEventSubscription`
- Capability types: `HostScope`, `PluginFeature`

## When to Also Depend on `bmux_plugin`

The `bmux_plugin` crate provides two traits that live outside the SDK:

- **`ServiceCaller`** -- the low-level trait for dispatching cross-plugin service calls
- **`HostRuntimeApi`** -- ergonomic methods like `ctx.session_list()`, `ctx.storage_get(...)`, `ctx.context_create(...)`

If your plugin calls host services (session management, storage, pane
operations, etc.), add `bmux_plugin` as a dependency and import these traits:

```rust
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
```

Simple plugins that only handle commands or receive service calls do **not**
need `bmux_plugin`.

## Manifest Reference (`plugin.toml`)

### Top-Level Fields

| Field                   | Type    | Required | Default    | Description                                                |
| ----------------------- | ------- | -------- | ---------- | ---------------------------------------------------------- |
| `id`                    | string  | **Yes**  | --         | Unique plugin identifier (e.g. `"bmux.clipboard"`)         |
| `name`                  | string  | **Yes**  | --         | Human-readable display name                                |
| `version`               | string  | **Yes**  | --         | Plugin version (semver)                                    |
| `description`           | string  | No       | --         | Optional description                                       |
| `homepage`              | string  | No       | --         | Optional URL                                               |
| `runtime`               | string  | No       | `"native"` | Runtime type (only `"native"` supported)                   |
| `bundled_only`          | string  | No       | --         | Why the plugin cannot load as a dynamic library            |
| `entry`                 | path    | No       | --         | Path to `.dylib`/`.so` (only for external plugins)         |
| `provider_priority`     | integer | No       | `0`        | Ordering when multiple plugins provide the same capability |
| `required_capabilities` | list    | No       | `[]`       | Host capabilities this plugin needs                        |
| `provided_capabilities` | list    | No       | `[]`       | Capabilities this plugin provides                          |
| `provided_features`     | list    | No       | `[]`       | Feature flags this plugin provides                         |

### Compatibility (optional)

```toml
[plugin_api]
minimum = "1.0"    # default; omit entire section if 1.0 is fine
maximum = "2.0"    # optional upper bound

[native_abi]
minimum = "1.0"    # default; omit entire section if 1.0 is fine
```

### Commands

```toml
[[commands]]
name          = "hello"             # required -- dispatch name (matches ctx.command)
summary       = "Print a greeting"  # required -- short description for help text
expose_in_cli = true                # default: false -- set true to show as CLI subcommand
path          = ["greet"]           # optional -- CLI subcommand path
aliases       = [["say", "hi"]]    # optional -- alternative CLI paths
execution     = "provider_exec"     # default: "provider_exec"
description   = "Longer help"       # optional
```

`execution = "provider_exec"` runs the command in the plugin provider process.
Use `execution = "caller_process"` for commands that need caller-local runtime
facilities, such as an attach client's prompt/modal host.

#### Command Arguments

```toml
[[commands.arguments]]
name          = "target"       # required
kind          = "string"       # required: string | integer | boolean | path | choice
required      = true           # default: false
position      = 0              # optional: positional index (omit for flags/options)
long          = "target"       # optional: --target
short         = "t"            # optional: -t
multiple      = true           # default: false
value_name    = "TARGET"       # optional: displayed in help
choice_values = ["a", "b"]    # for kind = "choice"
```

### Services

```toml
[[services]]
capability   = "bmux.clipboard.write"    # required -- host capability scope
interface_id = "clipboard-write/v1"      # required -- service interface identifier
kind         = "command"                 # required -- "command" or "query"
```

### Event Subscriptions

```toml
[[event_subscriptions]]
kinds = ["system", "window"]
names = ["server_started", "window_created"]
```

### Dependencies

```toml
[[dependencies]]
plugin_id   = "bmux.permissions"
version_req = "=0.0.1-alpha.0"
required    = true                # default: true
```

### Keybindings

```toml
[keybindings.runtime]
c = "plugin:bmux.tabs:new-tab"
"alt+w" = "plugin:bmux.tabs:switch-tab"

[keybindings.scroll]
y = "copy_scrollback"

[keybindings.global]
# global keybindings (active outside runtime mode)
```
