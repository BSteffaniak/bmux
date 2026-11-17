//! The single author trait, its declaration builder and the host bridge.
//!
//! A plugin implements [`Plugin`]: `declare` lists what it provides,
//! requires, handles and contributes (all type-checked against the
//! BPDL-generated bindings), and `activate`/`deactivate` own its runtime
//! work. [`crate::plugin!`] turns the type into a `plugin_definition()` whose
//! instances are [`DeclaredInstance`]s: a generic implementation of today's
//! `PluginInstance`, so the host is unchanged.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

use super::{
    BoxFuture, Call, CommandArgs, CommandDecl, Error, EventDelivery, EventEmitter, Extends,
    Provides, RequiredInterface, Result,
};
use crate::definition::{
    CommandOutcome, PluginCallContext, PluginCreateContext, PluginFuture, PluginHostServices,
    PluginInstance, PluginLifecycleContext, PluginResult,
};
use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    NativeStreamingServiceContext, PluginEvent, PluginEventKind, PortableServiceProvider,
    ServiceInterfaceDescriptor, ServiceResponse, TypedServiceRegistry,
};

/// The one author trait.
///
/// Construction uses `Default` or a `create` function named in
/// [`crate::plugin!`]; every host scope gets a fresh instance.
pub trait Plugin: Send + Sync + 'static {
    /// What this plugin provides, requires, handles and contributes. Pure:
    /// no side effects and no host access.
    fn declare(d: &mut Declarations<Self>)
    where
        Self: Sized;

    /// Start: subscribe, spawn scoped tasks, read settings. Everything
    /// acquired here is owned by the activation.
    fn activate(&self, cx: &mut Activation) -> impl Future<Output = Result<()>> + Send {
        let _ = cx;
        async { Ok(()) }
    }

    /// Optional final flush; scoped tasks are already being drained.
    fn deactivate(&self, cx: &mut Deactivation) -> impl Future<Output = Result<()>> + Send {
        let _ = cx;
        async { Ok(()) }
    }
}

/// A typed client a plugin can obtain from its activation.
pub trait ServiceClient: RequiredInterface + Sized {
    /// Connect through the activation's host services.
    fn connect(host: PluginHostServices) -> Self;
}

/// A command handler: `async fn(&self, &Call, Args) -> Result<()>`.
pub trait CommandHandler<'a, P: 'a, A>: Send + Sync {
    type Future: Future<Output = Result<()>> + Send + 'a;
    fn call(&self, plugin: &'a P, cx: &'a Call, args: A) -> Self::Future;
}

impl<'a, P: 'a, A, F, Fut> CommandHandler<'a, P, A> for F
where
    F: Fn(&'a P, &'a Call, A) -> Fut + Send + Sync,
    Fut: Future<Output = Result<()>> + Send + 'a,
{
    type Future = Fut;
    fn call(&self, plugin: &'a P, cx: &'a Call, args: A) -> Fut {
        self(plugin, cx, args)
    }
}

/// A command handler over the raw host command context, for commands a
/// plugin's `plugin.toml` still declares by hand (not modeled in BPDL).
pub trait NativeCommandHandler<'a, P: 'a>: Send + Sync {
    type Future: Future<Output = Result<i32>> + Send + 'a;
    fn call(&self, plugin: &'a P, cx: &'a Call, context: NativeCommandContext) -> Self::Future;
}

impl<'a, P: 'a, F, Fut> NativeCommandHandler<'a, P> for F
where
    F: Fn(&'a P, &'a Call, NativeCommandContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<i32>> + Send + 'a,
{
    type Future = Fut;
    fn call(&self, plugin: &'a P, cx: &'a Call, context: NativeCommandContext) -> Fut {
        self(plugin, cx, context)
    }
}

/// An event handler: `async fn(&self, PluginEvent) -> Result<()>`.
pub trait EventHandler<'a, P: 'a>: Send + Sync {
    type Future: Future<Output = Result<()>> + Send + 'a;
    fn call(&self, plugin: &'a P, event: PluginEvent) -> Self::Future;
}

impl<'a, P: 'a, F, Fut> EventHandler<'a, P> for F
where
    F: Fn(&'a P, PluginEvent) -> Fut + Send + Sync,
    Fut: Future<Output = Result<()>> + Send + 'a,
{
    type Future = Fut;
    fn call(&self, plugin: &'a P, event: PluginEvent) -> Fut {
        self(plugin, event)
    }
}

type Dispatch<P> =
    for<'a> fn(&'a P, Call, NativeServiceContext) -> BoxFuture<'a, Result<ServiceResponse>>;
type DispatchStreaming<P> = for<'a> fn(
    &'a P,
    Call,
    NativeStreamingServiceContext,
) -> BoxFuture<'a, Result<ServiceResponse>>;
type Register<P> = fn(Arc<P>, Call, &mut TypedServiceRegistry) -> crate::Result<()>;
type Portable<P> = fn(Arc<P>, Call) -> Arc<dyn PortableServiceProvider>;
type ExtensionDispatch<P> = for<'a> fn(&'a P, &'a Call, &'a [u8]) -> BoxFuture<'a, Result<Vec<u8>>>;
type CommandFn<P> = Box<
    dyn for<'a> Fn(&'a P, Call, NativeCommandContext) -> BoxFuture<'a, Result<i32>> + Send + Sync,
>;
type EventFn<P> =
    Box<dyn for<'a> Fn(&'a P, PluginEvent) -> BoxFuture<'a, Result<()>> + Send + Sync>;

struct Provided<P> {
    descriptor: ServiceInterfaceDescriptor,
    dispatch: Dispatch<P>,
    dispatch_streaming: DispatchStreaming<P>,
    register: Register<P>,
    portable: Portable<P>,
}

struct Command<P> {
    name: &'static str,
    run: CommandFn<P>,
}

struct Contribution<P> {
    id: &'static str,
    data: std::result::Result<Vec<u8>, String>,
    dispatch: ExtensionDispatch<P>,
}

/// What a plugin declares in [`Plugin::declare`].
pub struct Declarations<P> {
    provided: Vec<Provided<P>>,
    required: Vec<ServiceInterfaceDescriptor>,
    commands: Vec<Command<P>>,
    contributions: Vec<Contribution<P>>,
    events: Option<EventFn<P>>,
}

impl<P> Default for Declarations<P> {
    fn default() -> Self {
        Self {
            provided: Vec::new(),
            required: Vec::new(),
            commands: Vec::new(),
            contributions: Vec::new(),
            events: None,
        }
    }
}

impl<P: Send + Sync + 'static> Declarations<P> {
    /// Provide a generated service interface: `provide::<dyn counter_commands::v1::CounterCommands>()`.
    /// Does not compile unless `P` implements it.
    pub fn provide<I: ?Sized + Provides<P>>(&mut self) -> &mut Self {
        self.provided.push(Provided {
            descriptor: I::DESCRIPTOR,
            dispatch: <I as Provides<P>>::dispatch,
            dispatch_streaming: <I as Provides<P>>::dispatch_streaming,
            register: <I as Provides<P>>::register,
            portable: <I as Provides<P>>::portable,
        });
        self
    }

    /// Require a generated service interface: `require::<counter_state::v1::Client>()`.
    pub fn require<C: RequiredInterface>(&mut self) -> &mut Self {
        self.required.push(C::DESCRIPTOR);
        self
    }

    /// Handle a BPDL command with typed arguments.
    pub fn command<A, F>(&mut self, decl: CommandDecl<A>, handler: F) -> &mut Self
    where
        A: CommandArgs + Send + 'static,
        F: for<'a> CommandHandler<'a, P, A> + Copy + 'static,
    {
        self.commands.push(Command {
            name: decl.name(),
            run: Box::new(move |plugin, call, context| {
                let args = A::parse(&context.arguments);
                Box::pin(async move {
                    handler.call(plugin, &call, args?).await?;
                    Ok(crate::EXIT_OK)
                })
            }),
        });
        self
    }

    /// Handle a command `plugin.toml` declares by hand, with the raw host
    /// context. Prefer BPDL commands and [`Self::command`].
    pub fn native_command<F>(&mut self, name: &'static str, handler: F) -> &mut Self
    where
        F: for<'a> NativeCommandHandler<'a, P> + Copy + 'static,
    {
        self.commands.push(Command {
            name,
            run: Box::new(move |plugin, call, context| {
                Box::pin(async move { handler.call(plugin, &call, context).await })
            }),
        });
        self
    }

    /// Contribute to an extension point: its declarative data, with `P`
    /// implementing the point's `Behavior`.
    // The data is handed off to the declaration, mirroring `d.extend(data)`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn extend<E: Extends<P>>(&mut self, data: E::Data) -> &mut Self {
        self.contributions.push(Contribution {
            id: E::ID,
            data: crate::encode_service_message(&data).map_err(|error| error.to_string()),
            dispatch: <E as Extends<P>>::dispatch,
        });
        self
    }

    /// Receive host events delivered to this plugin.
    pub fn on_event<F>(&mut self, handler: F) -> &mut Self
    where
        F: for<'a> EventHandler<'a, P> + Copy + 'static,
    {
        self.events = Some(Box::new(move |plugin, event| {
            Box::pin(async move { handler.call(plugin, event).await })
        }));
        self
    }

    /// Check the declarations against the generated manifest: every provided
    /// interface with generated bindings is served exactly once, required
    /// interfaces are required by the world, commands are the manifest's,
    /// and contribution data encodes.
    ///
    /// # Errors
    /// Names the first mismatch.
    pub fn validate(&self, manifest: &str, services: &[ServiceInterfaceDescriptor]) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct Commands {
            name: String,
        }
        #[derive(serde::Deserialize)]
        struct Manifest {
            #[serde(default)]
            required_capabilities: Vec<String>,
            #[serde(default)]
            optional_capabilities: Vec<String>,
            #[serde(default)]
            commands: Vec<Commands>,
        }
        let manifest: Manifest =
            toml::from_str(manifest).map_err(|error| Error::failed(error.to_string()))?;
        let mut provided = BTreeSet::new();
        for entry in &self.provided {
            let id = entry.descriptor.interface_id.as_str();
            if !provided.insert(id) {
                return Err(Error::failed(format!("interface '{id}' is provided twice")));
            }
            if !services
                .iter()
                .any(|service| service.interface_id.as_str() == id)
            {
                return Err(Error::failed(format!(
                    "declared interface '{id}' is not provided by the BPDL world"
                )));
            }
        }
        if let Some(missing) = services
            .iter()
            .find(|service| !provided.contains(service.interface_id.as_str()))
        {
            return Err(Error::failed(format!(
                "the BPDL world provides '{}' but declare() does not",
                missing.interface_id.as_str()
            )));
        }
        for required in &self.required {
            let capability = required.capability.as_str();
            if !manifest
                .required_capabilities
                .iter()
                .chain(&manifest.optional_capabilities)
                .any(|declared| declared == capability)
            {
                return Err(Error::failed(format!(
                    "required interface '{}' needs capability '{capability}', which the BPDL world does not require",
                    required.interface_id.as_str()
                )));
            }
        }
        let mut commands = BTreeSet::new();
        for command in &self.commands {
            if !commands.insert(command.name) {
                return Err(Error::failed(format!(
                    "command '{}' is handled twice",
                    command.name
                )));
            }
            if !manifest
                .commands
                .iter()
                .any(|declared| declared.name == command.name)
            {
                return Err(Error::failed(format!(
                    "command '{}' is not declared by the manifest",
                    command.name
                )));
            }
        }
        if let Some(missing) = manifest
            .commands
            .iter()
            .find(|declared| !commands.contains(declared.name.as_str()))
        {
            return Err(Error::failed(format!(
                "the manifest declares command '{}' but declare() does not handle it",
                missing.name
            )));
        }
        for contribution in &self.contributions {
            if let Err(error) = &contribution.data {
                return Err(Error::failed(format!(
                    "contribution data for '{}' does not encode: {error}",
                    contribution.id
                )));
            }
        }
        Ok(())
    }

    /// Extension points this plugin contributes to, with encoded data.
    pub fn contributions(&self) -> impl Iterator<Item = (&'static str, &[u8])> {
        self.contributions.iter().filter_map(|contribution| {
            contribution
                .data
                .as_ref()
                .ok()
                .map(|data| (contribution.id, data.as_slice()))
        })
    }

    /// Descriptors of the declared required interfaces.
    #[must_use]
    pub fn required(&self) -> &[ServiceInterfaceDescriptor] {
        &self.required
    }
}

/// The activation context handed to [`Plugin::activate`].
pub struct Activation {
    context: PluginLifecycleContext,
    tasks: Option<crate::HostAsyncHandle>,
    required: Vec<ServiceInterfaceDescriptor>,
    events: Arc<Mutex<Arc<dyn EventEmitter>>>,
}

impl Activation {
    /// The plugin's id.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.context.native.plugin_id
    }

    /// This plugin's settings, if configured.
    #[must_use]
    pub const fn settings(&self) -> Option<&toml::Value> {
        self.context.native.settings.as_ref()
    }

    /// The host lifecycle context (connection paths, registered services…).
    #[must_use]
    pub const fn native(&self) -> &NativeLifecycleContext {
        &self.context.native
    }

    /// Host-supplied in-process values for this activation (bundled only).
    #[must_use]
    pub const fn capabilities(&self) -> &crate::definition::ActivationCapabilities {
        &self.context.capabilities
    }

    /// The activation's host services.
    ///
    /// # Errors
    /// Fails when the host bound none.
    pub fn host(&self) -> Result<PluginHostServices> {
        self.context
            .native
            .host_service_port
            .clone()
            .ok_or_else(|| Error::unavailable("activation has no host services"))
    }

    /// A typed client for a declared requirement: a lease through the host,
    /// never a reference to the provider.
    ///
    /// # Errors
    /// Fails for an interface `declare()` did not require, or without host services.
    pub fn service<C: ServiceClient>(&self) -> Result<C> {
        let descriptor = C::DESCRIPTOR;
        let wanted = descriptor.interface_id.as_str();
        if !self
            .required
            .iter()
            .any(|required| required.interface_id.as_str() == wanted)
        {
            return Err(Error::failed(format!(
                "'{wanted}' is not required in declare()"
            )));
        }
        Ok(C::connect(self.host()?))
    }

    /// Spawn work owned by this activation on the host runtime; the host
    /// cancels and drains it with the activation.
    ///
    /// # Errors
    /// Fails when the host provided no task scope for this activation.
    pub fn spawn(
        &self,
        name: &str,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let tasks = self
            .tasks
            .as_ref()
            .ok_or_else(|| Error::unavailable("activation has no host task scope"))?;
        drop(tasks.spawn_with_name(name, future));
        Ok(())
    }

    /// Bind the host event bus this plugin's `Call::emit` publishes through.
    pub fn bind_events(&self, events: Arc<dyn EventEmitter>) {
        if let Ok(mut slot) = self.events.lock() {
            *slot = events;
        }
    }
}

/// The deactivation context handed to [`Plugin::deactivate`].
pub struct Deactivation {
    context: PluginLifecycleContext,
}

impl Deactivation {
    /// The plugin's id.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.context.native.plugin_id
    }

    /// The host lifecycle context.
    #[must_use]
    pub const fn native(&self) -> &NativeLifecycleContext {
        &self.context.native
    }
}

/// Drops events until a host event bus is bound (dylib event transport is
/// still being built); `Call::emit` then succeeds without subscribers.
struct Unbound;

impl EventEmitter for Unbound {
    fn emit(&self, kind: &PluginEventKind, _: EventDelivery, _: Vec<u8>) -> Result<()> {
        tracing::debug!(
            kind = kind.as_str(),
            "event dropped: no host event bus is bound"
        );
        Ok(())
    }
}

/// Forwards to whatever emitter is currently bound.
struct Bound(Arc<Mutex<Arc<dyn EventEmitter>>>);

impl EventEmitter for Bound {
    fn emit(
        &self,
        kind: &PluginEventKind,
        delivery: EventDelivery,
        payload: Vec<u8>,
    ) -> Result<()> {
        let current = self
            .0
            .lock()
            .map_err(|_| Error::failed("event emitter lock poisoned"))?
            .clone();
        current.emit(kind, delivery, payload)
    }
}

/// The host-facing instance [`crate::plugin!`] creates for a [`Plugin`].
pub struct DeclaredInstance<P> {
    plugin: Arc<P>,
    declarations: Arc<Declarations<P>>,
    events: Arc<Mutex<Arc<dyn EventEmitter>>>,
}

impl<P: Plugin> DeclaredInstance<P> {
    /// Wrap a fresh plugin value.
    ///
    /// # Errors
    /// Returns the first mismatch between `declare()` and the manifest.
    pub fn new(plugin: P, manifest: &str, services: &[ServiceInterfaceDescriptor]) -> Result<Self> {
        let mut declarations = Declarations::default();
        P::declare(&mut declarations);
        declarations.validate(manifest, services)?;
        Ok(Self {
            plugin: Arc::new(plugin),
            declarations: Arc::new(declarations),
            events: Arc::new(Mutex::new(Arc::new(Unbound))),
        })
    }

    /// The plugin value (tests and in-process hosts).
    #[must_use]
    pub const fn plugin(&self) -> &Arc<P> {
        &self.plugin
    }

    /// The validated declarations.
    #[must_use]
    pub fn declarations(&self) -> &Declarations<P> {
        &self.declarations
    }

    fn emitter(&self) -> Arc<dyn EventEmitter> {
        Arc::new(Bound(self.events.clone()))
    }

    /// The base call for invocations that carry no host context.
    fn base_call(&self) -> Call {
        Call::detached().with_events(self.emitter())
    }

    fn provided(&self, interface: &str) -> Option<&Provided<P>> {
        self.declarations
            .provided
            .iter()
            .find(|entry| entry.descriptor.interface_id.as_str() == interface)
    }

    fn activation(
        &self,
        context: PluginLifecycleContext,
        tasks: Option<crate::HostAsyncHandle>,
    ) -> Activation {
        Activation {
            context,
            tasks,
            required: self.declarations.required.clone(),
            events: self.events.clone(),
        }
    }
}

fn unsupported_interface(interface: &str) -> ServiceResponse {
    ServiceResponse::error(
        "unsupported_service_interface",
        format!("interface '{interface}' is not provided"),
    )
}

impl<P: Plugin> PluginInstance for DeclaredInstance<P> {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let mut cx = self.activation(context, None);
            self.plugin.activate(&mut cx).await
        })
    }

    fn activate_with_async(
        &self,
        context: PluginLifecycleContext,
        runtime: crate::HostAsyncHandle,
    ) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let mut cx = self.activation(context, Some(runtime));
            self.plugin.activate(&mut cx).await
        })
    }

    fn deactivate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let mut cx = Deactivation { context };
            self.plugin.deactivate(&mut cx).await
        })
    }

    fn command(
        &self,
        call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async move {
            let Some(command) = self
                .declarations
                .commands
                .iter()
                .find(|command| command.name == context.command)
            else {
                return Err(crate::PluginCommandError::unknown_command(&context.command)).into();
            };
            let cx = Call::from_invocation(call).with_events(self.emitter());
            (command.run)(&self.plugin, cx, context).await.into()
        })
    }

    fn invoke(
        &self,
        call: PluginCallContext,
        context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async move {
            let interface = context.request.service.interface_id.clone();
            let Some(entry) = self.provided(&interface) else {
                return Ok(unsupported_interface(&interface));
            };
            let cx = Call::from_service(call, &context).with_events(self.emitter());
            (entry.dispatch)(&self.plugin, cx, context).await
        })
    }

    fn invoke_streaming(
        &self,
        call: PluginCallContext,
        context: NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async move {
            let interface = context.service.request.service.interface_id.clone();
            let Some(entry) = self.provided(&interface) else {
                return Ok(unsupported_interface(&interface));
            };
            let cx = Call::from_service(call, &context.service).with_events(self.emitter());
            (entry.dispatch_streaming)(&self.plugin, cx, context).await
        })
    }

    fn event(&self, event: PluginEvent) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            match &self.declarations.events {
                Some(handler) => handler(&self.plugin, event).await,
                None => Ok(()),
            }
        })
    }

    fn extension(
        &self,
        _call: PluginCallContext,
        contract: String,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            let contribution = self
                .declarations
                .contributions
                .iter()
                .find(|contribution| contribution.id == contract)
                .ok_or_else(|| Error::unavailable(format!("no contribution to '{contract}'")))?;
            let cx = self.base_call();
            (contribution.dispatch)(&self.plugin, &cx, &payload).await
        })
    }

    fn portable_services(&self) -> PluginResult<Vec<Arc<dyn PortableServiceProvider>>> {
        Ok(self
            .declarations
            .provided
            .iter()
            .map(|entry| (entry.portable)(self.plugin.clone(), self.base_call()))
            .collect())
    }

    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        self.provided(interface)
            .map(|entry| entry.descriptor.contract)
    }

    fn register_typed_services(&self, registry: &mut TypedServiceRegistry) -> PluginResult<()> {
        for entry in &self.declarations.provided {
            (entry.register)(self.plugin.clone(), self.base_call(), registry)
                .map_err(|error| Error::failed(error.to_string()))?;
        }
        Ok(())
    }
}

/// Build the definition [`crate::plugin!`] emits.
///
/// # Errors
/// Fails when the manifest lacks identity/roles.
pub fn definition<P: Plugin>(
    manifest: &'static str,
    services: &'static [ServiceInterfaceDescriptor],
    create: fn(&PluginCreateContext) -> Result<P>,
) -> Result<crate::definition::PluginDefinition> {
    let metadata = super::metadata(manifest, services)?;
    let definition =
        crate::definition::PluginDefinition::new(metadata, move |scope: PluginCreateContext| {
            let plugin = create(&scope)?;
            Ok(Arc::new(DeclaredInstance::new(plugin, manifest, services)?)
                as Arc<dyn PluginInstance>)
        });
    definition.validate()?;
    Ok(definition)
}

/// Declare a [`Plugin`] type. Emits `plugin_definition()`, and the single
/// dynamic-library entry symbol when built by the packager.
///
/// ```ignore
/// bmux_plugin_sdk::plugin!(Counter);                       // Default construction
/// bmux_plugin_sdk::plugin!(Counter, create = Counter::new); // fn(&PluginCreateContext) -> Result<Counter>
/// ```
///
/// The plugin crate's `declaration` module (generated by `schema!`/
/// `plugin_manifest!`) supplies the manifest and contract descriptors; pass
/// `declaration = path::to::module` when it lives elsewhere.
///
/// The packager builds the crate with `cargo rustc --crate-type cdylib
/// --features dynamic-<role>` and `--cfg bmux_plugin_dylib`; only then does
/// the expansion export the entry symbol, for exactly that role. Plugin
/// crates declare the `dynamic-backend`/`dynamic-frontend` features.
// `crate::declaration` deliberately names the *calling* crate's generated
// module: `plugin!` binds a plugin to its own manifest.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! plugin {
    ($plugin:ty $(,)?) => {
        $crate::plugin!(
            $plugin,
            create =
                |_| ::core::result::Result::Ok(<$plugin as ::core::default::Default>::default()),
            declaration = crate::declaration
        );
    };
    ($plugin:ty, declaration = $declaration:path $(,)?) => {
        $crate::plugin!(
            $plugin,
            create =
                |_| ::core::result::Result::Ok(<$plugin as ::core::default::Default>::default()),
            declaration = $declaration
        );
    };
    ($plugin:ty, create = $create:expr $(,)?) => {
        $crate::plugin!($plugin, create = $create, declaration = crate::declaration);
    };
    ($plugin:ty, create = $create:expr, declaration = $declaration:path $(,)?) => {
        /// This crate's plugin definition (inert until the host creates an instance).
        ///
        /// # Panics
        /// Panics when the generated manifest lacks identity or roles, which
        /// `schema!`/`plugin_manifest!` never produce.
        #[must_use]
        pub fn plugin_definition() -> $crate::definition::PluginDefinition {
            use $declaration as __declaration;
            $crate::author::definition::<$plugin>(
                __declaration::PLUGIN_MANIFEST,
                __declaration::SERVICE_CONTRACTS,
                $create,
            )
            .expect("generated manifest declares identity and roles")
        }
        $crate::__plugin_dylib_entry!();
    };
}

/// The packaged entry symbol for the role the packager selected.
///
/// `--cfg bmux_plugin_dylib` is evaluated here, in the SDK (the packager sets
/// it for every crate of the build), so plugin crates never name the cfg;
/// they only declare the `dynamic-backend`/`dynamic-frontend` role features.
#[cfg(bmux_plugin_dylib)]
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_dylib_entry {
    () => {
        #[cfg(all(feature = "dynamic-backend", feature = "dynamic-frontend"))]
        ::core::compile_error!(
            "a dynamic library packages one component: enable either `dynamic-backend` or `dynamic-frontend`"
        );
        #[cfg(all(feature = "dynamic-backend", not(feature = "dynamic-frontend")))]
        $crate::__plugin_dylib_export!(Backend);
        #[cfg(all(feature = "dynamic-frontend", not(feature = "dynamic-backend")))]
        $crate::__plugin_dylib_export!(Frontend);
    };
}

/// Without `--cfg bmux_plugin_dylib` (every workspace and bundled build)
/// nothing is exported.
#[cfg(not(bmux_plugin_dylib))]
#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_dylib_entry {
    () => {};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_dylib_export {
    ($role:ident) => {
        const _: () = {
            fn __bmux_packaged_definition() -> $crate::definition::PluginDefinition {
                $crate::author::packaged(plugin_definition(), $crate::ComponentRole::$role)
            }
            $crate::export_plugin_entrypoint!(__bmux_packaged_definition);
            // The packager's build label, retained in the artifact bytes (the
            // byte hash is the artifact identity; this is only a label).
            #[used]
            static __BMUX_PACKAGING_IDENTITY: &str =
                match ::core::option_env!("BMUX_PLUGIN_ARTIFACT_ID") {
                    ::core::option::Option::Some(label) => label,
                    ::core::option::Option::None => "unlabelled",
                };
        };
    };
}

/// Restrict a definition to the component a dynamic library packages.
#[doc(hidden)]
#[must_use]
pub fn packaged(
    mut definition: crate::definition::PluginDefinition,
    role: crate::ComponentRole,
) -> crate::definition::PluginDefinition {
    definition
        .metadata
        .roles
        .retain(|candidate| *candidate == role);
    definition
}
