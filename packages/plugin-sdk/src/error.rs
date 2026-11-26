use crate::{ApiVersion, ServiceKind};
use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, PluginError>;

/// Machine-readable hint describing how to resolve a
/// [`PluginError::CapabilityAccessDenied`].
///
/// Hints are populated by the dispatcher when enough context is
/// available to suggest an actionable fix (most commonly: the
/// caller's `required_capabilities` list in `plugin.toml` is
/// missing the capability that was denied). The host is free to
/// surface this to the user in a log message, a developer-mode
/// toast, or a structured error payload.
///
/// The type is intentionally `Clone + Debug` and carries owned
/// strings so it can be cheaply propagated across layers without
/// borrowing from the original error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAccessDeniedHint {
    /// Short, user-facing description of the suggested fix.
    pub suggested_fix: String,
    /// Optional identifier of the missing manifest field
    /// (for tooling: e.g. `"required_capabilities"`).
    pub manifest_field: Option<String>,
}

impl CapabilityAccessDeniedHint {
    /// Build a hint that suggests declaring `capability` in the
    /// caller's manifest `required_capabilities` array.
    #[must_use]
    pub fn declare_required_capability(capability: &str) -> Self {
        Self {
            suggested_fix: format!(
                "declare '{capability}' in the caller plugin's required_capabilities in plugin.toml"
            ),
            manifest_field: Some("required_capabilities".to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("invalid plugin id '{id}'")]
    InvalidPluginId { id: String },

    #[error("duplicate plugin id '{id}'")]
    DuplicatePluginId { id: String },

    #[error("plugin '{plugin_id}' requests duplicate command '{command}'")]
    DuplicateCommand { plugin_id: String, command: String },

    #[error("plugin '{plugin_id}' has invalid CLI path for command '{command}'")]
    InvalidPluginCommandPath { plugin_id: String, command: String },

    #[error("plugin '{plugin_id}' has duplicate CLI alias entries for command '{command}'")]
    DuplicatePluginCommandAlias { plugin_id: String, command: String },

    #[error("invalid capability id '{capability}'")]
    InvalidCapabilityId { capability: String },

    #[error("invalid plugin feature '{feature}'")]
    InvalidPluginFeature { feature: String },

    #[error("plugin '{plugin_id}' both requires and provides capability '{capability}'")]
    CapabilitySelfRequirement {
        plugin_id: String,
        capability: String,
    },

    #[error(
        "plugin '{plugin_id}' registered invalid service interface for capability '{capability}' ({kind:?})"
    )]
    InvalidServiceInterfaceId {
        plugin_id: String,
        capability: String,
        kind: ServiceKind,
    },

    #[error(
        "plugin '{plugin_id}' declared reentrant operations on non-query service '{interface_id}' ({kind:?}); only query operations may be reentrant"
    )]
    InvalidReentrantService {
        plugin_id: String,
        interface_id: String,
        kind: ServiceKind,
    },

    #[error(
        "plugin '{plugin_id}' registered service '{interface_id}' for unowned capability '{capability}'"
    )]
    UnownedServiceCapability {
        plugin_id: String,
        capability: String,
        interface_id: String,
    },

    #[error("plugin '{plugin_id}' declares duplicate dependency '{dependency_id}'")]
    DuplicatePluginDependency {
        plugin_id: String,
        dependency_id: String,
    },

    #[error("plugin '{plugin_id}' cannot depend on itself")]
    PluginDependencyOnSelf { plugin_id: String },

    /// The plugin declared `bundled_only`: it needs host facilities plug-rt
    /// does not proxy into dynamic libraries, so it must ship bundled.
    #[error("plugin '{plugin_id}' is bundled-only and cannot load as a dynamic library: {reason}")]
    BundledOnlyPlugin { plugin_id: String, reason: String },

    /// The dynamic library cannot be served by the host runtime (not built in
    /// dynamic mode, plug-rt ABI mismatch, or loaded outside the runtime).
    #[error("plugin '{plugin_id}' cannot use the host runtime: {details}")]
    DynamicRuntimeUnavailable { plugin_id: String, details: String },

    #[error(
        "plugin '{plugin_id}' declares invalid dependency version requirement for '{dependency_id}': '{version_req}' ({details})"
    )]
    InvalidDependencyVersion {
        plugin_id: String,
        dependency_id: String,
        version_req: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' requires missing dependency '{dependency_id}'")]
    MissingRequiredDependency {
        plugin_id: String,
        dependency_id: String,
    },

    #[error(
        "plugin '{plugin_id}' requires dependency '{dependency_id}' matching '{version_req}', but found version '{found_version}'"
    )]
    IncompatibleDependencyVersion {
        plugin_id: String,
        dependency_id: String,
        version_req: String,
        found_version: String,
    },

    #[error("plugin dependency cycle detected: {cycle:?}")]
    PluginDependencyCycle { cycle: Vec<String> },

    #[error("plugin '{plugin_id}' is missing native entry path")]
    MissingEntryPath { plugin_id: String },

    #[error("plugin '{plugin_id}' entry file does not exist: {path:?}")]
    MissingEntryFile { plugin_id: String, path: PathBuf },

    #[error("plugin '{plugin_id}' process entry is not executable: {path:?}")]
    ProcessEntryNotExecutable { plugin_id: String, path: PathBuf },

    #[error("plugin '{plugin_id}' uses unsupported runtime '{runtime}'")]
    UnsupportedPluginRuntime { plugin_id: String, runtime: String },

    #[error("plugin '{plugin_id}' has invalid version range for {field}: {details}")]
    InvalidVersionRange {
        plugin_id: String,
        field: &'static str,
        details: String,
    },

    #[error("plugin '{plugin_id}' requires plugin API {required}, but host provides {host}")]
    IncompatibleApiVersion {
        plugin_id: String,
        required: String,
        host: ApiVersion,
    },

    #[error("plugin '{plugin_id}' requires native ABI {required}, but host provides {host}")]
    IncompatibleAbiVersion {
        plugin_id: String,
        required: String,
        host: ApiVersion,
    },

    #[error("plugin '{plugin_id}' requires missing capability '{capability}'")]
    MissingRequiredCapability {
        plugin_id: String,
        capability: String,
    },

    #[error("failed to spawn process plugin '{plugin_id}' command '{command}': {details}")]
    ProcessPluginSpawn {
        plugin_id: String,
        command: String,
        details: String,
    },

    #[error(
        "process plugin '{plugin_id}' command '{command}' timed out after {timeout_ms}ms ({details})"
    )]
    ProcessPluginTimeout {
        plugin_id: String,
        command: String,
        // Indirection also avoids imposing u128 alignment on every PluginError.
        timeout_ms: Box<u128>,
        details: String,
    },

    #[error("plugin '{plugin_id}' does not declare command '{command}'")]
    UnknownPluginCommand { plugin_id: String, command: String },

    #[error("plugin '{plugin_id}' lifecycle payload contains interior NUL")]
    InvalidNativeLifecycleInput { plugin_id: String },

    #[error("plugin '{plugin_id}' service invocation failed with status {status}")]
    NativeServiceInvocation { plugin_id: String, status: i32 },

    #[error("plugin '{plugin_id}' returned invalid service output: {details}")]
    InvalidNativeServiceOutput { plugin_id: String, details: String },

    #[error("service protocol error: {details}")]
    ServiceProtocol { details: String },

    #[error(
        "service provider '{provider_plugin_id}' for capability '{capability}' and interface '{interface_id}' is not available"
    )]
    MissingServiceProvider {
        provider_plugin_id: String,
        capability: String,
        interface_id: String,
    },

    #[error(
        "service call '{operation}' to '{provider_plugin_id}' failed for capability '{capability}' and interface '{interface_id}': [{code}] {message}"
    )]
    ServiceInvocationFailed {
        provider_plugin_id: Box<str>,
        capability: String,
        interface_id: String,
        operation: Box<str>,
        // Keep infrequent diagnostic allocations off the Result fast path.
        code: Box<str>,
        message: Box<str>,
    },

    /// Typed rejection before dispatch effects began. This does not describe
    /// cancellation after admission, rollback, or an unknown execution outcome.
    /// Native v3 maps this explicit classification to `abi::CANCELLED`; native
    /// v2 must retain its failed/unknown contract rather than reinterpret it.
    #[error("invocation cancelled before dispatch")]
    InvocationCancelled,

    /// The same typed classification as [`Self::InvocationCancelled`] (no
    /// dispatch effect began; native v3 maps it to `abi::CANCELLED`), carrying
    /// the host's reason, e.g. its selected connection being unavailable.
    #[error("rejected before dispatch: {details}")]
    RejectedBeforeDispatch { details: String },

    /// The host gave up waiting (for a worker slot or for a response), or the
    /// provider stopped at its caller's deadline, before the call completed. The request itself was not rejected; whether its
    /// effect happened may be unknown, so it must not be replayed blindly.
    #[error("host timed out: {details}")]
    HostTimeout { details: String },

    /// The host could not run the call now (its worker pool or executor is
    /// gone, or the response was lost). The request itself was not rejected.
    #[error("host unavailable: {details}")]
    HostUnavailable { details: String },

    #[error("unsupported host operation: {operation}")]
    UnsupportedHostOperation { operation: &'static str },

    #[error(
        "plugin '{plugin_id}' is not authorized for capability '{capability}' while performing '{operation}'"
    )]
    CapabilityAccessDenied {
        plugin_id: String,
        capability: String,
        operation: &'static str,
        /// Structured, machine-readable hint describing how to
        /// resolve this error (for example, by declaring the missing
        /// capability in the plugin's manifest). The `Display` impl
        /// intentionally does NOT include this field so formatted
        /// error strings remain stable for existing test assertions
        /// and log matchers; hosts wishing to render the hint should
        /// pattern-match on this variant.
        hint: Option<CapabilityAccessDeniedHint>,
    },

    #[error("failed to parse plugin manifest: {0}")]
    ManifestParse(#[source] Box<toml::de::Error>),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// Preserve `?` conversion while keeping the TOML parser's large diagnostic off
// every successful service Result's stack footprint.
impl From<toml::de::Error> for PluginError {
    fn from(error: toml::de::Error) -> Self {
        Self::ManifestParse(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_error_stays_small_for_generated_result_apis() {
        assert!(std::mem::size_of::<PluginError>() < 128);
    }

    #[test]
    fn boxed_service_diagnostics_keep_display_and_fields() {
        let error = PluginError::ServiceInvocationFailed {
            provider_plugin_id: "provider".into(),
            capability: "example.read".to_string(),
            interface_id: "state/v1".to_string(),
            operation: "get".into(),
            code: "denied".into(),
            message: "not authorized".into(),
        };
        assert_eq!(
            error.to_string(),
            "service call 'get' to 'provider' failed for capability 'example.read' and interface 'state/v1': [denied] not authorized"
        );
        let PluginError::ServiceInvocationFailed { code, message, .. } = error else {
            panic!("expected service failure");
        };
        assert_eq!(&*code, "denied");
        assert_eq!(&*message, "not authorized");
    }

    #[test]
    fn boxed_manifest_error_preserves_conversion_display_and_source() {
        let original = toml::from_str::<toml::Value>("broken = [").unwrap_err();
        let display = original.to_string();
        let error = PluginError::from(original);
        assert_eq!(
            error.to_string(),
            format!("failed to parse plugin manifest: {display}")
        );
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            display
        );
    }
}
