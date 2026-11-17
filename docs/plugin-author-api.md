# Plugin author API: `Plugin`, `Declarations` and `plugin!`

The author API of the shared plugin architecture: how the single `Plugin`
trait, the `Declarations` builder and the `plugin!` macro consume the
bindings BPDL generates, how the bundled fast path and the dylib path
differ, and how a plugin crate builds its own dynamic library without a
generated wrapper crate.

All of it is implemented (`bmux_plugin_sdk::author`,
`scripts/package-plugin.py`, `bmux_plugin_test`) and proven by the reference
plugins, each tested bundled and as a real dylib under `TestHost`:

| Example                   | Exercises                                                                                       |
| ------------------------- | ----------------------------------------------------------------------------------------------- |
| `examples/hello-plugin`   | a BPDL command with typed arguments                                                             |
| `examples/counter-plugin` | provided services, a retained contract event (`emits`), a command calling back through the host |
| `examples/watcher-plugin` | required services, a typed event subscription in an activation-owned task, a provided query     |
| `examples/clock-plugin`   | a contribution to a host-declared extension point (`bmux_extpoints`)                            |

`examples/native-plugin` covers hand-declared (`native_command`) commands.
Every BMUX workspace plugin is defined this way; the legacy author trait,
its adapter and the generated-wrapper packager are gone.

## What BPDL generates today

All of this is generated from the canonical IR (`bmux_plugin_schema::ir`).
The runtime types it targets live in `bmux_plugin_sdk::author`.

For every service interface that has a capability, the interface module
(`counter_commands`) gains a module `v<N>` (`N` = `@interface-version`, else
1):

| Item                                                | Purpose                                                                                                                                                                                                                  |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `trait CounterCommands`                             | Async provider trait. Methods take `&author::Call` and return `author::Result<T>`. Every method is `where Self: Sized`, so authors write plain `async fn` and `dyn CounterCommands` stays a valid type for declarations. |
| `Client<T = PluginHostServices>`                    | Typed client. `Client::direct(..)`/`from_handle(..)` calls a bundled provider with no serialization; `Client::remote(transport)` uses any `TypedDispatchClient` through the checked codec.                               |
| `dispatch(provider, call, context)`                 | Serves one `PluginInstance::invoke` call: accepts checked and plain frames, maps provider failures to error responses.                                                                                                   |
| `dispatch_streaming(provider, call, context)`       | Serves one `PluginInstance::invoke_streaming` call: ordered event frames into the invocation's sink, then the single terminal response.                                                                                  |
| `impl ServiceClient for Client<PluginHostServices>` | Lets `Activation::service::<Client>()` connect a declared requirement through the host.                                                                                                                                  |
| `Portable<P>`                                       | `PortableServiceProvider` over a provider: the checked byte adapter for dylib and cross-process callers (streams included).                                                                                              |
| `register(provider, base, registry)`                | Registers the direct `Arc<dyn Erased>` and the portable adapter in a `TypedServiceRegistry`. Calls without host context run with `base`, the provider-bound `Call`.                                                      |
| `impl Provides<P> for dyn CounterCommands`          | Type-level proof that `P` implements the interface; carries `dispatch`, `register` and `portable` as associated functions.                                                                                               |
| `impl RequiredInterface for Client<T>`              | Type-level marker for consumed interfaces.                                                                                                                                                                               |
| `impl ContractEvent for Changed`                    | An `events` payload bound to its channel (`STATE_KIND`/`EVENT_KIND`) and delivery, so `cx.emit(Changed { .. })` is typed.                                                                                                |

An events interface (`counter_events`, which has an `events` item and no
operations) generates, in its `v<N>` module, a subscription client:

| Item                                              | Purpose                                                                                                                        |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `Client<T = PluginHostServices>`                  | `Client::remote(source)` over any `author::EventSource`; the activation's `PluginHostServices` is one in both packaging modes. |
| `subscribe_<event>().await`                       | An `author::Subscription<Changed>`: typed items in order (a state channel yields its current value first).                     |
| `impl EventClient for Client<PluginHostServices>` | Lets `Activation::events::<Client>()` connect a declared subscription.                                                         |

Package-level declarations generate:

- `commands::BumpArgs` (typed, parsed from the manifest's `--long` arguments
  with `author::CommandArgParser`) and `commands::BUMP:
  CommandDecl<BumpArgs>`.
- `extension_points::<name>_v<N>`: the `Data` record, a `Behavior` trait
  (async, `where Self: Sized`), a wire `Request` enum, a `Point` type
  implementing `author::ExtensionPoint` (`ID`, `VERSION`, `Data`) and
  `impl<B: Behavior> Extends<B> for Point`. By convention, a data field
  named `order` (an integer) generates `ExtensionPoint::order` and one named
  `id` (a string) generates `ExtensionPoint::key`; hosts sort and
  deduplicate contributions with them.

The manifest macros (`schema! { manifest }`, `plugin_manifest!`) emit, next
to `PLUGIN_ID` and `PLUGIN_MANIFEST`:

- `provides`: re-exports exactly the provided interfaces;
- `requires`: re-exports exactly the required interfaces, so a client for an
  undeclared dependency has no path;
- `SERVICE_CONTRACTS`: the provided interfaces' generated descriptors.
  `author::metadata(PLUGIN_MANIFEST, SERVICE_CONTRACTS)` builds the plugin
  metadata and embeds them as `modeled_service_contracts`, the evidence a
  host needs to route checked frames to a dylib.

BPDL keeps one service kind per interface (queries, commands and events are
separate interfaces), so the spec's single `counter::v1::Counter` is three
generated traits here (`counter_commands`, `counter_state`,
`counter_events`). Relaxing that needs a service kind per operation in the
manifest and host routing; it is not required for Wave 2.

## `Plugin` and `Declarations`

```rust
pub trait Plugin: Send + Sync + 'static {
    fn declare(d: &mut Declarations<Self>) where Self: Sized;
    async fn activate(&self, cx: &mut Activation) -> Result<()> { Ok(()) }
    async fn deactivate(&self, cx: &mut Deactivation) -> Result<()> { Ok(()) }
}

impl Plugin for Counter {
    fn declare(d: &mut Declarations<Self>) {
        d.provide::<dyn counter_commands::v1::CounterCommands>()
            .provide::<dyn counter_state::v1::CounterState>()
            .command(BUMP, Self::bump); // async fn bump(&self, &Call, BumpArgs) -> Result<()>
    }
}
bmux_plugin_sdk::plugin!(Counter);
```

`Declarations<P>` stores type-erased entries built from the generated
markers; nothing in it is product specific:

| Method                          | Entry                                                                                                                                     |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `provide::<dyn I>()`            | `I::DESCRIPTOR` plus the generated `dispatch`, `dispatch_streaming`, `register` and `portable` functions. Does not compile unless `P: I`. |
| `require::<C>()`                | The client's descriptor; `Activation::service::<C>()` refuses undeclared requirements.                                                    |
| `command(decl, handler)`        | A BPDL command with typed arguments; `handler` is any `async fn(&self, &Call, Args) -> Result<()>`.                                       |
| `native_command(name, handler)` | A command still declared by hand in `plugin.toml`, handled with the raw `NativeCommandContext` (`native-plugin` uses it).                 |
| `extend::<E>(data)`             | A contribution to extension point `E` with `P` implementing its `Behavior`.                                                               |
| `emits::<E>()`                  | A contract event `P` publishes: its channel is registered for the activation before `activate` runs.                                      |
| `subscribe::<C>()`              | An events interface `P` follows; `Activation::events::<C>()` refuses undeclared subscriptions.                                            |
| `on_event(handler)`             | Host events delivered to the plugin.                                                                                                      |
| `native_services(handler)`                  | One async handler over the raw service context for every manifest service not `provide`d with generated bindings (opaque or hand-routed interfaces). It owns checked framing. |
| `blocking_services(unary, streaming)`       | The same with synchronous bodies, run on the host's blocking pool inside the call's callback entry; checked frames are decoded with the generated contract and re-framed.     |
| `blocking_command(name, handler)`           | A hand-declared command with a synchronous body run inside its callback entry; command-outcome metadata it records is returned.                                               |
| `blocking_events(handler)`                  | Host events with a synchronous body run inside the event's entry.                                                                                                             |
| `blocking_activate` / `blocking_deactivate` | Synchronous lifecycle bodies run inside the lifecycle entry, after `Plugin::activate` / `Plugin::deactivate`.                                                                 |
| `typed_services(register)`                  | Context-aware registration of typed handles that are not generated providers; may refuse.                                                                                     |
| `registrations(register)`                   | Activation-time contributions (state, readiness, frontend extensions).                                                                                                        |

`DeclaredInstance::new` validates the declarations against the generated
manifest before any instance runs: every interface the world provides with
generated bindings (`SERVICE_CONTRACTS`) is provided exactly once and nothing
else, every `require` names a capability the world requires, and the handled
commands are exactly the manifest's. Paths make most mistakes compile errors
already: an author can only spell
`declaration::provides::counter_commands::v1::CounterCommands`, and
`provide::<dyn I>()` for a type that does not implement `I` does not compile
(`packages/plugin-schema-macros/tests/ui/fail/undeclared_provider.rs`).

`Activation` gives the plugin id, settings, the lifecycle context, the host
services, typed clients (`service::<C>()`), typed event clients
(`events::<C>()`), activation-owned tasks (`spawn`, on the host runtime
through plug-rt) and `bind_events` to replace the emitter behind every
`Call::emit`.

## Contract events

```rust
impl Plugin for Counter {
    fn declare(d: &mut Declarations<Self>) {
        d.provide::<dyn counter_commands::v1::CounterCommands>()
            .emits::<counter_events::v1::Changed>();
    }
}
// in a provider method: cx.emit(counter_events::v1::Changed { value })?;

impl Plugin for Watcher {
    fn declare(d: &mut Declarations<Self>) {
        d.subscribe::<counter_events::v1::Client>();
    }
    async fn activate(&self, cx: &mut Activation) -> Result<()> {
        let mut changes = cx
            .events::<counter_events::v1::Client>()?
            .subscribe_changed()
            .await?;
        cx.spawn("observe-changes", async move {
            while let Some(change) = changes.next().await {
                // change: author::Result<Changed>
            }
        })
    }
}
```

Both sides go through the activation's host services, the same in both
packaging modes:

- Publishing: `PluginHostServices::register_event_channel(kind, state)` and
  `publish_event(kind, json)`. In process they use the activation's scoped
  view of the host `EventBus`. Inside a dylib they use the instance's plug-rt
  event port. The port's `event_register_state` carries retained state
  channels across the boundary. `DeclaredInstance` registers every `emits`
  channel before `activate` and binds `Call::emit` to the route. A channel
  emitted without a declaration is registered on first use. Without a
  route (a host that bound no event bus), events stay unbound as before.
  Channels belong to the activation and go away with it.
- Payloads cross as JSON, the bus's serialized form: `Call::emit` encodes
  with `serde_json`, and subscriptions decode the same bytes.
- Subscribing: `subscribe_<event>()` wraps
  `PluginHostServices::subscribe_event`. The subscription is owned by the
  activation: it ends when the activation is released, and dropping it
  releases it. Waits are woken by the host; nothing polls. A registered
  state channel with no publication yet yields nothing. A payload that does
  not decode arrives as an `Err` item, never silently dropped.

## `plugin!`

`bmux_plugin_sdk::plugin!(Counter)` (optionally `create = expr` for a
`Fn(&PluginCreateContext) -> Result<Counter>`, evaluated once per
`plugin_definition()` so it may capture per-definition ports, and
`declaration = path` when
the generated module is not `crate::declaration`) emits
`plugin_definition()`:

- metadata = `author::metadata(declaration::PLUGIN_MANIFEST,
  declaration::SERVICE_CONTRACTS)`, so the definition can never disagree with
  its declaration;
- a factory creating a fresh `P` per host scope wrapped in
  `DeclaredInstance<P>`, a generic implementation of today's
  `PluginInstance` (the host is unchanged):
  - `invoke` / `invoke_streaming` route by interface id to the stored
    `dispatch` / `dispatch_streaming`, with a host-built `Call`;
  - `register_typed_services` and `portable_services` use the stored
    `register` / `portable` functions with the provider-bound base `Call`;
  - `command` parses through the stored `CommandDecl` and calls the handler;
  - `extension` dispatches through the stored `Extends` entry, and
    `registrations` reports each `extend` as an extension contribution;
  - `activate_with_async` / `activate` / `deactivate` call the `Plugin` hooks
    with `Activation` / `Deactivation`.

It also emits `plugin_crate()`: the definition plus the crate's
`Cargo.toml`, which is what `TestHost` loads in either mode.

Later Wave 2 swaps `PluginInstance` for the slim host trait without changing
author code.

## Static fast path and dylib

|                   | Bundled                                                                    | Dylib                                                                                              |
| ----------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Provider entry    | Typed registry: `Arc<dyn Erased>` (a `Bound<P>`) plus the portable adapter | `PluginInstance::invoke` → generated `dispatch`; `Portable<P>` for checked callers                 |
| Consumer          | `Client::direct`/`from_handle`: one virtual call, no serialization         | `Client::remote(host)`: checked `bmux_codec` frames tagged with the interface's `SERVICE_CONTRACT` |
| Contract evidence | Rust types                                                                 | `modeled_service_contracts` from `SERVICE_CONTRACTS` (the host refuses checked routes without it)  |
| Call context      | Host-built per call in Wave 2; `base` today                                | Host-built from the invocation                                                                     |
| Events            | `Call::emit` → activation's bus view                                       | `Call::emit` → plug-rt event port → the same bus view                                              |
| Subscriptions     | `subscribe_<event>()` → bus view                                           | `subscribe_<event>()` → plug-rt event port, woken by the host                                      |
| Contributions     | registrations → host `ExtensionPoints`; behavior via `extension`           | Same, across the boundary                                                                          |

Both modes run the same contract tests: every example's
`tests/both_modes.rs` under `TestHost`, and
`packages/cli/tests/author_api_dylib_integration.rs` (counter and watcher
packaged as dylibs in a real server, including the watcher observing the
counter's changes).

`cx.service::<counter::v1::Client>()` (Wave 2) resolves the provider's
registry handle and returns `Client::direct` when the provider is bundled in
the same host, otherwise `Client::remote` over the scope's host services.
Admission and authorization stay in the host for both: a direct handle is a
lease checked per call, never a raw reference.

## Testing in both modes: `TestHost`

`bmux_plugin_test::TestHost` runs any `Plugin` under the same assertions,
bundled and as a real dynamic library:

```rust
async fn contract(mode: Mode) -> author::Result<()> {
    let host = TestHost::new(mode)
        .load(bmux_example_counter_plugin::plugin_crate())
        .await?;
    let mut changes = host.subscribe::<counter_events::v1::Changed>()?;
    let host = host.load(bmux_example_watcher_plugin::plugin_crate()).await?;
    assert_eq!(host.client::<counter_state::v1::Client>().get().await?, 5);
    assert_eq!(host.command("example.watcher", "watch", &["--by", "4"]).await?, 0);
    host.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_contract() {
    bmux_plugin_test::both_modes(contract).await.unwrap();
}
```

- The host is the real scoped loader (`DefinitionArtifact`,
  `LoadedExecution`), one host `EventBus`, an `ExtensionPoints` registry and a
  service router standing in for the server's. `Mode::Bundled` loads
  `plugin_definition()` in process. `Mode::Dylib` packages the crate with
  `scripts/package-plugin.py` into `<target>/plugin-dylib`, once per test
  process (python3 3.11+), and loads it into a process-wide host runtime: a
  library image is installed into one runtime and never unloaded.
- Plugins activate in load order. Calls between plugins, and from the test
  through `remote::<C>()`, go through the host as checked frames,
  attributed to the caller and checked against its granted capabilities
  (what it requires, plus what it provides). `client::<C>()` uses the
  bundled direct-handle fast path when there is one
  (`ServiceClient::direct`), otherwise the codec.
- `subscribe::<E>()` records contract events from the bus. `events::<C>()`
  returns an events client. `fake::<dyn I, _>(provider)` serves an interface
  from the test. `command(plugin, name, args)` runs a plugin command.
  `extension_point::<E>()` and `extensions::<E>()` declare a point and read
  its contributions. `shutdown()` deactivates in reverse order and fails if
  an activation left tasks behind.

## Host-declared extension points

A host program declares the extension points it offers. Plugins contribute
in `declare()`:

```rust
// bmux_extpoints (bpdl/bmux-extpoints.bpdl):
//   extension-point status-item version 1 {
//     data { id: string, order: u32, label: string? }
//     interface { query render(width: u16) -> string; }
//   }
impl Plugin for Clock {
    fn declare(d: &mut Declarations<Self>) {
        d.extend::<status_item_v1::Point>(status_item_v1::Data {
            id: "clock".into(), order: 100, label: Some("UTC".into()),
        });
    }
}
impl status_item_v1::Behavior for Clock { /* async fn render(..) */ }
```

- `packages/extpoints` (`bmux_extpoints`) is the BMUX product layer: BPDL
  extension points that BMUX declares, starting with `status-item`. It sits
  above the framework, and no core crate depends on it.
- `bmux_plugin::ExtensionPoints` is the generic host side. A host declares
  points (`declare::<Point>()`) and collects contributions from each
  instance's registrations (`contribute_registrations`, both packaging
  modes). Consumers read `set::<Point>()`, an `ExtensionSet` holding a
  snapshot sorted by `order` with the plugin id as a stable tiebreak, plus a
  change stream (`changed()`). Each entry's `call(&Request::Render { .. })`
  runs the contributor's behavior through its extension port.
- Refused, with the plugins named: a contribution to an undeclared point,
  data that does not decode as the point's `Data`, and a duplicate `id`.
  `withdraw(plugin)` removes a plugin's contributions when its instance
  goes away.
- Plugins can still declare their own extension points in their package
  (`counter-display`). Such points are typed the same way, but no host
  declares them.

## Packaging without wrapper crates

A plugin crate builds its own dynamic library; nothing is generated.

- The crate keeps `[lib] crate-type = ["rlib"]` and declares the role
  features:

  ```toml
  [features]
  dynamic-backend = []
  dynamic-frontend = []
  ```

- `plugin!` emits the single C entry symbol only under
  `--cfg bmux_plugin_dylib` plus exactly one role feature (enabling both is a
  compile error), and restricts the packaged definition to that role. The cfg
  is evaluated inside the SDK's own macro definitions, so plugin crates never
  name it. The packager's build label (`BMUX_PLUGIN_ARTIFACT_ID`) is retained
  in the artifact bytes.
- `scripts/package-plugin.py --implementation-manifest <crate>/Cargo.toml
  --component backend --build` runs
  `cargo rustc --lib --crate-type cdylib --no-default-features
  --features <component features>,dynamic-backend` on the crate in its own
  workspace, with `--cfg bmux_plugin_dylib` added to `CARGO_ENCODED_RUSTFLAGS`
  (existing flags preserved) and a target directory reserved for dylib builds
  (`<target>/plugin-dylib`, or `--cargo-target-dir`). `--crate-type` applies
  to that invocation only, so workspace builds never produce a cdylib.
- The SDK is whatever the crate's own dependency graph resolves (`cargo
  metadata` over its Cargo.lock): a path SDK in this workspace, or bmux pinned
  from git (`git = "...", branch = "master"`) in bcode. The receipt
  (`--output/packaging.json`) records it, e.g.
  `git+https://…?branch=master#<commit>`. There is no `--sdk-manifest`.
  `packages/cli/tests/plugin_packaging_git_source_integration.rs` packages a
  plugin whose bmux dependencies come from a `file://` git repository.
- Two gates keep host builds clean: the cfg is never set by a workspace build
  (it is not a feature, so it cannot unify), and the feature only selects the
  role. A host that accidentally enabled `dynamic-backend` still gets no entry
  symbol without the cfg.
- The sidecar `plugin.toml` stays the artifact's embedded manifest
  (`package-plugin.py --declaration-of`).

Every BMUX workspace plugin declares the role features and is packaged this
way by `bmux plugin rebuild`; there is no generated-wrapper path. A crate
without the role feature for a component cannot be packaged as that component
(bcode plugins gain the features as they move to `plugin!`).

BPDL commands are exposed in the CLI, so the generated manifest also claims
their paths (`owns_paths`); `plugin.toml` may add paths, never remove them.

## Known gaps

- Inside a dylib, `PluginHostServices::call_service_async` runs each call as
  a host task owned by the instance's scope (plug-rt `rt::services`), awaited
  with real wakers; dropping the future cancels the call on the host. The
  watcher example uses `Client::remote(host)` from its server-side
  activation, its CLI command-local activation, and its async `watch`
  command body. A command body (or blocking work it awaits) runs outside
  the author's callback entry; its calls take the host's selected dispatch
  for that command's own invocation (same plugin attribution, declared
  capability checks and issuing route), and calls made while an entry is
  held keep the entry's ancestry. A nested plugin command
  (`plugin_command_run`) still needs a live entry, and an entry cannot be
  held across an await.
- Streaming operations (`emits`) are served on the provider side in both
  modes. The generated `dispatch_streaming` runs behind
  `DeclaredInstance::invoke_streaming`, and `Portable<P>` streams for checked
  callers. Plain `dispatch` answers a streaming operation with
  `streaming_required`. A consumer cannot call one through the host yet:
  `PluginHostServices` does not implement `supports_service_streaming`, so
  generated clients reject request-scoped streams over
  `Client::remote(host)`. `TestHost` therefore has no streaming route
  either, and no example exercises one. Deferred: it needs a host streaming
  route (sink plus credits) on the host-services port and the plug-rt
  table, in both modes.
- Contribution data lives in `declare()`, and hosts read it from an
  instance's registrations once the instance is activated. BPDL
  `contribute alias.point@N { .. }` parses and validates but is not yet
  lowered into the generated manifest, so a host cannot read contributions
  without creating an instance. Deferred: the manifest needs the encoded
  data, and `Declarations::validate` should then match `extend` against it,
  as it does for commands.
- The BMUX server does not yet declare `bmux_extpoints` points or consume an
  `ExtensionSet`. Only `TestHost` hosts them today. The existing surface,
  layout, attach and prompt contracts still live in `packages/plugin` and
  move to `bmux_extpoints` with the Wave 2 plugin port.
- `TestHost` has no virtual clock. Plugins use the real plug-rt timers, and
  tests wait with bounded polls.
