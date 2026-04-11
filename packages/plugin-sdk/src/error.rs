use crate::{ApiVersion, ServiceKind};
use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, PluginError>;

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

    #[error("plugin '{plugin_id}' provides duplicate capability '{capability}'")]
    DuplicateProvidedCapability {
        plugin_id: String,
        capability: String,
    },

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

    #[error("plugin '{plugin_id}' is registered as bundled-static but has no compiled vtable")]
    MissingStaticVtable { plugin_id: String },

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

    #[error(
        "capability '{capability}' has multiple providers: '{first_provider}' and '{second_provider}'"
    )]
    DuplicateCapabilityProvider {
        capability: String,
        first_provider: String,
        second_provider: String,
    },

    #[error(
        "service '{interface_id}' for capability '{capability}' and kind '{kind:?}' has multiple providers: '{first_provider}' and '{second_provider}'"
    )]
    DuplicateServiceProvider {
        capability: String,
        kind: ServiceKind,
        interface_id: String,
        first_provider: String,
        second_provider: String,
    },

    #[error("failed to load native plugin '{plugin_id}' from {path:?}: {details}")]
    NativeLibraryLoad {
        plugin_id: String,
        path: PathBuf,
        details: String,
    },

    #[error("failed to spawn process plugin '{plugin_id}' command '{command}': {details}")]
    ProcessPluginSpawn {
        plugin_id: String,
        command: String,
        details: String,
    },

    #[error("failed to resolve entry symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeEntrySymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' returned a null entry from symbol '{symbol}'")]
    NullPluginEntry { plugin_id: String, symbol: String },

    #[error("plugin '{plugin_id}' returned invalid UTF-8 entry text from '{symbol}'")]
    InvalidPluginEntryUtf8 { plugin_id: String, symbol: String },

    #[error("plugin '{plugin_id}' returned invalid entry from '{symbol}': {details}")]
    InvalidPluginEntry {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error(
        "plugin '{plugin_id}' embedded manifest field '{field}' does not match registered manifest (registered: {manifest_value}, embedded: {embedded_value})"
    )]
    ManifestMismatch {
        plugin_id: String,
        field: &'static str,
        manifest_value: String,
        embedded_value: String,
    },

    #[error("plugin '{plugin_id}' does not declare command '{command}'")]
    UnknownPluginCommand { plugin_id: String, command: String },

    #[error("failed to resolve command symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeCommandSymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' command input contains interior NUL in {field}")]
    InvalidNativeCommandInput {
        plugin_id: String,
        field: &'static str,
    },

    #[error("failed to resolve lifecycle symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeLifecycleSymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' lifecycle payload contains interior NUL")]
    InvalidNativeLifecycleInput { plugin_id: String },

    #[error("failed to resolve event symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeEventSymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' event payload contains interior NUL")]
    InvalidNativeEventInput { plugin_id: String },

    #[error("failed to resolve service symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeServiceSymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

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
        provider_plugin_id: String,
        capability: String,
        interface_id: String,
        operation: String,
        code: String,
        message: String,
    },

    #[error("unsupported host operation: {operation}")]
    UnsupportedHostOperation { operation: &'static str },

    #[error(
        "plugin '{plugin_id}' is not authorized for capability '{capability}' while performing '{operation}'"
    )]
    CapabilityAccessDenied {
        plugin_id: String,
        capability: String,
        operation: &'static str,
    },

    #[error("failed to parse plugin manifest: {0}")]
    ManifestParse(#[from] toml::de::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
