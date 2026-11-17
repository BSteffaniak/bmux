//! Types shared by plugin authors and hosts: command errors and exit codes,
//! the typed-service registration context and the activation's task scope.
use std::future::Future;

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

/// Host and transport failures (`crate::Result`) surfaced to author code, so
/// handlers and tests `?` both error types.
impl From<crate::PluginError> for PluginCommandError {
    fn from(error: crate::PluginError) -> Self {
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

/// Context passed to typed-service registration
/// ([`crate::author::Declarations::typed_services`]).
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
