use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    NativeStreamingServiceContext, PluginEvent, PluginService, ServiceEnvelopeKind,
    ServiceResponse, TypedServiceRegistry, decode_service_envelope_with_invocation_id,
    encode_service_envelope_with_invocation_id, encode_service_message,
};
use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::future::Future;
use std::ptr;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

const PLUGIN_LOCK_LATENCY_BUDGET: Duration = Duration::from_millis(10);

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
/// method returns `Err(PluginCommandError)`, the SDK captures the
/// error for the host to retrieve via [`take_last_command_error`] and
/// returns the error's exit code to the host. The error message is
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

/// Convert a plugin Result into an FFI exit code.
///
/// - `Ok(code)` → returns `code`
/// - `Err(e)` → returns `e.code`
///
/// The error's message is not written to stderr: `native_fast` plugins
/// execute in-process alongside an interactive attach TUI, and writing
/// to stderr corrupts the attached terminal. Hosts that need the error
/// text read [`take_last_command_error`] after the FFI call returns.
fn result_to_exit_code(result: Result<i32, PluginCommandError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(error) => error.code,
    }
}

thread_local! {
    /// Slot populated by [`run_command_export`] when a plugin command
    /// returns `Err`. The host reads this via [`take_last_command_error`]
    /// immediately after the FFI call and routes the message to logs /
    /// status line instead of the raw tty.
    static LAST_COMMAND_ERROR: RefCell<Option<PluginCommandError>> = const { RefCell::new(None) };
}

/// Retrieve and clear the most recent plugin command error captured by
/// the SDK's FFI boundary.
///
/// Hosts call this immediately after `bmux_plugin_run_command_v1` /
/// `bmux_plugin_run_command_with_context_v1` returns to fetch the
/// structured error (if any) the plugin produced.
///
/// Returns `None` when the last command succeeded or when no command
/// has been invoked on the current thread.
#[must_use]
pub fn take_last_command_error() -> Option<PluginCommandError> {
    LAST_COMMAND_ERROR.with(|slot| slot.borrow_mut().take())
}

/// Store a pending command error so the host can retrieve it via
/// [`take_last_command_error`]. Overwrites any previously-stored error.
fn store_last_command_error(error: PluginCommandError) {
    LAST_COMMAND_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(error);
    });
}

// ── Internal FFI status codes (not exposed to plugin authors) ────────────────

const SERVICE_STATUS_OK: i32 = 0;
const SERVICE_STATUS_INVALID_ARGUMENT: i32 = 2;
const SERVICE_STATUS_DECODE_FAILED: i32 = 3;
const SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const SERVICE_STATUS_ENCODE_FAILED: i32 = 5;
const SERVICE_STATUS_PLUGIN_UNAVAILABLE: i32 = 70;

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
/// `Err`, the SDK captures the error for the host to retrieve via
/// [`take_last_command_error`] and returns the error's exit code to the
/// host. The error message is never written to stderr — that would corrupt
/// an attached TUI for `native_fast` plugins running in-process.
///
/// Service handlers return [`ServiceResponse`] directly — a structured RPC
/// response with an optional error payload.  Use [`handle_service`](crate::handle_service)
/// or [`route_service!`](crate::route_service) to reduce boilerplate.
/// Thread-safe Rust plugin trait for hot-path in-process plugins.
///
/// Implement this when service/lifecycle methods can run through shared
/// references. The SDK stores concurrent plugin instances in `Arc<P>` and does
/// not create a per-plugin async runtime; scheduling remains host-owned.
pub trait ConcurrentRustPlugin: Default + Send + Sync + 'static {
    /// BPDL-generated contract this plugin implements.
    type Contract: crate::PluginContract;

    /// Called when the plugin is activated by the host.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if activation fails.
    fn activate_concurrent(
        &self,
        _context: NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Called when the plugin is deactivated by the host.
    ///
    /// # Errors
    ///
    /// Returns [`PluginCommandError`] if deactivation fails.
    fn deactivate_concurrent(
        &self,
        _context: NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Handle a service call without requiring a plugin-instance mutex.
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "unsupported_service",
            format!(
                "plugin '{}' does not implement service '{}:{}'",
                context.plugin_id, context.request.service.interface_id, context.request.operation,
            ),
        )
    }

    /// Handle a streaming service call without requiring a plugin-instance mutex.
    fn invoke_streaming_service_concurrent(
        &self,
        context: NativeStreamingServiceContext,
    ) -> ServiceResponse {
        self.invoke_service_concurrent(context.service)
    }

    /// Return BPDL-generated services this plugin provides.
    ///
    /// # Errors
    ///
    /// Returns when generated descriptors cannot be converted into host service
    /// declarations.
    fn declared_services() -> crate::Result<Vec<PluginService>>
    where
        Self: Sized,
    {
        <Self::Contract as crate::PluginContract>::service_declarations()
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

pub trait RustPlugin: Default + Send + 'static {
    /// BPDL-generated contract this plugin implements.
    ///
    /// Use a generated `*_plugin_api::Contract` for BPDL-backed plugins or
    /// [`crate::NoPluginContract`] for plugins whose service surface is manual
    /// or command-only.
    type Contract: crate::PluginContract;

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

    /// Return BPDL-generated services this plugin provides.
    ///
    /// Hosts merge these descriptors into the plugin declaration for native
    /// bundled plugins, so BPDL service interfaces do not have to be repeated in
    /// `plugin.toml`. Explicit manifest services remain supported for
    /// non-BPDL surfaces, process plugins, dynamic compatibility, and override
    /// use cases.
    ///
    /// # Errors
    ///
    /// Returns when generated descriptors cannot be converted into host service
    /// declarations.
    fn declared_services() -> crate::Result<Vec<PluginService>>
    where
        Self: Sized,
    {
        <Self::Contract as crate::PluginContract>::service_declarations()
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

/// Handle for scheduling plugin-owned background work on the host's existing
/// async runtime.
///
/// This wrapper is intentionally exposed only through in-process Rust plugin
/// hooks. It is not serialized into lifecycle contexts and should not be passed
/// across process or dynamic-library ABI boundaries.
#[derive(Debug, Clone)]
pub struct HostAsyncHandle {
    inner: switchy::unsync::runtime::Handle,
}

impl HostAsyncHandle {
    /// Wrap a switchy async runtime handle.
    #[must_use]
    pub const fn new(inner: switchy::unsync::runtime::Handle) -> Self {
        Self { inner }
    }

    /// Try to capture the async runtime currently entered on this thread.
    ///
    /// # Errors
    ///
    /// Returns an error string when no runtime is currently entered.
    pub fn try_current() -> std::result::Result<Self, String> {
        switchy::unsync::runtime::Handle::try_current()
            .map(Self::new)
            .map_err(|error| error.to_string())
    }

    /// Spawn a named `Send` future onto the host runtime.
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> switchy::unsync::task::JoinHandle<T> {
        self.inner.spawn(future)
    }

    /// Spawn a named `Send` future onto the host runtime.
    pub fn spawn_with_name<T: Send + 'static>(
        &self,
        name: &str,
        future: impl Future<Output = T> + Send + 'static,
    ) -> switchy::unsync::task::JoinHandle<T> {
        self.inner.spawn_with_name(name, future)
    }

    /// Spawn blocking work onto the host runtime's blocking pool.
    pub fn spawn_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> switchy::unsync::task::JoinHandle<T> {
        self.inner.spawn_blocking(f)
    }
}

// ── FFI helpers ──────────────────────────────────────────────────────────────

#[doc(hidden)]
pub fn plugin_instance<P: RustPlugin>(
    instance: &'static OnceLock<RwLock<P>>,
) -> &'static RwLock<P> {
    instance.get_or_init(|| RwLock::new(P::default()))
}

#[doc(hidden)]
pub fn concurrent_plugin_instance<P: ConcurrentRustPlugin>(
    instance: &'static OnceLock<Arc<P>>,
) -> &'static Arc<P> {
    instance.get_or_init(|| Arc::new(P::default()))
}

/// Invoke a bundled plugin's [`RustPlugin::register_typed_services`] hook
/// and return the populated registry. Called from the
/// [`bundled_plugin_vtable!`] macro.
#[doc(hidden)]
pub fn register_typed_services_bundled<P: RustPlugin>(
    instance: &'static RwLock<P>,
    context: TypedServiceRegistrationContext<'_>,
) -> TypedServiceRegistry {
    let mut registry = TypedServiceRegistry::new();
    if let Ok(plugin) = instance.read() {
        plugin.register_typed_services(context, &mut registry);
    }
    registry
}

/// Invoke a bundled plugin's runtime-aware activation hook.
#[doc(hidden)]
pub fn activate_with_async_bundled<P: RustPlugin>(
    instance: &'static RwLock<P>,
    context: NativeLifecycleContext,
    async_handle: HostAsyncHandle,
) -> i32 {
    instance.write().map_or(EXIT_UNAVAILABLE, |mut plugin| {
        result_to_exit_code(plugin.activate_with_async(context, async_handle))
    })
}

/// Collect BPDL-generated service declarations from a bundled plugin instance.
#[doc(hidden)]
pub fn declared_services_bundled<P: RustPlugin>() -> crate::Result<Vec<PluginService>> {
    P::declared_services()
}

#[doc(hidden)]
pub fn register_contributions_bundled<P: RustPlugin>(
    instance: &'static RwLock<P>,
) -> crate::Result<Vec<crate::PluginContribution>> {
    let plugin = instance
        .read()
        .map_err(|_| crate::PluginError::ServiceProtocol {
            details: "plugin contribution read lock poisoned".to_string(),
        })?;
    let mut registrar = crate::ContributionRegistrar::new();
    plugin.register_contributions(&mut registrar)?;
    drop(plugin);
    Ok(registrar.into_contributions())
}

#[doc(hidden)]
pub fn register_contributions_concurrent_bundled<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
) -> crate::Result<Vec<crate::PluginContribution>> {
    let mut registrar = crate::ContributionRegistrar::new();
    instance.register_contributions(&mut registrar)?;
    Ok(registrar.into_contributions())
}

#[doc(hidden)]
pub fn register_contributions_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }
    let Ok(contributions) = register_contributions_bundled(instance) else {
        return SERVICE_STATUS_PLUGIN_UNAVAILABLE;
    };
    write_encoded_contributions(&contributions, output_ptr, output_capacity, output_len)
}

#[doc(hidden)]
pub fn register_contributions_concurrent_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }
    let Ok(contributions) = register_contributions_concurrent_bundled(instance) else {
        return SERVICE_STATUS_PLUGIN_UNAVAILABLE;
    };
    write_encoded_contributions(&contributions, output_ptr, output_capacity, output_len)
}

fn write_encoded_contributions(
    contributions: &[crate::PluginContribution],
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    let Ok(encoded) = encode_service_message(&contributions.to_vec()) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };
    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_STATUS_OK
}

#[doc(hidden)]
pub fn manifest_toml_ptr(
    manifest_toml: &'static str,
    cached: &'static OnceLock<Option<CString>>,
) -> *const c_char {
    let cached = cached.get_or_init(|| CString::new(manifest_toml).ok());
    cached
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr())
}

#[doc(hidden)]
pub fn run_command_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<NativeCommandContext>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance.write().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                let result = plugin.run_command(payload);
                if let Err(error) = &result {
                    store_last_command_error(error.clone());
                }
                result_to_exit_code(result)
            })
        },
    )
}

#[doc(hidden)]
pub fn activate_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<NativeLifecycleContext>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance.write().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.activate(payload))
            })
        },
    )
}

#[doc(hidden)]
pub fn deactivate_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<NativeLifecycleContext>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance.write().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.deactivate(payload))
            })
        },
    )
}

#[doc(hidden)]
pub fn activate_concurrent_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<NativeLifecycleContext>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| result_to_exit_code(instance.activate_concurrent(payload)),
    )
}

#[doc(hidden)]
pub fn deactivate_concurrent_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<NativeLifecycleContext>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| result_to_exit_code(instance.deactivate_concurrent(payload)),
    )
}

#[doc(hidden)]
pub fn declared_services_concurrent_bundled<P: ConcurrentRustPlugin>()
-> crate::Result<Vec<PluginService>> {
    P::declared_services()
}

#[doc(hidden)]
pub fn invoke_service_concurrent_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok((invocation_id, request_id, context)) = decode_service_envelope_with_invocation_id::<
        NativeServiceContext,
    >(input, ServiceEnvelopeKind::Request) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    let response = instance.invoke_service_concurrent(context);
    let Ok(encoded) = encode_service_envelope_with_invocation_id(
        invocation_id,
        request_id,
        ServiceEnvelopeKind::Response,
        &response,
    ) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };
    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_STATUS_OK
}

#[doc(hidden)]
pub fn invoke_streaming_service_concurrent_export<P: ConcurrentRustPlugin>(
    instance: &'static Arc<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok((invocation_id, request_id, service)) = decode_service_envelope_with_invocation_id::<
        NativeServiceContext,
    >(input, ServiceEnvelopeKind::Request) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };
    let response = instance.invoke_streaming_service_concurrent(NativeStreamingServiceContext {
        service,
        events: crate::ServiceEventSinkHandle::noop(),
    });
    let Ok(encoded) = encode_service_envelope_with_invocation_id(
        invocation_id,
        request_id,
        ServiceEnvelopeKind::Response,
        &response,
    ) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };
    unsafe {
        *output_len = encoded.len();
    }
    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }
    SERVICE_STATUS_OK
}

#[doc(hidden)]
pub fn handle_event_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
) -> i32 {
    parse_binary_input::<PluginEvent>(input_ptr, input_len, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance.write().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.handle_event(payload))
            })
        },
    )
}

#[doc(hidden)]
pub fn invoke_service_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok((invocation_id, request_id, context)) = decode_service_envelope_with_invocation_id::<
        NativeServiceContext,
    >(input, ServiceEnvelopeKind::Request) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };

    let plugin_id = context.plugin_id.clone();
    let interface_id = context.request.service.interface_id.clone();
    let operation = context.request.operation.clone();
    let lock_started = Instant::now();
    let response = {
        let Ok(plugin) = instance.read() else {
            return SERVICE_STATUS_PLUGIN_UNAVAILABLE;
        };
        let lock_wait = lock_started.elapsed();
        if lock_wait > PLUGIN_LOCK_LATENCY_BUDGET {
            tracing::warn!(
                request_id,
                plugin_id = plugin_id.as_str(),
                interface_id = interface_id.as_str(),
                operation = operation.as_str(),
                wait_us = lock_wait.as_micros(),
                "plugin service read lock wait exceeded latency budget"
            );
        }

        let call_started = Instant::now();
        let response = plugin.invoke_service(context);
        let lock_hold = call_started.elapsed();
        if lock_hold > PLUGIN_LOCK_LATENCY_BUDGET {
            tracing::warn!(
                request_id,
                plugin_id = plugin_id.as_str(),
                interface_id = interface_id.as_str(),
                operation = operation.as_str(),
                hold_us = lock_hold.as_micros(),
                "plugin service read lock hold exceeded latency budget"
            );
        }
        response
    };

    let Ok(encoded) = encode_service_envelope_with_invocation_id(
        invocation_id,
        request_id,
        ServiceEnvelopeKind::Response,
        &response,
    ) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };

    unsafe {
        *output_len = encoded.len();
    }

    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }

    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }

    SERVICE_STATUS_OK
}

#[doc(hidden)]
#[allow(dead_code)]
pub fn invoke_streaming_service_export<P: RustPlugin>(
    instance: &'static RwLock<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let Ok((invocation_id, request_id, service)) = decode_service_envelope_with_invocation_id::<
        NativeServiceContext,
    >(input, ServiceEnvelopeKind::Request) else {
        return SERVICE_STATUS_DECODE_FAILED;
    };

    let context = NativeStreamingServiceContext {
        service,
        events: crate::ServiceEventSinkHandle::noop(),
    };
    let response = {
        let Ok(plugin) = instance.read() else {
            return SERVICE_STATUS_PLUGIN_UNAVAILABLE;
        };
        plugin.invoke_streaming_service(context)
    };

    let Ok(encoded) = encode_service_envelope_with_invocation_id(
        invocation_id,
        request_id,
        ServiceEnvelopeKind::Response,
        &response,
    ) else {
        return SERVICE_STATUS_ENCODE_FAILED;
    };

    unsafe {
        *output_len = encoded.len();
    }

    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }

    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }

    SERVICE_STATUS_OK
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod concurrent_tests {
    use super::{
        ConcurrentRustPlugin, NativeLifecycleContext, NativeServiceContext,
        NativeStreamingServiceContext, ServiceEnvelopeKind, ServiceResponse,
    };
    use crate::{
        ApiVersion, CancellationToken, HostConnectionInfo, HostMetadata, HostScope,
        NoPluginContract, PluginInvocationId, ProviderId, RegisteredService, ServiceKind,
        ServiceRequest, decode_service_envelope_with_invocation_id,
        encode_service_envelope_with_invocation_id,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, OnceLock};
    use std::thread;

    #[derive(Default)]
    struct TestConcurrentPlugin {
        active_calls: AtomicUsize,
        max_active_calls: AtomicUsize,
        activations: AtomicUsize,
        deactivations: AtomicUsize,
    }

    impl ConcurrentRustPlugin for TestConcurrentPlugin {
        type Contract = NoPluginContract;

        fn activate_concurrent(
            &self,
            _context: NativeLifecycleContext,
        ) -> Result<i32, crate::PluginCommandError> {
            self.activations.fetch_add(1, Ordering::SeqCst);
            Ok(crate::EXIT_OK)
        }

        fn deactivate_concurrent(
            &self,
            _context: NativeLifecycleContext,
        ) -> Result<i32, crate::PluginCommandError> {
            self.deactivations.fetch_add(1, Ordering::SeqCst);
            Ok(crate::EXIT_OK)
        }

        fn invoke_service_concurrent(&self, _context: NativeServiceContext) -> ServiceResponse {
            let current = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active_calls.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.active_calls.fetch_sub(1, Ordering::SeqCst);
            ServiceResponse::ok(vec![1])
        }

        fn invoke_streaming_service_concurrent(
            &self,
            context: NativeStreamingServiceContext,
        ) -> ServiceResponse {
            let mut payload = context.service.request.payload;
            payload.push(2);
            ServiceResponse::ok(payload)
        }
    }

    fn service_context(payload: Vec<u8>) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "test.concurrent".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new("test.service").expect("scope should parse"),
                    kind: ServiceKind::Command,
                    interface_id: "test-service".to_string(),
                    provider: ProviderId::Plugin("test.concurrent".to_string()),
                },
                operation: "run".to_string(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux-test".to_string(),
                product_version: "0.0.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: Vec::new(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: CancellationToken::new(),
            host_kernel_bridge: None,
        }
    }

    fn lifecycle_context() -> NativeLifecycleContext {
        NativeLifecycleContext {
            plugin_id: "test.concurrent".to_string(),
            host: HostMetadata {
                product_name: "bmux-test".to_string(),
                product_version: "0.0.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: Vec::new(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        }
    }

    #[test]
    fn concurrent_plugin_handles_parallel_calls() {
        let plugin = Arc::new(TestConcurrentPlugin::default());
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let plugin = plugin.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                assert_eq!(
                    plugin
                        .invoke_service_concurrent(service_context(Vec::new()))
                        .payload,
                    vec![1]
                );
            }));
        }
        for handle in handles {
            handle.join().expect("worker should finish");
        }
        assert!(plugin.max_active_calls.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn concurrent_plugin_streaming_works() {
        let plugin = TestConcurrentPlugin::default();
        let response = plugin.invoke_streaming_service_concurrent(NativeStreamingServiceContext {
            service: service_context(vec![1]),
            events: crate::ServiceEventSinkHandle::noop(),
        });
        assert_eq!(response.payload, vec![1, 2]);
    }

    #[test]
    fn concurrent_plugin_activation_deactivation_exports_work() {
        static INSTANCE: OnceLock<Arc<TestConcurrentPlugin>> = OnceLock::new();
        let plugin = crate::__private::concurrent_plugin_instance(&INSTANCE);
        let payload = crate::encode_service_message(&lifecycle_context())
            .expect("lifecycle context should encode");
        assert_eq!(
            super::activate_concurrent_export(plugin, payload.as_ptr(), payload.len()),
            crate::EXIT_OK
        );
        assert_eq!(
            super::deactivate_concurrent_export(plugin, payload.as_ptr(), payload.len()),
            crate::EXIT_OK
        );
        assert_eq!(plugin.activations.load(Ordering::SeqCst), 1);
        assert_eq!(plugin.deactivations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_service_export_preserves_response_semantics() {
        static INSTANCE: OnceLock<Arc<TestConcurrentPlugin>> = OnceLock::new();
        let plugin = crate::__private::concurrent_plugin_instance(&INSTANCE);
        let invocation_id = PluginInvocationId::new();
        let request = encode_service_envelope_with_invocation_id(
            invocation_id.clone(),
            7,
            ServiceEnvelopeKind::Request,
            &service_context(Vec::new()),
        )
        .expect("request should encode");
        let mut output = vec![0_u8; 4096];
        let mut output_len = 0_usize;
        assert_eq!(
            super::invoke_service_concurrent_export(
                plugin,
                request.as_ptr(),
                request.len(),
                output.as_mut_ptr(),
                output.len(),
                &raw mut output_len,
            ),
            super::SERVICE_STATUS_OK
        );
        output.truncate(output_len);
        let (decoded_invocation_id, request_id, response): (
            PluginInvocationId,
            u64,
            ServiceResponse,
        ) = decode_service_envelope_with_invocation_id(&output, ServiceEnvelopeKind::Response)
            .expect("response should decode");
        assert_eq!(decoded_invocation_id, invocation_id);
        assert_eq!(request_id, 7);
        assert_eq!(response.payload, vec![1]);
    }
}

fn parse_binary_input<T>(
    input_ptr: *const u8,
    input_len: usize,
    null_code: i32,
    parse_code: i32,
) -> Result<T, i32>
where
    T: serde::de::DeserializeOwned,
{
    if input_ptr.is_null() {
        return Err(null_code);
    }
    let payload = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    crate::decode_service_message(payload).map_err(|_| parse_code)
}
