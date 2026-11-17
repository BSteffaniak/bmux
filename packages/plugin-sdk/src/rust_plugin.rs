//! The author-facing `RustPlugin` trait and its supporting types. Hosts adapt
//! it per scoped instance through `define_plugin!(rust_plugin = ...)`.
use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    NativeStreamingServiceContext, PluginEvent, ServiceResponse, TypedServiceRegistry,
};
use std::future::Future;
use std::sync::Arc;

// ── Plugin exit codes ────────────────────────────────────────────────────────

/// Command completed successfully.
pub const EXIT_OK: i32 = 0;

/// Command failed with a generic error.
pub const EXIT_ERROR: i32 = 1;

/// Command received invalid arguments or was unknown.
pub const EXIT_USAGE: i32 = 64;

/// Plugin is unavailable (e.g. mutex poisoned, feature disabled).
pub const EXIT_UNAVAILABLE: i32 = 70;

// ── Plugin command error ─────────────────────────────────────────────────────

/// Error type for plugin command and lifecycle methods.
///
/// Carries an exit code and a human-readable message. When a plugin
/// method returns `Err(PluginCommandError)`, the scoped definition adapter
/// reports it to the host as a structured outcome. The error message is
/// never written to stderr — `native_fast` plugins share stderr with an
/// interactive attach TUI and raw writes would corrupt pane rendering.
///
/// Implements `From<String>` and `From<&str>` for easy use with the `?`
/// operator — string errors map to [`EXIT_ERROR`].
#[derive(Debug, Clone)]
pub struct PluginCommandError {
    pub code: i32,
    pub message: String,
}

impl PluginCommandError {
    /// Create an error with a specific exit code and message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Generic failure ([`EXIT_ERROR`]).
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(EXIT_ERROR, message)
    }

    /// Unknown or unsupported command ([`EXIT_USAGE`]).
    #[must_use]
    pub fn unknown_command(name: &str) -> Self {
        Self::new(EXIT_USAGE, format!("unknown command '{name}'"))
    }

    /// Invalid arguments ([`EXIT_USAGE`]).
    #[must_use]
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }

    /// Plugin unavailable ([`EXIT_UNAVAILABLE`]).
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(EXIT_UNAVAILABLE, message)
    }
}

impl std::fmt::Display for PluginCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PluginCommandError {}

impl From<String> for PluginCommandError {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl From<&str> for PluginCommandError {
    fn from(message: &str) -> Self {
        Self::failed(message)
    }
}

impl From<std::io::Error> for PluginCommandError {
    fn from(error: std::io::Error) -> Self {
        Self::failed(error.to_string())
    }
}

impl From<serde_json::Error> for PluginCommandError {
    fn from(error: serde_json::Error) -> Self {
        Self::failed(error.to_string())
    }
}

impl From<toml::de::Error> for PluginCommandError {
    fn from(error: toml::de::Error) -> Self {
        Self::failed(error.to_string())
    }
}

impl From<Box<dyn std::error::Error>> for PluginCommandError {
    fn from(error: Box<dyn std::error::Error>) -> Self {
        Self::failed(error.to_string())
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for PluginCommandError {
    fn from(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::failed(error.to_string())
    }
}

// ── Plugin trait ─────────────────────────────────────────────────────────────

/// The core trait that every bmux plugin implements.
///
/// All five methods have default implementations, so a plugin only needs to
/// override the methods relevant to its functionality:
///
/// - [`run_command`](Self::run_command) — handle CLI commands declared in `plugin.toml`
/// - [`invoke_service`](Self::invoke_service) — handle inbound service calls from other plugins
/// - [`activate`](Self::activate) / [`deactivate`](Self::deactivate) — lifecycle hooks
/// - [`handle_event`](Self::handle_event) — react to system or plugin events
///
/// ## Error patterns
///
/// Commands and lifecycle hooks return `Result<i32, PluginCommandError>` where
/// the `i32` is an exit code (use [`EXIT_OK`], [`EXIT_ERROR`], etc.). On
/// `Err`, the scoped definition adapter reports the structured error in the
/// command outcome. The error message is never written to stderr — that would
/// corrupt an attached TUI for `native_fast` plugins running in-process.
///
/// Service handlers return [`ServiceResponse`] directly — a structured RPC
/// response with an optional error payload.  Use [`handle_service`](crate::handle_service)
/// or [`route_service!`](crate::route_service) to reduce boilerplate.
pub trait RustPlugin: Default + Send + 'static {
    /// Migration hook for generic state/readiness/frontend registration
    /// contracts. Consumed by the unified scoped definition, not a global slot.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn registration_contracts(
        &self,
    ) -> Result<Vec<crate::RegistrationContract>, PluginCommandError> {
        Ok(Vec::new())
    }
    /// Native optimization for the same declared typed persistence contracts.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn stateful_participants(
        &self,
    ) -> Result<Vec<crate::StatefulPluginHandle>, PluginCommandError> {
        Ok(Vec::new())
    }
    /// Generated portable dispatch adapters must wrap the same provider objects
    /// registered through `register_typed_services`, never a second factory.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn portable_services(
        &self,
    ) -> Result<Vec<Arc<dyn crate::PortableServiceProvider>>, PluginCommandError> {
        Ok(Vec::new())
    }
    /// BPDL-generated contract this plugin implements.
    ///
    /// Use a generated `*_plugin_api::Contract` for BPDL-backed plugins or
    /// [`crate::NoPluginContract`] for plugins whose service surface is manual
    /// or command-only.
    type Contract: crate::PluginContract;

    /// Whether this implementation needs mutable event callbacks. Implementations
    /// using the default no-op handler may opt out to avoid taking a write lock
    /// while service readers wait for reentrant peer calls. Defaults to true for
    /// compatibility with existing event handlers.
    const HANDLES_EVENTS: bool = true;

    /// Handle a CLI command declared in the plugin manifest.
    ///
    /// The default returns `Err(PluginCommandError::unknown_command(""))`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] when the command fails or is unrecognised.
    fn run_command(&mut self, _context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    /// Called when the plugin is activated by the host.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if activation fails.
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Called when the plugin is activated by an in-process host that can
    /// provide access to its existing async runtime.
    ///
    /// The default delegates to [`Self::activate`], preserving synchronous
    /// lifecycle behavior for plugins that do not need background async work.
    /// Dynamic/process plugin backends continue to use [`Self::activate`]
    /// because runtime handles are intentionally not serialized across plugin
    /// boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if activation fails.
    fn activate_with_async(
        &mut self,
        context: NativeLifecycleContext,
        _async_handle: HostAsyncHandle,
    ) -> Result<i32, PluginCommandError> {
        self.activate(context)
    }

    /// Called when the plugin is deactivated by the host.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if deactivation fails.
    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Called when a subscribed event fires.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if event handling fails.
    fn handle_event(&mut self, _event: PluginEvent) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Handle an inbound service call from another plugin or the host.
    ///
    /// The default returns an "`unsupported_service`" error response.
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "unsupported_service",
            format!(
                "plugin '{}' does not implement service '{}:{}'",
                context.plugin_id, context.request.service.interface_id, context.request.operation,
            ),
        )
    }

    /// Handle a streaming service call.
    ///
    /// The default delegates to [`Self::invoke_service`], preserving existing
    /// non-streaming behavior for plugins that do not emit request-scoped events.
    fn invoke_streaming_service(&self, context: NativeStreamingServiceContext) -> ServiceResponse {
        self.invoke_service(context.service)
    }

    /// Populate a [`TypedServiceRegistry`] with this plugin's typed
    /// service handles. Called once at plugin-host setup time, before
    /// [`Self::activate`]. Providers insert `Arc`s of their
    /// service-trait implementations into the registry; the host
    /// stores the resulting handles so consumers can resolve them via
    /// [`crate::PluginHost::resolve_typed_service`] and invoke typed
    /// calls without serialization.
    ///
    /// `context` exposes the [`crate::HostKernelBridge`] and plugin
    /// identity so providers can construct handles that make host-
    /// level calls from inside their trait methods. Implementations
    /// that don't need host access can ignore it.
    ///
    /// The default is a no-op; plugins that don't provide typed
    /// services need not override it.
    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
    }

    /// Register activation-time contributions.
    ///
    /// # Errors
    ///
    /// Returns when contribution registration fails.
    fn register_contributions(
        &self,
        _registrar: &mut crate::ContributionRegistrar,
    ) -> crate::Result<()> {
        Ok(())
    }
}

/// Context passed to [`RustPlugin::register_typed_services`].
///
/// Carries everything a provider plugin needs to construct stateful
/// typed handles — most importantly a [`crate::HostKernelBridge`]
/// reference so the handle can call into the host from inside its
/// trait methods, plus the plugin identity and capability/service
/// inventory needed to build a standalone
/// [`crate::ServiceCaller`] wrapper if the handle needs one.
#[derive(Debug, Clone)]
pub struct TypedServiceRegistrationContext<'a> {
    /// The plugin's own id (matches `plugin_id` everywhere else).
    pub plugin_id: &'a str,
    /// Selected activation-owned host capability for registered typed handles.
    /// The host must admit a fresh invocation for each subsequent call, not
    /// capture the temporary registration invocation. `None` means unsupported;
    /// inventory metadata never supplies replacement authority or absence proof.
    pub host_service_port: Option<&'a crate::PluginHostServices>,
    /// Handle used to dispatch calls to the host kernel. `None` when
    /// the host is not wired for bridge-style dispatch (e.g. some
    /// test harnesses); typed handles that need host access should
    /// treat that as a reason to skip registration and log a warning.
    pub host_kernel_bridge: Option<&'a crate::HostKernelBridge>,
    /// Capabilities this plugin declared as required in its manifest.
    pub required_capabilities: &'a [String],
    /// Capabilities this plugin provides to other plugins.
    pub provided_capabilities: &'a [String],
    /// Registered services visible to this plugin for cross-plugin calls.
    pub services: &'a [crate::RegisteredService],
    /// All capabilities available in the current host environment.
    pub available_capabilities: &'a [String],
    /// IDs of all currently enabled plugins.
    pub enabled_plugins: &'a [String],
    /// Filesystem roots where plugin manifests are discovered.
    pub plugin_search_roots: &'a [String],
    /// Host runtime metadata (product name, version, API version).
    pub host: &'a crate::HostMetadata,
    /// Host connection paths (config dir, runtime dir, data dir, state dir).
    pub connection: &'a crate::HostConnectionInfo,
    /// Settings map for all plugins (keyed by plugin ID).
    pub plugin_settings_map: &'a std::collections::BTreeMap<String, toml::Value>,
}

/// The activation's task scope, handed to async activation hooks.
///
/// It is not a runtime handle: every spawn goes through plug-rt
/// ([`crate::rt`]) onto the host's single runtime and is owned by this
/// activation, so host teardown cancels, drains and aborts it. It is never
/// serialized into lifecycle contexts.
#[derive(Debug, Clone)]
pub struct HostAsyncHandle {
    background: bmux_plugin_runtime::background::BackgroundTasks,
}

impl HostAsyncHandle {
    /// Bind to one host-managed activation's task scope.
    #[must_use]
    pub const fn new(tasks: bmux_plugin_runtime::background::BackgroundTasks) -> Self {
        Self { background: tasks }
    }

    /// Start cooperative work owned by this activation.
    ///
    /// # Errors
    /// Rejects closed or full task scopes.
    pub fn spawn_background<F, Fut>(&self, name: &str, work: F) -> Result<(), String>
    where
        F: FnOnce(bmux_plugin_runtime::background::TaskCancellation) -> Fut,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let task_name = name.to_string();
        self.background.spawn(name, move |cancel| {
            let future = work(cancel);
            async move {
                let result = future.await;
                if let Err(error) = &result {
                    tracing::error!(task = %task_name, %error, "plugin background task failed");
                }
                result
            }
        })
    }

    /// Spawn a future owned by this activation onto the host runtime.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> crate::rt::JoinHandle<T> {
        let _scope = self.background.enter();
        crate::rt::spawn(future)
    }

    /// Spawn a named future owned by this activation onto the host runtime.
    pub fn spawn_with_name<T: Send + 'static>(
        &self,
        name: &str,
        future: impl Future<Output = T> + Send + 'static,
    ) -> crate::rt::JoinHandle<T> {
        let _scope = self.background.enter();
        crate::rt::spawn_named(name, future)
    }

    /// Run blocking work owned by this activation on the host's blocking pool.
    pub fn spawn_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> crate::rt::JoinHandle<T> {
        let _scope = self.background.enter();
        crate::rt::spawn_blocking(f)
    }
}
