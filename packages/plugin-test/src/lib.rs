//! `TestHost`: run any [`Plugin`](bmux_plugin_sdk::author::Plugin) bundled
//! and as a real dynamic library, under the same assertions.
//!
//! The host is the real scoped loader (`bmux_plugin::DefinitionArtifact` /
//! `LoadedExecution`), one host event bus, and a service router standing in
//! for the server's. [`Mode::Bundled`] loads a crate's `plugin_definition()`
//! in process; [`Mode::Dylib`] packages the same crate with
//! `scripts/package-plugin.py` (its own `plugin!` entry, `dynamic-backend`)
//! and loads the library through plug-rt. Everything else is identical:
//!
//! - plugins activate in load order with the host's services, so their
//!   activations can call providers loaded before them and subscribe to
//!   their events;
//! - every call between plugins (and from the test through
//!   [`TestHost::remote`]) goes through the host as checked frames,
//!   attributed to the caller and checked against its granted
//!   capabilities;
//! - [`TestHost::client`] uses the bundled direct fast path (no
//!   serialization) in [`Mode::Bundled`] and the codec in [`Mode::Dylib`];
//! - [`TestHost::subscribe`] records a plugin's contract events from the
//!   host bus;
//! - [`TestHost::fake`] serves an interface from an in-test provider.
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
//! async fn counter_contract() {
//!     bmux_plugin_test::both_modes(|mode| async move {
//!         let host = TestHost::new(mode).load(counter::plugin_crate()).await?;
//!         let commands = host.client::<counter_commands::v1::Client>();
//!         assert_eq!(commands.incr(2).await?, 2);
//!         host.shutdown().await
//!     })
//!     .await
//!     .unwrap();
//! }
//! ```
//!
//! Dylib mode needs `python3` (3.11+) and builds each crate once per test
//! process into `<target>/plugin-dylib`.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

mod package;
mod routes;

use std::future::Future;
use std::sync::Arc;

use bmux_plugin::{
    DefinitionArtifact, EventBus, ExtensionInvoker, ExtensionPoints, ExtensionSet, LoadedExecution,
};
use bmux_plugin_runtime::{PluginConcurrencyConfig, PluginRuntimeScopeId};
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
pub use bmux_plugin_sdk::author::PluginCrate;
use bmux_plugin_sdk::author::{
    self, ContractEvent, EventClient, ExtensionPoint, Provides, ServiceClient, Subscription,
};
use bmux_plugin_sdk::definition::{ComponentRole, PluginHostServices};
use bmux_plugin_sdk::{
    NativeCommandContext, NativeLifecycleContext, PluginInvocationId, TypedServiceRegistry,
};
use routes::{Dispatch, Routes, settle};

/// Result of test-host operations: the author error type, so test bodies
/// `?` generated client calls and host operations alike.
pub type Result<T> = author::Result<T>;

fn fail(details: impl Into<String>) -> author::Error {
    author::Error::failed(details)
}

/// How the plugins under test are packaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In process, from the crate's `plugin_definition()`.
    Bundled,
    /// A real dynamic library built by the shared packager.
    Dylib,
}

impl Mode {
    /// Both modes, bundled first.
    pub const BOTH: [Self; 2] = [Self::Bundled, Self::Dylib];
}

/// Run `test` once per [`Mode`], bundled first; the first failure names
/// its mode.
///
/// # Errors
/// The first mode whose run failed, with that mode in the message.
pub async fn both_modes<F, Fut>(test: F) -> Result<()>
where
    F: Fn(Mode) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for mode in Mode::BOTH {
        test(mode)
            .await
            .map_err(|error| fail(format!("{mode:?}: {error}")))?;
    }
    Ok(())
}

/// One loaded plugin.
struct Loaded {
    id: String,
    execution: Arc<LoadedExecution>,
    /// The bundled typed registry (direct fast path); dylibs have none.
    typed: Option<TypedServiceRegistry>,
}

/// A host for plugin tests. See the crate docs.
pub struct TestHost {
    mode: Mode,
    runtime: PluginRuntimeScopeId,
    bus: Arc<EventBus>,
    routes: Arc<Routes>,
    extensions: ExtensionPoints,
    plugins: Vec<Loaded>,
}

/// Identity of calls the test itself makes.
pub const TEST_CALLER: &str = "bmux.test-host";

fn lifecycle(plugin_id: &str) -> Result<NativeLifecycleContext> {
    serde_json::from_value(serde_json::json!({
        "plugin_id": plugin_id,
        "host": routes::host_metadata(),
        "connection": routes::connection(),
    }))
    .map_err(|error| fail(error.to_string()))
}

impl TestHost {
    /// An empty host in `mode`.
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            runtime: PluginRuntimeScopeId::new(),
            bus: Arc::new(EventBus::new()),
            routes: Arc::new(Routes::default()),
            extensions: ExtensionPoints::new(),
            plugins: Vec::new(),
        }
    }

    /// Declare extension point `E`, as a host program does; plugins loaded
    /// afterwards may contribute to it. Contributions to points the host
    /// does not declare fail `load`.
    ///
    /// # Errors
    /// `E` is already declared.
    pub fn extension_point<E: ExtensionPoint>(self) -> Result<Self> {
        self.extensions.declare::<E>()?;
        Ok(self)
    }

    /// The contributions to a declared extension point: a sorted snapshot
    /// and a change stream; each entry calls its contributor's behavior
    /// across the packaging boundary.
    ///
    /// # Errors
    /// `E` is not declared.
    pub fn extensions<E: ExtensionPoint>(&self) -> Result<ExtensionSet<E>> {
        self.extensions.set::<E>().map_err(Into::into)
    }

    /// The packaging mode of this host's plugins.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Serve the interface `I` from `provider` in the test (a fake provider
    /// of a dependency), reached through its checked adapter in both modes.
    ///
    /// # Errors
    /// Another provider already serves `I`.
    pub fn fake<I: ?Sized + Provides<P>, P: Send + Sync + 'static>(
        self,
        provider: P,
    ) -> Result<Self> {
        self.routes
            .add_fake(I::portable(Arc::new(provider), author::Call::detached()))?;
        Ok(self)
    }

    /// Load and activate one plugin crate in this host's mode. Providers it
    /// serves become routable before its activation runs.
    ///
    /// # Errors
    /// Packaging, loading, creation or activation failed.
    pub async fn load(mut self, plugin: PluginCrate) -> Result<Self> {
        let artifact = match self.mode {
            Mode::Bundled => Arc::new(DefinitionArtifact::from_definition(plugin.definition())),
            Mode::Dylib => package::dylib_artifact(plugin.manifest())?,
        };
        let metadata = artifact.metadata().clone();
        let declarations = plugin.definition().declarations()?;
        let execution = artifact.create(
            self.runtime,
            ComponentRole::Backend,
            &metadata.required_capabilities,
        )?;
        execution.bind_event_bus(&self.bus)?;
        execution.bind_host_dispatch(Arc::new(Dispatch {
            caller: metadata.plugin_id.clone(),
            // What it requires, plus what it provides itself.
            granted: metadata
                .required_capabilities
                .iter()
                .cloned()
                .chain(
                    declarations
                        .services
                        .iter()
                        .map(|service| service.capability.as_str().to_owned()),
                )
                .collect(),
            routes: Arc::clone(&self.routes),
        }))?;
        self.routes
            .add_plugin(&metadata.plugin_id, &execution, &declarations.services)?;
        let id = execution.begin_activation(
            lifecycle(&metadata.plugin_id)?,
            PluginConcurrencyConfig::Concurrent,
        )?;
        match settle(&execution, id).await? {
            NativeOutcome::Unit => {}
            NativeOutcome::Error { message, .. } => {
                return Err(fail(format!(
                    "activating '{}': {message}",
                    metadata.plugin_id
                )));
            }
            other => return Err(fail(format!("unexpected activation outcome {other:?}"))),
        }
        let typed = match self.mode {
            Mode::Bundled => Some(Self::typed_registry(&execution, &metadata.plugin_id)?),
            Mode::Dylib => None,
        };
        let id = execution.begin(NativeOperation::Registrations)?;
        let registrations = match settle(&execution, id).await? {
            NativeOutcome::Registrations(registrations) => registrations,
            NativeOutcome::Error { message, .. } => return Err(fail(message)),
            other => return Err(fail(format!("unexpected registrations {other:?}"))),
        };
        let invoker: Arc<dyn ExtensionInvoker> = Arc::new(Invoker(Arc::downgrade(&execution)));
        self.plugins.push(Loaded {
            id: metadata.plugin_id.clone(),
            execution,
            typed,
        });
        self.extensions
            .contribute_registrations(&metadata.plugin_id, &registrations, &invoker)?;
        Ok(self)
    }

    fn typed_registry(
        execution: &LoadedExecution,
        plugin_id: &str,
    ) -> Result<TypedServiceRegistry> {
        let host = routes::host_metadata();
        let connection = routes::connection();
        let settings = std::collections::BTreeMap::new();
        execution
            .collect_typed_services(bmux_plugin_sdk::TypedServiceRegistrationContext {
                plugin_id,
                host_service_port: None,
                host_kernel_bridge: None,
                required_capabilities: &[],
                provided_capabilities: &[],
                services: &[],
                available_capabilities: &[],
                enabled_plugins: &[],
                plugin_search_roots: &[],
                host: &host,
                connection: &connection,
                plugin_settings_map: &settings,
            })
            .map_err(Into::into)
    }

    /// Host services for calls the test makes: attributed to
    /// [`TEST_CALLER`], with every capability, and subscriptions on the
    /// host bus.
    #[must_use]
    pub fn services(&self) -> PluginHostServices {
        let routes = Arc::clone(&self.routes);
        let bus = Arc::clone(&self.bus);
        PluginHostServices::new(|request| {
            Err(routes::failure(format!(
                "the test host serves '{}' asynchronously only",
                request.interface
            )))
        })
        .with_async(move |request| routes.call(TEST_CALLER, None, request))
        .with_event_subscriptions(move |kind, state| {
            bmux_plugin::subscribe_event_payloads(&bus, kind, state)
        })
    }

    /// A typed client for a provided interface. Bundled: the provider's
    /// registered handle, called directly with no serialization. Dylib:
    /// checked frames through the host, as any consumer.
    #[must_use]
    pub fn client<C: ServiceClient>(&self) -> C {
        let descriptor = C::DESCRIPTOR;
        let direct = self.plugins.iter().find_map(|plugin| {
            let handle = plugin.typed.as_ref()?.get(
                &bmux_plugin_sdk::HostScope::new(descriptor.capability.as_str()).ok()?,
                descriptor.kind,
                descriptor.interface_id.as_str(),
            )?;
            C::direct(handle).ok()
        });
        direct.unwrap_or_else(|| self.remote())
    }

    /// A typed client through the host in either mode (checked frames).
    #[must_use]
    pub fn remote<C: ServiceClient>(&self) -> C {
        C::connect(self.services())
    }

    /// A typed events client over the host bus.
    #[must_use]
    pub fn events<C: EventClient>(&self) -> C {
        C::connect(self.services())
    }

    /// Record contract events `E` from the host bus from now on (a state
    /// channel yields its current value first). The channel must exist: its
    /// publisher declared `emits::<E>()` and is loaded.
    ///
    /// # Errors
    /// The channel is not registered.
    pub fn subscribe<E: ContractEvent + serde::de::DeserializeOwned>(
        &self,
    ) -> author::Result<Subscription<E>> {
        author::subscribe(&self.services(), &E::KIND, E::DELIVERY)
    }

    fn plugin(&self, plugin_id: &str) -> Result<&Loaded> {
        self.plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .ok_or_else(|| fail(format!("plugin '{plugin_id}' is not loaded")))
    }

    /// Run one plugin command with CLI-style arguments; returns its exit code.
    ///
    /// # Errors
    /// The plugin is not loaded or the command failed.
    pub async fn command(&self, plugin_id: &str, command: &str, arguments: &[&str]) -> Result<i32> {
        let plugin = self.plugin(plugin_id)?;
        let context = NativeCommandContext {
            plugin_id: plugin_id.to_owned(),
            command: command.to_owned(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            registered_plugins: vec![],
            active_keybindings: vec![],
            host: routes::host_metadata(),
            connection: routes::connection(),
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::default(),
            host_service_port: None,
            host_kernel_bridge: None,
        };
        let id = plugin.execution.begin(NativeOperation::Command {
            invocation_id: PluginInvocationId::new(),
            context,
        })?;
        match settle(&plugin.execution, id).await? {
            NativeOutcome::Command(outcome) => match outcome.error {
                Some(error) => Err(author::Error::new(outcome.exit_code, error)),
                None => Ok(outcome.exit_code),
            },
            NativeOutcome::Error { message, .. } => Err(fail(message)),
            other => Err(fail(format!("unexpected command outcome {other:?}"))),
        }
    }

    /// Deactivate and destroy every plugin in reverse load order, draining
    /// the tasks their activations own.
    ///
    /// # Errors
    /// Teardown failed or left activation-owned work behind.
    pub async fn shutdown(mut self) -> Result<()> {
        while let Some(plugin) = self.plugins.pop() {
            self.extensions.withdraw(&plugin.id);
            let execution = &plugin.execution;
            execution.cancel_background_tasks()?;
            execution.drain_background_tasks().await?;
            let id = execution.begin_deactivation(lifecycle(&plugin.id)?).await?;
            settle(execution, id).await?;
            execution.destroy().await?;
            let leftover = execution.pending_background_work();
            if !leftover.is_empty() {
                return Err(fail(format!(
                    "'{}' left activation work behind: {leftover:?}",
                    plugin.id
                )));
            }
        }
        Ok(())
    }

    /// Plugin ids in load order.
    #[must_use]
    pub fn plugins(&self) -> Vec<&str> {
        self.plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect()
    }
}

/// Calls a loaded instance's contribution behavior through its extension
/// port (in process or across the dylib boundary).
struct Invoker(std::sync::Weak<LoadedExecution>);

impl ExtensionInvoker for Invoker {
    fn invoke(
        &self,
        extension_point: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        let execution = self.0.upgrade();
        let contract = extension_point.to_owned();
        Box::pin(async move {
            let execution =
                execution.ok_or_else(|| routes::failure("the contributing instance is gone"))?;
            let id = execution.begin(NativeOperation::Extension {
                invocation_id: PluginInvocationId::new(),
                contract,
                payload,
            })?;
            match settle(&execution, id).await? {
                NativeOutcome::Bytes(bytes) => Ok(bytes),
                NativeOutcome::Error { message, .. } => Err(routes::failure(message)),
                other => Err(routes::failure(format!(
                    "unexpected extension outcome {other:?}"
                ))),
            }
        })
    }
}
