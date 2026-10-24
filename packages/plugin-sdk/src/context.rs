//! Plugin context types and the [`ServiceCaller`] trait.
//!
//! These types are passed to plugin methods by the host runtime.  The struct
//! fields carry everything a plugin might need — from the immediate command
//! name and arguments to the full service registry and host metadata.
//!
//! # Which fields matter?
//!
//! Most plugins only touch a handful of fields.  The rest are available for
//! advanced introspection or cross-plugin service calls.
//!
//! | Importance | Fields |
//! |------------|--------|
//! | **Always used** | `plugin_id`, `command`, `arguments` (commands) / `request` (services) |
//! | **For host API calls** | `services`, `host_kernel_bridge` (used internally by `HostRuntimeApi`) |
//! | **For introspection** | `registered_plugins`, `enabled_plugins`, `available_capabilities` |
//! | **Advanced** | `plugin_search_roots`, `settings`, `plugin_settings_map` |

use crate::{
    CancellationToken, HostConnectionInfo, HostMetadata, PluginError, RegisteredService, Result,
    ServiceRequest, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Serde helpers for toml::Value over binary codecs ─────────────────────────
//
// `toml::Value` requires `deserialize_any` which is unsupported by
// non-self-describing formats like bincode/bmux_codec.  These modules
// serialize values as JSON text strings so they survive the binary round-trip.

mod toml_value_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ref_option)] // serde `with` modules require `&T` for the field type
    pub fn serialize<S: Serializer>(
        value: &Option<toml::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let text: Option<String> = value
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(serde::ser::Error::custom)?;
        text.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<toml::Value>, D::Error> {
        let text: Option<String> = Option::deserialize(deserializer)?;
        text.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

mod toml_value_map {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<String, toml::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let text_map: BTreeMap<String, String> = map
            .iter()
            .map(|(k, v)| serde_json::to_string(v).map(|s| (k.clone(), s)))
            .collect::<Result<_, _>>()
            .map_err(serde::ser::Error::custom)?;
        text_map.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, toml::Value>, D::Error> {
        let text_map: BTreeMap<String, String> = BTreeMap::deserialize(deserializer)?;
        text_map
            .into_iter()
            .map(|(k, s)| {
                serde_json::from_str(&s)
                    .map(|v| (k, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// Standard service error code for cancellation.
pub const SERVICE_ERROR_CANCELLED: &str = "cancelled";

/// Serializable summary of a registered plugin, carried through command and
/// lifecycle contexts so plugins can introspect the full plugin registry
/// without re-scanning the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredPluginInfo {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub bundled_static: bool,
    pub required_capabilities: Vec<String>,
    pub provided_capabilities: Vec<String>,
    pub commands: Vec<String>,
    #[serde(default)]
    pub command_schemas: Vec<crate::PluginCommand>,
}

/// A keybinding that is active for the current command invocation context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveKeybinding {
    /// Binding scope, such as `global`, `scroll`, or the active mode id.
    pub scope: String,
    /// Human-readable chord, such as `Ctrl-A q`.
    pub chord: String,
    /// Canonical action string dispatched by this binding.
    pub action: String,
}

/// Verified auxiliary artifact observation. The checksum identifies the exact
/// bytes inspected by the host, not a trusted signing identity or launch lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAuxiliaryExecutable {
    pub path: String,
    pub protocol_version: u32,
    pub artifact_sha256: String,
}

/// Context passed to [`RustPlugin::activate`] and [`RustPlugin::deactivate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLifecycleContext {
    /// The plugin's own ID (e.g. `"bmux.clipboard"`).
    pub plugin_id: String,
    /// Capabilities this plugin declared as required in its manifest.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Capabilities this plugin provides to other plugins.
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    /// Registered services visible to this plugin for cross-plugin calls.
    #[serde(default)]
    pub services: Vec<RegisteredService>,
    /// All capabilities available in the current host environment.
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    /// IDs of all currently enabled plugins.
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    /// Filesystem roots where plugin manifests are discovered.
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    /// Summary of all registered plugins (for introspection).
    #[serde(default)]
    pub registered_plugins: Vec<RegisteredPluginInfo>,
    /// Host runtime metadata (product name, version, API version).
    pub host: HostMetadata,
    /// Host connection paths (config dir, runtime dir, data dir, state dir).
    pub connection: HostConnectionInfo,
    /// Plugin-specific settings from the host configuration.
    #[serde(default, with = "toml_value_option")]
    pub settings: Option<toml::Value>,
    /// Settings map for all plugins (keyed by plugin ID).
    #[serde(default, with = "toml_value_map")]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
    /// Opaque handle for dispatching calls to the host kernel (internal use).
    #[serde(default)]
    pub host_kernel_bridge: Option<HostKernelBridge>,
    /// Negotiated native ABI 1.1 owner-local artifact observations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auxiliary_executables: BTreeMap<String, ResolvedAuxiliaryExecutable>,
    /// Missing/invalid artifacts do not disable baseline activation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auxiliary_executable_errors: BTreeMap<String, String>,
}

/// Exact native ABI 1.0 lifecycle wire snapshot. Keep its field order and
/// adapters frozen; additions to `NativeLifecycleContext` must not alter this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLifecycleContextV1 {
    pub plugin_id: String,
    pub required_capabilities: Vec<String>,
    pub provided_capabilities: Vec<String>,
    pub services: Vec<RegisteredService>,
    pub available_capabilities: Vec<String>,
    pub enabled_plugins: Vec<String>,
    pub plugin_search_roots: Vec<String>,
    pub registered_plugins: Vec<RegisteredPluginInfo>,
    pub host: HostMetadata,
    pub connection: HostConnectionInfo,
    #[serde(default, with = "toml_value_option")]
    pub settings: Option<toml::Value>,
    #[serde(default, with = "toml_value_map")]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub host_kernel_bridge: Option<HostKernelBridge>,
}

impl NativeLifecycleContext {
    /// Explicit frozen snapshot for lifecycle dispatch to native ABI 1.0.
    #[must_use]
    pub fn legacy_wire_snapshot(&self) -> NativeLifecycleContextV1 {
        NativeLifecycleContextV1 {
            plugin_id: self.plugin_id.clone(),
            required_capabilities: self.required_capabilities.clone(),
            provided_capabilities: self.provided_capabilities.clone(),
            services: self.services.clone(),
            available_capabilities: self.available_capabilities.clone(),
            enabled_plugins: self.enabled_plugins.clone(),
            plugin_search_roots: self.plugin_search_roots.clone(),
            registered_plugins: self.registered_plugins.clone(),
            host: self.host.clone(),
            connection: self.connection.clone(),
            settings: self.settings.clone(),
            plugin_settings_map: self.plugin_settings_map.clone(),
            host_kernel_bridge: self.host_kernel_bridge.clone(),
        }
    }
}

/// Context passed to [`RustPlugin::run_command`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeCommandContext {
    /// The plugin's own ID (e.g. `"bmux.clipboard"`).
    pub plugin_id: String,
    /// The command name being invoked (e.g. `"hello"`, `"list-tabs"`).
    pub command: String,
    /// Positional and flag arguments passed to the command.
    pub arguments: Vec<String>,
    /// Capabilities this plugin declared as required in its manifest.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Capabilities this plugin provides to other plugins.
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    /// Registered services visible to this plugin for cross-plugin calls.
    #[serde(default)]
    pub services: Vec<RegisteredService>,
    /// All capabilities available in the current host environment.
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    /// IDs of all currently enabled plugins.
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    /// Filesystem roots where plugin manifests are discovered.
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    /// Summary of all registered plugins (for introspection).
    #[serde(default)]
    pub registered_plugins: Vec<RegisteredPluginInfo>,
    /// Keybindings active for this command invocation, if the host has an
    /// interactive keymap context.
    #[serde(default)]
    pub active_keybindings: Vec<ActiveKeybinding>,
    /// Host runtime metadata (product name, version, API version).
    pub host: HostMetadata,
    /// Host connection paths (config dir, runtime dir, data dir, state dir).
    pub connection: HostConnectionInfo,
    /// Plugin-specific settings from the host configuration.
    #[serde(default, with = "toml_value_option")]
    pub settings: Option<toml::Value>,
    /// Settings map for all plugins (keyed by plugin ID).
    #[serde(default, with = "toml_value_map")]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
    /// Identifier of the client that issued this command (the
    /// connected-client id, not a plugin id). `None` for
    /// server-internal invocations where no specific client is
    /// associated.
    #[serde(default)]
    pub caller_client_id: Option<uuid::Uuid>,
    /// How the host dispatched this command.
    ///
    /// Plugins use this to decide whether writing to `stdout` is
    /// safe. For [`NativeCommandInvocationSource::Cli`] the host
    /// stdout is the user's terminal in cooked mode and writes are
    /// appropriate. For
    /// [`NativeCommandInvocationSource::AttachKeybinding`] the host
    /// terminal is in raw mode rendering a TUI; direct writes would
    /// corrupt pane content and plugins should return their output as
    /// structured data (or stay silent and rely on host-side state
    /// observation).
    #[serde(default)]
    pub invocation_source: NativeCommandInvocationSource,
    /// Opaque handle for dispatching calls to the host kernel (internal use).
    #[serde(default)]
    pub host_kernel_bridge: Option<HostKernelBridge>,
}

/// How a plugin command was dispatched by the host.
///
/// This tells the plugin which surfaces are safe to write to:
/// `stdout`/`stderr` are TTY-safe for [`Self::Cli`], but the same
/// writes corrupt pane rendering when the host is in attach mode
/// ([`Self::AttachKeybinding`]).
///
/// The default is [`Self::Unknown`] so pre-existing serialized
/// contexts (or hosts that haven't been updated) round-trip
/// without changing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativeCommandInvocationSource {
    /// The invocation source was not communicated by the host. Plugins
    /// should treat this as "assume stdout may be a TTY in raw mode"
    /// and avoid direct writes for safety.
    #[default]
    Unknown,
    /// Dispatched from a standalone CLI invocation
    /// (`bmux <plugin> <command>`). The host stdout is an ordinary
    /// terminal or pipe; `println!` is appropriate.
    Cli,
    /// Positively identified standalone local process, not dispatched through a
    /// service, invoking transport, or attached client. Only the host assigns
    /// this value; absence of a caller or bridge alone is not this proof.
    LocalProcess,
    /// Dispatched from a keybinding while the host is rendering the
    /// attach TUI in raw mode. Plugins must not write to stdout/stderr.
    AttachKeybinding,
    /// Dispatched via host-internal automation (tests, bridges). The
    /// plugin should follow the same silence rules as
    /// [`Self::AttachKeybinding`].
    Internal,
}

impl NativeCommandInvocationSource {
    /// Whether the invocation uses standalone CLI presentation and interaction.
    ///
    /// This is a UX classification, not authorization or proof of local provenance.
    /// Security-sensitive callers must check trusted provenance independently.
    #[must_use]
    pub const fn is_cli(self) -> bool {
        matches!(self, Self::Cli | Self::LocalProcess)
    }
}

/// Context passed to [`RustPlugin::invoke_service`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeServiceContext {
    /// The plugin's own ID (e.g. `"bmux.clipboard"`).
    pub plugin_id: String,
    /// The inbound service request (interface ID, operation, payload).
    pub request: ServiceRequest,
    /// Capabilities this plugin declared as required in its manifest.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Capabilities this plugin provides to other plugins.
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    /// Registered services visible to this plugin for cross-plugin calls.
    #[serde(default)]
    pub services: Vec<RegisteredService>,
    /// All capabilities available in the current host environment.
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    /// IDs of all currently enabled plugins.
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    /// Filesystem roots where plugin manifests are discovered.
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    /// Host runtime metadata (product name, version, API version).
    pub host: HostMetadata,
    /// Host connection paths (config dir, runtime dir, data dir, state dir).
    pub connection: HostConnectionInfo,
    /// Plugin-specific settings from the host configuration.
    #[serde(default, with = "toml_value_option")]
    pub settings: Option<toml::Value>,
    /// Settings map for all plugins (keyed by plugin ID).
    #[serde(default, with = "toml_value_map")]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
    /// Identifier of the client that issued this service request (the
    /// connected-client id, not a plugin id). `None` for
    /// server-internal invocations where no specific client is
    /// associated (e.g. CLI subcommands without an attached session).
    #[serde(default)]
    pub caller_client_id: Option<uuid::Uuid>,
    /// Cancellation state for this service invocation.
    #[serde(default)]
    pub cancellation: CancellationToken,
    /// Opaque handle for dispatching calls to the host kernel (internal use).
    #[serde(default)]
    pub host_kernel_bridge: Option<HostKernelBridge>,
}

/// Context passed to streaming service handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeStreamingServiceContext {
    /// Base service invocation context.
    pub service: NativeServiceContext,
    /// Sink used by streaming handlers to emit request-scoped event frames.
    #[serde(skip, default = "crate::ServiceEventSinkHandle::noop")]
    pub events: crate::ServiceEventSinkHandle,
}

// ── Host kernel bridge (opaque FFI handle) ───────────────────────────────────

type HostKernelBridgeFn = unsafe extern "C" fn(
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32;

/// Opaque handle to a host kernel bridge function pointer.
///
/// Used internally by the service dispatch machinery. Plugin authors
/// do not interact with this type directly.
///
/// Deserialize only trusted host context in the same address space: this
/// representation contains executable callbacks, never untrusted wire metadata.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum HostKernelBridge {
    Legacy(u64),
    Bound {
        version: u16,
        pointer: u64,
        token: uuid::Uuid,
        ownership: u64,
    },
}

// The ownership callback has a C ABI and accepts only opaque token bytes. It
// never receives Rust pointers, Arc layout, or caller identity metadata.
type BridgeOwnershipFn = unsafe extern "C" fn(*const u8, bool) -> bool;

impl HostKernelBridge {
    fn ownership(&self, retain: bool) -> bool {
        match self {
            Self::Legacy(_) => true,
            Self::Bound {
                token,
                ownership,
                version: 1,
                ..
            } => {
                let Ok(pointer) = usize::try_from(*ownership) else {
                    return false;
                };
                if pointer == 0 {
                    return false;
                }
                let callback: BridgeOwnershipFn = unsafe { std::mem::transmute(pointer) };
                unsafe { callback(token.as_bytes().as_ptr(), retain) }
            }
            Self::Bound { .. } => false,
        }
    }
}

impl Clone for HostKernelBridge {
    fn clone(&self) -> Self {
        if !self.ownership(true) {
            return Self::Legacy(0);
        }
        match self {
            Self::Legacy(pointer) => Self::Legacy(*pointer),
            Self::Bound {
                version,
                pointer,
                token,
                ownership,
            } => Self::Bound {
                version: *version,
                pointer: *pointer,
                token: *token,
                ownership: *ownership,
            },
        }
    }
}

impl Drop for HostKernelBridge {
    fn drop(&mut self) {
        let _ = self.ownership(false);
    }
}

impl<'de> Deserialize<'de> for HostKernelBridge {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Legacy(u64),
            Bound {
                version: u16,
                pointer: u64,
                token: uuid::Uuid,
                ownership: u64,
            },
        }
        let bridge = std::mem::ManuallyDrop::new(match Wire::deserialize(deserializer)? {
            Wire::Legacy(pointer) => Self::Legacy(pointer),
            Wire::Bound {
                version: 1,
                pointer,
                token,
                ownership,
            } => Self::Bound {
                version: 1,
                pointer,
                token,
                ownership,
            },
            Wire::Bound { .. } => {
                return Err(serde::de::Error::custom("unsupported bound bridge version"));
            }
        });
        if !bridge.ownership(true) {
            return Err(serde::de::Error::custom("expired bound bridge"));
        }
        Ok(std::mem::ManuallyDrop::into_inner(bridge))
    }
}

impl HostKernelBridge {
    #[must_use]
    pub fn from_fn(pointer: HostKernelBridgeFn) -> Self {
        Self::Legacy(pointer as usize as u64)
    }

    /// Bind an invocation to an opaque host-owned registration.
    #[must_use]
    pub fn bound(
        pointer: HostKernelBridgeFn,
        token: uuid::Uuid,
        ownership: BridgeOwnershipFn,
    ) -> Self {
        let bridge = std::mem::ManuallyDrop::new(Self::Bound {
            version: 1,
            pointer: pointer as usize as u64,
            token,
            ownership: ownership as usize as u64,
        });
        if bridge.ownership(true) {
            std::mem::ManuallyDrop::into_inner(bridge)
        } else {
            Self::Legacy(0)
        }
    }

    #[must_use]
    pub const fn token(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Legacy(_) => None,
            Self::Bound { token, .. } => Some(*token),
        }
    }

    /// Invoke the kernel bridge function pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure the bridge pointer is still valid (i.e. the host
    /// process has not been terminated or the function unmapped).
    pub fn invoke(
        &self,
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        // pointer was stored as u64 for serialization; fits in usize on supported 64-bit targets
        let pointer = match self {
            Self::Legacy(pointer)
            | Self::Bound {
                version: 1,
                pointer,
                ..
            } => pointer,
            Self::Bound { .. } => return 2,
        };
        let Ok(pointer) = usize::try_from(*pointer) else {
            return 2;
        };
        if pointer == 0 {
            return 2;
        }
        let bridge: HostKernelBridgeFn = unsafe { std::mem::transmute(pointer) };
        unsafe {
            bridge(
                input_ptr,
                input_len,
                output_ptr,
                output_capacity,
                output_len,
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKernelBridgeRequest {
    /// Opaque host-issued invocation capability, not a claimed identity.
    #[serde(default)]
    pub context_token: Option<uuid::Uuid>,
    /// Opaque invocation identity used to replay an oversized response without
    /// re-executing a side-effecting host operation.
    #[serde(default)]
    pub invocation_id: u64,
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

impl HostKernelBridgeRequest {
    #[must_use]
    pub fn new(payload: Vec<u8>) -> Self {
        static NEXT_INVOCATION_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        Self {
            context_token: None,
            invocation_id: NEXT_INVOCATION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKernelBridgeResponse {
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

/// Capability required for host-dispatched core CLI command execution.
pub const CORE_CLI_COMMAND_CAPABILITY: &str = "bmux.commands";
/// Service interface for host-dispatched core CLI command execution.
pub const CORE_CLI_COMMAND_INTERFACE_V1: &str = "cli-command/v1";
/// Service operation for executing a core CLI command path.
pub const CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1: &str = "run_path";
/// Service operation for executing a plugin command.
pub const CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1: &str = "run_plugin";
/// Marker prefix for host-kernel bridge CLI command payloads.
pub const CORE_CLI_BRIDGE_MAGIC_V1: &[u8] = b"BMUXCMD1";
/// Marker prefix for host-kernel bridge plugin command payloads.
pub const PLUGIN_CLI_BRIDGE_MAGIC_V1: &[u8] = b"BMUXPLG1";
/// Protocol version for `CoreCliCommandRequest`/`CoreCliCommandResponse`.
pub const CORE_CLI_BRIDGE_PROTOCOL_V1: u16 = 1;

pub use crate::cli_bridge::cli_command::client::{
    RunPathRequest as CoreCliCommandRequest, RunPluginRequest as PluginCliCommandRequest,
};
pub use crate::cli_bridge::cli_command::{CoreCliCommandResponse, PluginCliCommandResponse};

impl CoreCliCommandRequest {
    #[must_use]
    pub const fn new(command_path: Vec<String>, arguments: Vec<String>) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            command_path,
            arguments,
        }
    }
}

impl PluginCliCommandRequest {
    #[must_use]
    pub const fn new(plugin_id: String, command_name: String, arguments: Vec<String>) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            plugin_id,
            command_name,
            arguments,
        }
    }
}

impl PluginCliCommandResponse {
    #[must_use]
    pub const fn new(exit_code: i32) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            exit_code,
            error: None,
        }
    }

    #[must_use]
    pub const fn failed(exit_code: i32, error: String) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            exit_code,
            error: Some(error),
        }
    }
}

impl CoreCliCommandResponse {
    #[must_use]
    pub const fn new(exit_code: i32) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            exit_code,
            error: None,
        }
    }

    #[must_use]
    pub const fn failed(exit_code: i32, error: String) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            exit_code,
            error: Some(error),
        }
    }
}

/// Encode a host-kernel bridge payload representing an in-process core CLI
/// command invocation request.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_host_kernel_bridge_cli_command_payload(
    request: &CoreCliCommandRequest,
) -> Result<Vec<u8>> {
    let mut payload = CORE_CLI_BRIDGE_MAGIC_V1.to_vec();
    payload.extend(encode_service_message(request)?);
    Ok(payload)
}

/// Decode a host-kernel bridge payload for in-process core CLI command
/// invocation.
///
/// Returns `Ok(None)` when the payload is not a CLI-command bridge payload.
///
/// # Errors
///
/// Returns an error when the payload has the CLI prefix but cannot be decoded.
pub fn decode_host_kernel_bridge_cli_command_payload(
    payload: &[u8],
) -> Result<Option<CoreCliCommandRequest>> {
    if !payload.starts_with(CORE_CLI_BRIDGE_MAGIC_V1) {
        return Ok(None);
    }
    let encoded = &payload[CORE_CLI_BRIDGE_MAGIC_V1.len()..];
    let request: CoreCliCommandRequest = decode_service_message(encoded)?;
    if request.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported core CLI bridge request protocol version: {}",
                request.protocol_version
            ),
        });
    }
    Ok(Some(request))
}

/// Encode a host-kernel bridge payload representing an in-process plugin
/// command invocation request.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode_host_kernel_bridge_plugin_command_payload(
    request: &PluginCliCommandRequest,
) -> Result<Vec<u8>> {
    let mut payload = PLUGIN_CLI_BRIDGE_MAGIC_V1.to_vec();
    payload.extend(encode_service_message(request)?);
    Ok(payload)
}

/// Decode a host-kernel bridge payload for in-process plugin command
/// invocation.
///
/// Returns `Ok(None)` when the payload is not a plugin-command bridge payload.
///
/// # Errors
///
/// Returns an error when the payload has the plugin prefix but cannot be decoded.
pub fn decode_host_kernel_bridge_plugin_command_payload(
    payload: &[u8],
) -> Result<Option<PluginCliCommandRequest>> {
    if !payload.starts_with(PLUGIN_CLI_BRIDGE_MAGIC_V1) {
        return Ok(None);
    }
    let encoded = &payload[PLUGIN_CLI_BRIDGE_MAGIC_V1.len()..];
    let request: PluginCliCommandRequest = decode_service_message(encoded)?;
    if request.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported plugin CLI bridge request protocol version: {}",
                request.protocol_version
            ),
        });
    }
    Ok(Some(request))
}

#[cfg(test)]
mod tests {
    #[test]
    fn invocation_source_cli_classification_preserves_non_cli_silence() {
        use super::NativeCommandInvocationSource;
        assert!(NativeCommandInvocationSource::Cli.is_cli());
        assert!(NativeCommandInvocationSource::LocalProcess.is_cli());
        for source in [
            NativeCommandInvocationSource::Unknown,
            NativeCommandInvocationSource::AttachKeybinding,
            NativeCommandInvocationSource::Internal,
        ] {
            assert!(!source.is_cli());
        }
    }

    use super::{
        CORE_CLI_BRIDGE_PROTOCOL_V1, CoreCliCommandRequest, HostKernelBridgeRequest,
        HostKernelBridgeResponse, PluginCliCommandRequest,
        decode_host_kernel_bridge_cli_command_payload,
        decode_host_kernel_bridge_plugin_command_payload,
        encode_host_kernel_bridge_cli_command_payload,
        encode_host_kernel_bridge_plugin_command_payload,
    };

    #[test]
    fn bound_bridge_ownership_acquisition_release_and_failures() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        static OWNERS: AtomicUsize = AtomicUsize::new(0);
        static ALLOW: AtomicBool = AtomicBool::new(true);
        unsafe extern "C" fn ownership(_: *const u8, retain: bool) -> bool {
            if retain {
                if !ALLOW.load(Ordering::SeqCst) {
                    return false;
                }
                OWNERS.fetch_add(1, Ordering::SeqCst);
            } else {
                assert!(OWNERS.fetch_sub(1, Ordering::SeqCst) > 0);
            }
            true
        }
        unsafe extern "C" fn invoke(
            _: *const u8,
            _: usize,
            _: *mut u8,
            _: usize,
            _: *mut usize,
        ) -> i32 {
            0
        }
        let bridge = super::HostKernelBridge::bound(invoke, uuid::Uuid::from_u128(1), ownership);
        assert_eq!(OWNERS.load(Ordering::SeqCst), 1);
        let cloned = bridge.clone();
        let bytes = super::encode_service_message(&bridge).unwrap();
        let decoded: super::HostKernelBridge = super::decode_service_message(&bytes).unwrap();
        assert_eq!(OWNERS.load(Ordering::SeqCst), 3);
        ALLOW.store(false, Ordering::SeqCst);
        let failed = bridge.clone();
        assert!(super::decode_service_message::<super::HostKernelBridge>(&bytes).is_err());
        drop(failed);
        assert_eq!(OWNERS.load(Ordering::SeqCst), 3);
        drop(bridge);
        drop(cloned);
        assert_eq!(OWNERS.load(Ordering::SeqCst), 1);
        drop(decoded);
        assert_eq!(OWNERS.load(Ordering::SeqCst), 0);
        let failed = super::HostKernelBridge::bound(invoke, uuid::Uuid::from_u128(1), ownership);
        drop(failed);
        assert_eq!(OWNERS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn host_kernel_bridge_request_preserves_raw_payload_bytes() {
        let request = HostKernelBridgeRequest::new(vec![0, 1, 2, 13, 14, 17, 255]);
        let encoded = super::encode_service_message(&request).expect("request should encode");
        let decoded: HostKernelBridgeRequest =
            super::decode_service_message(&encoded).expect("request should decode");
        assert_ne!(request.invocation_id, 0);
        assert_eq!(decoded, request);
    }

    #[test]
    fn host_kernel_bridge_request_decodes_legacy_envelope_without_invocation_id() {
        #[derive(serde::Serialize)]
        struct LegacyRequest {
            #[serde(with = "bmux_codec::serde_bytes_vec")]
            payload: Vec<u8>,
        }
        let encoded = super::encode_service_message(&LegacyRequest {
            payload: vec![1, 2, 3],
        })
        .expect("legacy request should encode");
        let decoded: HostKernelBridgeRequest =
            super::decode_service_message(&encoded).expect("legacy request should decode");
        assert_eq!(decoded.invocation_id, 0);
        assert_eq!(decoded.payload, vec![1, 2, 3]);
    }

    #[test]
    fn host_kernel_bridge_request_ids_are_unique() {
        let first = HostKernelBridgeRequest::new(Vec::new());
        let second = HostKernelBridgeRequest::new(Vec::new());
        assert_ne!(first.invocation_id, second.invocation_id);
    }

    #[test]
    fn host_kernel_bridge_response_preserves_raw_payload_bytes() {
        let response = HostKernelBridgeResponse {
            payload: vec![0, 1, 2, 13, 14, 17, 255],
        };
        let encoded = super::encode_service_message(&response).expect("response should encode");
        let decoded: HostKernelBridgeResponse =
            super::decode_service_message(&encoded).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn host_kernel_bridge_cli_request_nested_payload_round_trip() {
        let command = CoreCliCommandRequest::new(
            vec!["playbook".to_string(), "validate".to_string()],
            vec!["--json".to_string(), "fixture.dsl".to_string()],
        );
        let command_payload = encode_host_kernel_bridge_cli_command_payload(&command)
            .expect("command payload should encode");
        let bridge_request = HostKernelBridgeRequest::new(command_payload);
        let encoded =
            super::encode_service_message(&bridge_request).expect("bridge request should encode");
        let decoded: HostKernelBridgeRequest =
            super::decode_service_message(&encoded).expect("bridge request should decode");
        let decoded_command = decode_host_kernel_bridge_cli_command_payload(&decoded.payload)
            .expect("nested command payload should decode")
            .expect("nested command payload should be recognized");
        assert_eq!(decoded_command.protocol_version, command.protocol_version);
        assert_eq!(decoded_command.command_path, command.command_path);
        assert_eq!(decoded_command.arguments, command.arguments);
    }

    #[test]
    fn cli_bridge_payload_round_trip_preserves_request() {
        let request = CoreCliCommandRequest::new(
            vec!["logs".to_string(), "path".to_string()],
            vec!["--json".to_string()],
        );
        let encoded =
            encode_host_kernel_bridge_cli_command_payload(&request).expect("request should encode");
        let decoded = decode_host_kernel_bridge_cli_command_payload(&encoded)
            .expect("payload should decode")
            .expect("payload should be recognized");
        assert_eq!(decoded.protocol_version, request.protocol_version);
        assert_eq!(decoded.command_path, request.command_path);
        assert_eq!(decoded.arguments, request.arguments);
    }

    #[test]
    fn cli_bridge_payload_ignores_unknown_prefix() {
        let decoded = decode_host_kernel_bridge_cli_command_payload(b"not-a-cli-bridge-payload")
            .expect("decode should succeed");
        assert!(decoded.is_none());
    }

    #[test]
    fn cli_bridge_payload_rejects_unsupported_protocol_version() {
        let mut request = CoreCliCommandRequest::new(Vec::new(), Vec::new());
        request.protocol_version = CORE_CLI_BRIDGE_PROTOCOL_V1 + 1;
        let encoded =
            encode_host_kernel_bridge_cli_command_payload(&request).expect("request should encode");
        let error = decode_host_kernel_bridge_cli_command_payload(&encoded)
            .expect_err("decode should fail for unsupported protocol version");
        assert!(
            error
                .to_string()
                .contains("unsupported core CLI bridge request protocol version")
        );
    }

    #[test]
    fn core_cli_response_failed_carries_error() {
        let response = super::CoreCliCommandResponse::failed(7, "boom".to_string());
        let encoded = super::encode_service_message(&response).expect("response should encode");
        let decoded: super::CoreCliCommandResponse =
            super::decode_service_message(&encoded).expect("response should decode");

        assert_eq!(decoded.exit_code, 7);
        assert_eq!(decoded.error.as_deref(), Some("boom"));
    }

    #[test]
    fn plugin_cli_bridge_payload_round_trip_preserves_request() {
        let request = PluginCliCommandRequest::new(
            "bmux.tabs".to_string(),
            "new-tab".to_string(),
            vec!["--name".to_string(), "work".to_string()],
        );
        let encoded = encode_host_kernel_bridge_plugin_command_payload(&request)
            .expect("request should encode");
        let decoded = decode_host_kernel_bridge_plugin_command_payload(&encoded)
            .expect("payload should decode")
            .expect("payload should be recognized");
        assert_eq!(decoded.protocol_version, request.protocol_version);
        assert_eq!(decoded.plugin_id, request.plugin_id);
        assert_eq!(decoded.command_name, request.command_name);
        assert_eq!(decoded.arguments, request.arguments);
    }

    #[test]
    fn plugin_cli_bridge_payload_ignores_unknown_prefix() {
        let decoded =
            decode_host_kernel_bridge_plugin_command_payload(b"not-a-plugin-bridge-payload")
                .expect("decode should succeed");
        assert!(decoded.is_none());
    }

    #[test]
    fn plugin_cli_bridge_payload_rejects_unsupported_protocol_version() {
        let mut request = PluginCliCommandRequest::new(
            "bmux.tabs".to_string(),
            "new-tab".to_string(),
            Vec::new(),
        );
        request.protocol_version = CORE_CLI_BRIDGE_PROTOCOL_V1 + 1;
        let encoded = encode_host_kernel_bridge_plugin_command_payload(&request)
            .expect("request should encode");
        let error = decode_host_kernel_bridge_plugin_command_payload(&encoded)
            .expect_err("decode should fail for unsupported protocol version");
        assert!(
            error
                .to_string()
                .contains("unsupported plugin CLI bridge request protocol version")
        );
    }
}
