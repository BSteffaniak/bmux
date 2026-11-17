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
    BoxFuture, Call, CommandArgs, CommandDecl, ContractEvent, Error, EventClient, EventDelivery,
    EventEmitter, Extends, Provides, RequiredInterface, Result,
};
use crate::definition::{
    CommandOutcome, LoadedInstanceId, PluginCallContext, PluginCreateContext, PluginFuture,
    PluginHostServices, PluginInstance, PluginLifecycleContext, PluginResult,
};
use crate::{
    ContributionRegistrar, HostAsyncHandle, NativeCommandContext, NativeLifecycleContext,
    NativeServiceContext, NativeStreamingServiceContext, PluginCallbackContext, PluginEvent,
    PluginEventKind, PortableServiceProvider, ServiceInterfaceDescriptor, ServiceResponse,
    TypedServiceRegistrationContext, TypedServiceRegistry,
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

    /// Call a bundled provider registered in a typed service handle
    /// directly, with no serialization (the bundled fast path).
    ///
    /// # Errors
    /// The handle holds a different provider type, or the client has no
    /// direct path.
    fn direct(handle: &crate::TypedServiceHandle) -> crate::Result<Self> {
        let _ = handle;
        Err(crate::PluginError::UnsupportedHostOperation {
            operation: "direct typed client",
        })
    }
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

/// A handler for service interfaces a plugin serves without generated
/// bindings: `async fn(&self, &Call, NativeServiceContext) -> Result<ServiceResponse>`
/// (or the streaming context).
pub trait NativeServiceHandler<'a, P: 'a, C>: Send + Sync {
    type Future: Future<Output = Result<ServiceResponse>> + Send + 'a;
    fn call(&self, plugin: &'a P, cx: &'a Call, context: C) -> Self::Future;
}

impl<'a, P: 'a, C, F, Fut> NativeServiceHandler<'a, P, C> for F
where
    F: Fn(&'a P, &'a Call, C) -> Fut + Send + Sync,
    Fut: Future<Output = Result<ServiceResponse>> + Send + 'a,
{
    type Future = Fut;
    fn call(&self, plugin: &'a P, cx: &'a Call, context: C) -> Fut {
        self(plugin, cx, context)
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
type NativeServiceFn<P> = Box<
    dyn for<'a> Fn(&'a P, Call, NativeServiceContext) -> BoxFuture<'a, Result<ServiceResponse>>
        + Send
        + Sync,
>;
type NativeStreamingFn<P> = Box<
    dyn for<'a> Fn(
            &'a P,
            Call,
            NativeStreamingServiceContext,
        ) -> BoxFuture<'a, Result<ServiceResponse>>
        + Send
        + Sync,
>;
/// A synchronous service body: the raw context in, the response out.
pub type BlockingServiceFn<P> = fn(&P, NativeServiceContext) -> ServiceResponse;
/// A synchronous streaming service body.
pub type BlockingStreamingFn<P> = fn(&P, NativeStreamingServiceContext) -> ServiceResponse;
/// A synchronous command body returning its exit code.
pub type BlockingCommandFn<P> = fn(&P, NativeCommandContext) -> Result<i32>;
/// A synchronous host-event body.
pub type BlockingEventFn<P> = fn(&P, PluginEvent) -> Result<()>;
/// A synchronous activation body, with the activation's task scope when the
/// host provides one.
pub type BlockingActivateFn<P> =
    fn(&P, NativeLifecycleContext, Option<HostAsyncHandle>) -> Result<()>;
/// A synchronous deactivation body.
pub type BlockingDeactivateFn<P> = fn(&P, NativeLifecycleContext) -> Result<()>;
/// Registers typed handles for interfaces whose providers are not generated
/// author bindings, with the host's registration context.
pub type TypedServicesFn<P> =
    fn(&P, TypedServiceRegistrationContext<'_>, &mut TypedServiceRegistry) -> Result<()>;
/// Registers activation-time contributions.
pub type RegistrationsFn<P> = fn(&P, &mut ContributionRegistrar) -> crate::Result<()>;

struct Provided<P> {
    descriptor: ServiceInterfaceDescriptor,
    dispatch: Dispatch<P>,
    dispatch_streaming: DispatchStreaming<P>,
    register: Register<P>,
    portable: Portable<P>,
}

enum CommandRun<P> {
    Async(CommandFn<P>),
    Blocking(BlockingCommandFn<P>),
}

struct Command<P> {
    name: &'static str,
    run: CommandRun<P>,
}

/// Service interfaces served without generated bindings.
enum NativeServices<P> {
    Async {
        unary: NativeServiceFn<P>,
        streaming: Option<NativeStreamingFn<P>>,
    },
    Blocking {
        unary: BlockingServiceFn<P>,
        streaming: Option<BlockingStreamingFn<P>>,
    },
}

/// Host events: async, or a synchronous body run under the event's entry.
enum Events<P> {
    Async(EventFn<P>),
    Blocking(BlockingEventFn<P>),
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
    events: Option<Events<P>>,
    emitted: Vec<(PluginEventKind, EventDelivery)>,
    subscribed: Vec<PluginEventKind>,
    native_services: Option<NativeServices<P>>,
    blocking_activate: Option<BlockingActivateFn<P>>,
    blocking_deactivate: Option<BlockingDeactivateFn<P>>,
    typed_services: Option<TypedServicesFn<P>>,
    registrations: Option<RegistrationsFn<P>>,
}

impl<P> Default for Declarations<P> {
    fn default() -> Self {
        Self {
            provided: Vec::new(),
            required: Vec::new(),
            commands: Vec::new(),
            contributions: Vec::new(),
            events: None,
            emitted: Vec::new(),
            subscribed: Vec::new(),
            native_services: None,
            blocking_activate: None,
            blocking_deactivate: None,
            typed_services: None,
            registrations: None,
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
            run: CommandRun::Async(Box::new(move |plugin, call, context| {
                let args = A::parse(&context.arguments);
                Box::pin(async move {
                    handler.call(plugin, &call, args?).await?;
                    Ok(crate::EXIT_OK)
                })
            })),
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
            run: CommandRun::Async(Box::new(move |plugin, call, context| {
                Box::pin(async move { handler.call(plugin, &call, context).await })
            })),
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

    /// Publish a BPDL contract event: `emits::<counter_events::v1::Changed>()`.
    /// The host registers its channel for the activation before `activate`
    /// runs, so consumers can subscribe before the first `Call::emit`.
    pub fn emits<E: ContractEvent>(&mut self) -> &mut Self {
        if !self.emitted.iter().any(|(kind, _)| *kind == E::KIND) {
            self.emitted.push((E::KIND, E::DELIVERY));
        }
        self
    }

    /// Subscribe to a BPDL events interface:
    /// `subscribe::<counter_events::v1::Client>()`. `Activation::events`
    /// refuses undeclared subscriptions.
    pub fn subscribe<C: EventClient>(&mut self) -> &mut Self {
        if !self.subscribed.contains(&C::KIND) {
            self.subscribed.push(C::KIND);
        }
        self
    }

    /// Receive host events delivered to this plugin.
    pub fn on_event<F>(&mut self, handler: F) -> &mut Self
    where
        F: for<'a> EventHandler<'a, P> + Copy + 'static,
    {
        self.events = Some(Events::Async(Box::new(move |plugin, event| {
            Box::pin(async move { handler.call(plugin, event).await })
        })));
        self
    }

    /// Serve every service the manifest declares that is not
    /// [`provide`](Self::provide)d with generated bindings through one
    /// handler over the raw service context. The handler owns checked
    /// framing. Prefer generated providers.
    pub fn native_services<F>(&mut self, handler: F) -> &mut Self
    where
        F: for<'a> NativeServiceHandler<'a, P, NativeServiceContext> + Copy + 'static,
    {
        self.native_services = Some(NativeServices::Async {
            unary: Box::new(move |plugin, call, context| {
                Box::pin(async move { handler.call(plugin, &call, context).await })
            }),
            streaming: None,
        });
        self
    }

    /// Streaming invocations of [`native_services`](Self::native_services);
    /// without it, a streaming call runs the unary handler.
    pub fn native_streaming_services<F>(&mut self, handler: F) -> &mut Self
    where
        F: for<'a> NativeServiceHandler<'a, P, NativeStreamingServiceContext> + Copy + 'static,
    {
        if let Some(NativeServices::Async { streaming, .. }) = &mut self.native_services {
            *streaming = Some(Box::new(move |plugin, call, context| {
                Box::pin(async move { handler.call(plugin, &call, context).await })
            }));
        }
        self
    }

    /// Like [`native_services`](Self::native_services), with synchronous
    /// bodies (`streaming` defaults to the unary body). Each call runs on the
    /// host's blocking pool inside the invocation's callback entry; checked
    /// framing is decoded with the generated contract first and a successful
    /// checked response is framed again.
    pub fn blocking_services(
        &mut self,
        unary: BlockingServiceFn<P>,
        streaming: Option<BlockingStreamingFn<P>>,
    ) -> &mut Self {
        self.native_services = Some(NativeServices::Blocking { unary, streaming });
        self
    }

    /// A command `plugin.toml` declares by hand, with a synchronous body run
    /// on the host's blocking pool inside the command's callback entry.
    /// Command-outcome metadata the body records is returned with it.
    pub fn blocking_command(
        &mut self,
        name: &'static str,
        handler: BlockingCommandFn<P>,
    ) -> &mut Self {
        self.commands.push(Command {
            name,
            run: CommandRun::Blocking(handler),
        });
        self
    }

    /// Host events with a synchronous body run inside the event's callback
    /// entry.
    pub fn blocking_events(&mut self, handler: BlockingEventFn<P>) -> &mut Self {
        self.events = Some(Events::Blocking(handler));
        self
    }

    /// Synchronous activation work, run on the host's blocking pool inside
    /// the activation's callback entry after [`Plugin::activate`]. Its
    /// lifecycle context carries the activation's host-supplied capabilities.
    pub fn blocking_activate(&mut self, handler: BlockingActivateFn<P>) -> &mut Self {
        self.blocking_activate = Some(handler);
        self
    }

    /// Synchronous deactivation work, run inside the deactivation's callback
    /// entry after [`Plugin::deactivate`].
    pub fn blocking_deactivate(&mut self, handler: BlockingDeactivateFn<P>) -> &mut Self {
        self.blocking_deactivate = Some(handler);
        self
    }

    /// Register typed handles for services that are not generated providers,
    /// with the host's registration context.
    pub fn typed_services(&mut self, register: TypedServicesFn<P>) -> &mut Self {
        self.typed_services = Some(register);
        self
    }

    /// Register activation-time contributions (state, readiness, frontend).
    pub fn registrations(&mut self, register: RegistrationsFn<P>) -> &mut Self {
        self.registrations = Some(register);
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
        if let Some(missing) = services.iter().find(|service| {
            self.native_services.is_none() && !provided.contains(service.interface_id.as_str())
        }) {
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
        let mut points = BTreeSet::new();
        for contribution in &self.contributions {
            if !points.insert(contribution.id) {
                return Err(Error::failed(format!(
                    "extension point '{}' is extended twice",
                    contribution.id
                )));
            }
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

    /// Event channels this plugin publishes, with their delivery.
    #[must_use]
    pub fn emitted(&self) -> &[(PluginEventKind, EventDelivery)] {
        &self.emitted
    }

    /// Event channels this plugin subscribes to.
    #[must_use]
    pub fn subscribed(&self) -> &[PluginEventKind] {
        &self.subscribed
    }
}

/// The activation context handed to [`Plugin::activate`].
pub struct Activation {
    context: PluginLifecycleContext,
    tasks: Option<crate::HostAsyncHandle>,
    required: Vec<ServiceInterfaceDescriptor>,
    subscribed: Vec<PluginEventKind>,
    emitter: Arc<Mutex<Arc<dyn EventEmitter>>>,
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

    /// A typed client for a declared event subscription; its
    /// `subscribe_<event>()` streams are owned by this activation.
    ///
    /// # Errors
    /// Fails for an events interface `declare()` did not subscribe to, or
    /// without host services.
    pub fn events<C: EventClient>(&self) -> Result<C> {
        if !self.subscribed.contains(&C::KIND) {
            return Err(Error::failed(format!(
                "'{}' is not subscribed in declare()",
                C::KIND.as_str()
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

    /// The whole host lifecycle context (scope, activation id, callback
    /// execution), for plugins that bind host ports to this activation.
    #[must_use]
    pub const fn lifecycle(&self) -> &PluginLifecycleContext {
        &self.context
    }

    /// The activation's task scope, when the host provides one.
    #[must_use]
    pub const fn tasks(&self) -> Option<&crate::HostAsyncHandle> {
        self.tasks.as_ref()
    }

    /// Bind the host event bus this plugin's `Call::emit` publishes through.
    pub fn bind_events(&self, events: Arc<dyn EventEmitter>) {
        if let Ok(mut slot) = self.emitter.lock() {
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

    /// The whole host lifecycle context of this deactivation.
    #[must_use]
    pub const fn lifecycle(&self) -> &PluginLifecycleContext {
        &self.context
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

/// Publishes through the activation's host services: the host event bus in
/// both packaging modes. Declared channels are registered at activation; a
/// channel first emitted without a declaration is registered on first use.
struct HostEvents {
    host: PluginHostServices,
    registered: Mutex<BTreeSet<String>>,
}

impl HostEvents {
    /// Register every declared channel for the activation.
    ///
    /// Returns `None` when the host offers no publication route (events stay
    /// unbound, as before).
    fn bind(
        host: PluginHostServices,
        declared: &[(PluginEventKind, EventDelivery)],
    ) -> Result<Option<Self>> {
        if !host.publishes_events() {
            return Ok(None);
        }
        let mut registered = BTreeSet::new();
        for (kind, delivery) in declared {
            match host.register_event_channel(kind.as_str(), *delivery == EventDelivery::State) {
                Ok(()) => {
                    registered.insert(kind.as_str().to_owned());
                }
                Err(crate::PluginError::UnsupportedHostOperation { .. }) => return Ok(None),
                Err(error) => {
                    return Err(Error::failed(format!(
                        "registering event channel '{}': {error}",
                        kind.as_str()
                    )));
                }
            }
        }
        Ok(Some(Self {
            host,
            registered: Mutex::new(registered),
        }))
    }
}

impl EventEmitter for HostEvents {
    fn emit(
        &self,
        kind: &PluginEventKind,
        delivery: EventDelivery,
        payload: Vec<u8>,
    ) -> Result<()> {
        let failed = |error: crate::PluginError| {
            Error::failed(format!("publishing '{}': {error}", kind.as_str()))
        };
        {
            let mut registered = self
                .registered
                .lock()
                .map_err(|_| Error::failed("event registration lock poisoned"))?;
            if !registered.contains(kind.as_str()) {
                match self
                    .host
                    .register_event_channel(kind.as_str(), delivery == EventDelivery::State)
                {
                    Ok(()) => {}
                    Err(crate::PluginError::UnsupportedHostOperation { .. }) => {
                        return Unbound.emit(kind, delivery, payload);
                    }
                    Err(error) => return Err(failed(error)),
                }
                registered.insert(kind.as_str().to_owned());
            }
        }
        self.host
            .publish_event(kind.as_str(), &payload)
            .map_err(failed)
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
    services: &'static [ServiceInterfaceDescriptor],
    instance: Option<LoadedInstanceId>,
}

impl<P: Plugin> DeclaredInstance<P> {
    /// Wrap a fresh plugin value.
    ///
    /// # Errors
    /// Returns the first mismatch between `declare()` and the manifest.
    pub fn new(
        plugin: P,
        manifest: &str,
        services: &'static [ServiceInterfaceDescriptor],
    ) -> Result<Self> {
        let mut declarations = Declarations::default();
        P::declare(&mut declarations);
        declarations.validate(manifest, services)?;
        Ok(Self {
            plugin: Arc::new(plugin),
            declarations: Arc::new(declarations),
            events: Arc::new(Mutex::new(Arc::new(Unbound))),
            services,
            instance: None,
        })
    }

    /// Bind the host-issued instance this value serves. Synchronous bodies
    /// enter their callbacks on it; without it they are refused.
    #[must_use]
    pub const fn for_instance(mut self, instance: LoadedInstanceId) -> Self {
        self.instance = Some(instance);
        self
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
    ) -> Result<Activation> {
        // Before the plugin's own activation runs: its declared channels
        // exist, and every `Call::emit` reaches the host event bus.
        if let Some(host) = context.native.host_service_port.clone()
            && let Some(events) = HostEvents::bind(host, &self.declarations.emitted)?
            && let Ok(mut slot) = self.events.lock()
        {
            *slot = Arc::new(events);
        }
        Ok(Activation {
            context,
            tasks,
            required: self.declarations.required.clone(),
            subscribed: self.declarations.subscribed.clone(),
            emitter: self.events.clone(),
        })
    }

    async fn run_activation(&self, mut cx: Activation) -> PluginResult<()> {
        let result = self.plugin.activate(&mut cx).await;
        if result.is_err() {
            self.unbind_events();
        }
        result
    }

    fn unbind_events(&self) {
        if let Ok(mut slot) = self.events.lock() {
            *slot = Arc::new(Unbound);
        }
    }

    /// Run a synchronous body on the host's blocking pool inside this
    /// invocation's callback entry, on this exact instance.
    fn entered<T: Send + 'static>(
        &self,
        execution: Option<PluginCallbackContext>,
        work: impl FnOnce(&P) -> Result<T> + Send + 'static,
    ) -> PluginFuture<'static, Result<T>> {
        let plugin = self.plugin.clone();
        let instance = self.instance;
        Box::pin(async move {
            let execution = execution
                .ok_or_else(|| Error::unavailable("callback execution authority missing"))?;
            let instance =
                instance.ok_or_else(|| Error::unavailable("callback instance binding missing"))?;
            let entry = execution.enter_adapter_entry(instance)?;
            let output = Arc::new(Mutex::new(None));
            let writer = output.clone();
            execution
                .execute_entered(
                    instance,
                    entry,
                    Box::new(move || {
                        let result = work(&plugin);
                        *writer
                            .lock()
                            .map_err(|_| Error::failed("callback result poisoned"))? = Some(result);
                        Ok(())
                    }),
                )
                .await?;
            let result = output
                .lock()
                .map_err(|_| Error::failed("callback result poisoned"))?
                .take();
            result.ok_or_else(|| Error::failed("callback settled without result"))?
        })
    }

    /// Run a blocking activation or deactivation body.
    fn lifecycle(
        &self,
        context: &PluginLifecycleContext,
        work: impl FnOnce(&P, NativeLifecycleContext) -> Result<()> + Send + 'static,
    ) -> PluginFuture<'static, Result<()>> {
        let mut native = context.native.clone();
        native.capabilities = context.capabilities.clone();
        self.entered(context.callback_execution.clone(), move |plugin| {
            work(plugin, native)
        })
    }

    async fn activate_declared(
        &self,
        context: PluginLifecycleContext,
        tasks: Option<crate::HostAsyncHandle>,
    ) -> PluginResult<()> {
        let blocking = self.declarations.blocking_activate.map(|activate| {
            let runtime = tasks.clone();
            self.lifecycle(&context, move |plugin, native| {
                activate(plugin, native, runtime)
            })
        });
        self.run_activation(self.activation(context, tasks)?).await?;
        let result = match blocking {
            Some(work) => work.await,
            None => Ok(()),
        };
        if result.is_err() {
            self.unbind_events();
        }
        result
    }

    /// The generated contract of a service served without generated bindings.
    fn native_contract(&self, interface: &str) -> Option<&'static [u8]> {
        self.services
            .iter()
            .find(|service| service.interface_id.as_str() == interface)
            .map(|service| service.contract)
            .filter(|contract| !contract.is_empty())
    }

    /// Decode checked framing for a blocking body, as the generated
    /// providers do for their own bodies.
    fn checked(
        &self,
        mut context: NativeServiceContext,
    ) -> PluginResult<(NativeServiceContext, bool)> {
        let checked = crate::service_contract::is_checked(&context.request.payload);
        if checked {
            let interface = crate::service_contract::interface_id_for(
                &context.request.service.interface_id,
                &context.request.payload,
            )
            .map_err(|error| Error::failed(error.to_string()))?;
            let evidence = self.native_contract(interface).ok_or_else(|| {
                Error::failed("provider has no generated contract for checked interface")
            })?;
            context.request.payload = crate::service_contract::decode_for(
                &context.request.service.interface_id,
                evidence,
                &context.request.payload,
            )
            .map_err(|error| Error::failed(error.to_string()))?
            .to_vec();
        }
        Ok((context, checked))
    }
}

fn checked_response(mut response: ServiceResponse, checked: bool) -> ServiceResponse {
    if checked && response.error.is_none() {
        response.payload = crate::service_contract::encode_response(&response.payload);
    }
    response
}

fn cancelled(call: &PluginCallContext) -> PluginResult<()> {
    if call.cancellation.is_cancelled() {
        return Err(Error::unavailable("invocation cancelled"));
    }
    Ok(())
}

fn unsupported_interface(interface: &str) -> ServiceResponse {
    ServiceResponse::error(
        "unsupported_service_interface",
        format!("interface '{interface}' is not provided"),
    )
}

impl<P: Plugin> PluginInstance for DeclaredInstance<P> {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(self.activate_declared(context, None))
    }

    fn activate_with_async(
        &self,
        context: PluginLifecycleContext,
        runtime: crate::HostAsyncHandle,
    ) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(self.activate_declared(context, Some(runtime)))
    }

    fn deactivate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let blocking = self
                .declarations
                .blocking_deactivate
                .map(|deactivate| self.lifecycle(&context, deactivate));
            let mut cx = Deactivation { context };
            let result = match self.plugin.deactivate(&mut cx).await {
                Ok(()) => match blocking {
                    Some(work) => work.await,
                    None => Ok(()),
                },
                Err(error) => Err(error),
            };
            // The activation's channels go with it.
            self.unbind_events();
            result
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
            match &command.run {
                CommandRun::Async(run) => {
                    let cx = Call::from_invocation(call).with_events(self.emitter());
                    run(&self.plugin, cx, context).await.into()
                }
                CommandRun::Blocking(run) => {
                    if let Err(error) = cancelled(&call) {
                        return Err(error).into();
                    }
                    let run = *run;
                    self.entered(call.callback_execution, move |plugin| {
                        Ok(CommandOutcome::capture(|| run(plugin, context)))
                    })
                    .await
                    .unwrap_or_else(|error| Err(error).into())
                }
            }
        })
    }

    fn invoke(
        &self,
        call: PluginCallContext,
        context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async move {
            let interface = context.request.service.interface_id.clone();
            if let Some(entry) = self.provided(&interface) {
                let cx = Call::from_service(call, &context).with_events(self.emitter());
                return (entry.dispatch)(&self.plugin, cx, context).await;
            }
            match &self.declarations.native_services {
                None => Ok(unsupported_interface(&interface)),
                Some(NativeServices::Async { unary, .. }) => {
                    let cx = Call::from_service(call, &context).with_events(self.emitter());
                    unary(&self.plugin, cx, context).await
                }
                Some(NativeServices::Blocking { unary, .. }) => {
                    cancelled(&call)?;
                    let (context, checked) = self.checked(context)?;
                    let unary = *unary;
                    self.entered(call.callback_execution, move |plugin| {
                        Ok(checked_response(unary(plugin, context), checked))
                    })
                    .await
                }
            }
        })
    }

    fn invoke_streaming(
        &self,
        call: PluginCallContext,
        context: NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async move {
            let interface = context.service.request.service.interface_id.clone();
            if let Some(entry) = self.provided(&interface) {
                let cx = Call::from_service(call, &context.service).with_events(self.emitter());
                return (entry.dispatch_streaming)(&self.plugin, cx, context).await;
            }
            match &self.declarations.native_services {
                None => Ok(unsupported_interface(&interface)),
                Some(NativeServices::Async { unary, streaming }) => {
                    let cx = Call::from_service(call, &context.service).with_events(self.emitter());
                    match streaming {
                        Some(streaming) => streaming(&self.plugin, cx, context).await,
                        None => unary(&self.plugin, cx, context.service).await,
                    }
                }
                Some(NativeServices::Blocking { unary, streaming }) => {
                    cancelled(&call)?;
                    let NativeStreamingServiceContext { service, events } = context;
                    let (service, checked) = self.checked(service)?;
                    let (unary, streaming) = (*unary, *streaming);
                    self.entered(call.callback_execution, move |plugin| {
                        let response = match streaming {
                            Some(streaming) => {
                                streaming(plugin, NativeStreamingServiceContext { service, events })
                            }
                            None => unary(plugin, service),
                        };
                        Ok(checked_response(response, checked))
                    })
                    .await
                }
            }
        })
    }

    fn event_scoped(
        &self,
        call: PluginCallContext,
        event: PluginEvent,
    ) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            match &self.declarations.events {
                Some(Events::Async(handler)) => handler(&self.plugin, event).await,
                Some(Events::Blocking(handler)) => {
                    let handler = *handler;
                    self.entered(call.callback_execution, move |plugin| {
                        handler(plugin, event)
                    })
                    .await
                }
                None => Ok(()),
            }
        })
    }

    fn event(&self, event: PluginEvent) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            match &self.declarations.events {
                Some(Events::Async(handler)) => handler(&self.plugin, event).await,
                Some(Events::Blocking(_)) => Err(Error::unavailable(
                    "event requires scoped callback authority",
                )),
                None => Ok(()),
            }
        })
    }

    fn extension(
        &self,
        call: PluginCallContext,
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
            let cx = Call::from_invocation(call).with_events(self.emitter());
            (contribution.dispatch)(&self.plugin, &cx, &payload).await
        })
    }

    /// Each `extend::<E>(data)` as an extension contribution: the host reads
    /// the encoded data and routes behavior calls back through `extension`.
    fn register_contributions(
        &self,
        registrar: &mut crate::ContributionRegistrar,
    ) -> PluginResult<()> {
        for (extension_point, data) in self.declarations.contributions() {
            registrar
                .register(crate::PluginContribution::Extension {
                    id: format!("extension:{extension_point}"),
                    extension_point: extension_point.to_owned(),
                    payload: data.to_vec(),
                })
                .map_err(Error::failed)?;
        }
        match self.declarations.registrations {
            Some(register) => {
                register(&self.plugin, registrar).map_err(|error| Error::failed(error.to_string()))
            }
            None => Ok(()),
        }
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
        match self.provided(interface) {
            Some(entry) => Some(entry.descriptor.contract),
            None if self.declarations.native_services.is_some() => self.native_contract(interface),
            None => None,
        }
    }

    fn register_typed_services_with_context(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) -> PluginResult<()> {
        self.register_typed_services(registry)?;
        match self.declarations.typed_services {
            Some(register) => register(&self.plugin, context, registry),
            None => Ok(()),
        }
    }

    fn register_typed_services(&self, registry: &mut TypedServiceRegistry) -> PluginResult<()> {
        for entry in &self.declarations.provided {
            (entry.register)(self.plugin.clone(), self.base_call(), registry)
                .map_err(|error| Error::failed(error.to_string()))?;
        }
        Ok(())
    }

}

/// A plugin crate as test harnesses load it, emitted by [`crate::plugin!`]
/// as `plugin_crate()`: the bundled definition and the crate manifest the
/// packager builds as a dynamic library (`dynamic-backend`).
#[derive(Debug, Clone, Copy)]
pub struct PluginCrate {
    definition: fn() -> crate::definition::PluginDefinition,
    manifest: &'static str,
}

impl PluginCrate {
    #[must_use]
    pub const fn new(
        definition: fn() -> crate::definition::PluginDefinition,
        manifest: &'static str,
    ) -> Self {
        Self {
            definition,
            manifest,
        }
    }

    /// The bundled definition.
    #[must_use]
    pub fn definition(&self) -> crate::definition::PluginDefinition {
        (self.definition)()
    }

    /// The crate's `Cargo.toml`.
    #[must_use]
    pub const fn manifest(&self) -> &'static str {
        self.manifest
    }
}

/// Build the definition [`crate::plugin!`] emits.
///
/// # Errors
/// Fails when the manifest lacks identity/roles.
pub fn definition<P: Plugin>(
    manifest: &'static str,
    services: &'static [ServiceInterfaceDescriptor],
    create: impl Fn(&PluginCreateContext) -> Result<P> + Send + Sync + 'static,
) -> Result<crate::definition::PluginDefinition> {
    let metadata = super::metadata(manifest, services)?;
    let definition =
        crate::definition::PluginDefinition::new(metadata, move |scope: PluginCreateContext| {
            let plugin = create(&scope)?;
            Ok(Arc::new(
                DeclaredInstance::new(plugin, manifest, services)?.for_instance(scope.instance),
            ) as Arc<dyn PluginInstance>)
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
        /// This crate as a test harness loads it: its definition (bundled)
        /// and the manifest the packager builds as a dynamic library.
        #[must_use]
        pub fn plugin_crate() -> $crate::author::PluginCrate {
            $crate::author::PluginCrate::new(
                plugin_definition,
                ::core::concat!(::core::env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            )
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
