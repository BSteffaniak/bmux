# Plugin author API: `Plugin`, `Declarations` and `plugin!`

Design note for Wave 2 of the shared plugin architecture. It describes how
the single `Plugin` trait, the `Declarations` builder and the `plugin!` macro
consume the bindings BPDL already generates, how the bundled fast path and
the dylib path differ, and how a plugin crate builds its own dynamic library
without a generated wrapper crate.

The generated layer described in the first section exists today and is
proven by `examples/counter-plugin` (provider) and `examples/watcher-plugin`
(consumer). The rest is the Wave 2 design that builds on it.

## What BPDL generates today

All of this is generated from the canonical IR (`bmux_plugin_schema::ir`).
The runtime types it targets live in `bmux_plugin_sdk::author`.

For every service interface that has a capability, the interface module
(`counter_commands`) gains a module `v<N>` (`N` = `@interface-version`, else
1):

| Item                                       | Purpose                                                                                                                                                                                                                  |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `trait CounterCommands`                    | Async provider trait. Methods take `&author::Call` and return `author::Result<T>`. Every method is `where Self: Sized`, so authors write plain `async fn` and `dyn CounterCommands` stays a valid type for declarations. |
| `Client<T = PluginHostServices>`           | Typed client. `Client::direct(..)`/`from_handle(..)` calls a bundled provider with no serialization; `Client::remote(transport)` uses any `TypedDispatchClient` through the checked codec.                               |
| `dispatch(provider, call, context)`        | Serves one `PluginInstance::invoke` call: accepts checked and plain frames, maps provider failures to error responses.                                                                                                   |
| `Portable<P>`                              | `PortableServiceProvider` over a provider: the checked byte adapter for dylib and cross-process callers (streams included).                                                                                              |
| `register(provider, base, registry)`       | Registers the direct `Arc<dyn Erased>` and the portable adapter in a `TypedServiceRegistry`. Calls without host context run with `base`, the provider-bound `Call`.                                                      |
| `impl Provides<P> for dyn CounterCommands` | Type-level proof that `P` implements the interface; carries `dispatch`, `register` and `portable` as associated functions.                                                                                               |
| `impl RequiredInterface for Client<T>`     | Type-level marker for consumed interfaces.                                                                                                                                                                               |
| `impl ContractEvent for Changed`           | An `events` payload bound to its channel (`STATE_KIND`/`EVENT_KIND`) and delivery, so `cx.emit(Changed { .. })` is typed.                                                                                                |

Package-level declarations generate:

- `commands::BumpArgs` (typed, parsed from the manifest's `--long` arguments
  with `author::CommandArgParser`) and `commands::BUMP:
  CommandDecl<BumpArgs>`.
- `extension_points::<name>_v<N>`: the `Data` record, a `Behavior` trait
  (async, `where Self: Sized`), a wire `Request` enum, a `Point` type
  implementing `author::ExtensionPoint` (`ID`, `VERSION`, `Data`) and
  `impl<B: Behavior> Extends<B> for Point`.

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
```

`Declarations<P>` stores type-erased entries built from the generated
markers. Nothing in it is product specific.

```rust
impl<P: Plugin> Declarations<P> {
    /// `I` is a generated `dyn Interface`; fails to compile unless `P: Interface`.
    pub fn provide<I: ?Sized + Provides<P>>(&mut self) -> &mut Self {
        self.provided.push(Provided {
            descriptor: I::DESCRIPTOR,
            dispatch: <I as Provides<P>>::dispatch,
            register: <I as Provides<P>>::register,
            portable: <I as Provides<P>>::portable,
        });
        self
    }
    pub fn require<C: RequiredInterface>(&mut self) -> &mut Self;
    pub fn command<A: CommandArgs>(&mut self, decl: CommandDecl<A>, handler: CommandHandler<P, A>) -> &mut Self;
    /// One contribution: declarative data plus the behavior implementation.
    pub fn extend<E: Extends<P>>(&mut self, data: E::Data) -> &mut Self;
    pub fn extend_with<E: Extends<B>, B: 'static>(&mut self, data: E::Data, behavior: fn(&P) -> &B) -> &mut Self;
}
```

`plugin!` checks declarations against the generated manifest when the
definition is built: every `provide` must name an interface in
`declaration::provides`, every `require` one in `requires`, every `command`
one in `commands`, and nothing the world declares may be missing. Paths make
most mistakes compile errors already: an author can only spell
`declaration::provides::counter_commands::v1::CounterCommands`, and
`provide::<dyn I>()` for a type that does not implement `I` does not compile
(`packages/plugin-schema-macros/tests/ui/fail/undeclared_provider.rs`).

## `plugin!`

`plug::plugin!(Counter)` (optionally `create = path`) emits
`plugin_definition()`:

- metadata = `author::metadata(declaration::PLUGIN_MANIFEST,
  declaration::SERVICE_CONTRACTS)`, so the definition can never disagree with
  its declaration;
- a factory creating a fresh `P` per host scope and wrapping it in a generic
  `DeclaredInstance<P>` that implements today's `PluginInstance`:
  - `invoke` routes by interface id to the stored `dispatch` entry, with a
    host-built `Call` (invocation, caller, the scope's event emitter);
  - `register_typed_services` calls each `register` entry with the
    provider-bound base `Call`;
  - `portable_services` returns each `portable` adapter;
  - `command` parses through the stored `CommandDecl` and calls the handler;
  - `extension` dispatches through the stored `Extends` entry;
  - `activate`/`deactivate` build `Activation`/`Deactivation` from the
    lifecycle context and call the `Plugin` hooks.

The examples hand-write exactly this glue today (`CounterPlugin::invoke`
routes through `declaration::provides::*::v1::dispatch`). Wave 2 deletes the
hand-written glue, and later swaps `PluginInstance` for the slim host trait
without changing author code.

## Static fast path and dylib

|                   | Bundled                                                                    | Dylib                                                                                              |
| ----------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Provider entry    | Typed registry: `Arc<dyn Erased>` (a `Bound<P>`) plus the portable adapter | `PluginInstance::invoke` → generated `dispatch`; `Portable<P>` for checked callers                 |
| Consumer          | `Client::direct`/`from_handle`: one virtual call, no serialization         | `Client::remote(host)`: checked `bmux_codec` frames tagged with the interface's `SERVICE_CONTRACT` |
| Contract evidence | Rust types                                                                 | `modeled_service_contracts` from `SERVICE_CONTRACTS` (the host refuses checked routes without it)  |
| Call context      | Host-built per call in Wave 2; `base` today                                | Host-built from the invocation                                                                     |
| Events            | `Call::emit` → scope emitter                                               | Same, emitter attached by the host                                                                 |

Both modes run the same contract tests:
`examples/watcher-plugin/tests/both_modes.rs` (direct registry calls versus
codec frames through `PluginInstance::invoke`) and
`packages/cli/tests/author_api_dylib_integration.rs` (both examples packaged
as dylibs in a real server).

`cx.service::<counter::v1::Client>()` (Wave 2) resolves the provider's
registry handle and returns `Client::direct` when the provider is bundled in
the same host, otherwise `Client::remote` over the scope's host services.
Admission and authorization stay in the host for both: a direct handle is a
lease checked per call, never a raw reference.

## Packaging without wrapper crates

Today `scripts/package-plugin.py` generates a thin wrapper crate per dylib.
The target is no generated crate at all:

- A plugin crate keeps `[lib] crate-type = ["rlib"]` and adds role features:

  ```toml
  [features]
  dynamic-backend = []
  dynamic-frontend = []
  ```

- `plugin!` emits the single C entry symbol only under
  `#[cfg(all(bmux_plugin_dylib, feature = "dynamic-backend"))]` (and the
  frontend equivalent), so the symbol exists only in a packaged artifact for
  the selected role.
- The packager builds with
  `cargo rustc -p <crate> --lib --crate-type cdylib --features dynamic-backend`,
  sets `--cfg bmux_plugin_dylib` through `CARGO_ENCODED_RUSTFLAGS` (preserving
  existing flags) and uses its own `--target-dir`. `--crate-type` applies to
  that invocation only, so workspace builds never produce a cdylib.
- Two gates keep host builds clean: the cfg is never set by a workspace build
  (it is not a feature, so it cannot unify), and the feature only selects the
  role. A host that accidentally enabled `dynamic-backend` still gets no entry
  symbol without the cfg.
- The sidecar `plugin.toml` stays the artifact's embedded manifest
  (`package-plugin.py --declaration-of`).

## Known gaps

- Inside a dylib (and in the CLI process for command-local activations),
  `PluginHostServices` has no asynchronous host route yet
  (`call_service_async` is unsupported until dylib host waits land), so
  `Client::remote(host)` fails there. The watcher example uses an
  example-local `SyncHost` transport over the existing synchronous route
  until then; the generated client and provider are unchanged.
- The generated `dispatch` serves unary operations; streaming operations need
  the `invoke_streaming` entry (`Portable<P>` already streams).
- Clients do not yet generate `subscribe_<event>()`; it needs the host
  subscription handle from the scoped event bus.
- Commands are exposed in the CLI by the generated manifest, so a plugin
  still declares `owns_paths` for them in `plugin.toml`.
