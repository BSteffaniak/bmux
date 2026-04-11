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
    HostConnectionInfo, HostMetadata, PluginError, RegisteredService, Result, ServiceRequest,
    decode_service_message, encode_service_message,
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
}

/// Context passed to [`RustPlugin::run_command`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeCommandContext {
    /// The plugin's own ID (e.g. `"bmux.clipboard"`).
    pub plugin_id: String,
    /// The command name being invoked (e.g. `"hello"`, `"list-windows"`).
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
    /// Opaque handle for dispatching calls to the host kernel (internal use).
    #[serde(default)]
    pub host_kernel_bridge: Option<HostKernelBridge>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostKernelBridge(u64);

impl HostKernelBridge {
    #[must_use]
    pub fn from_fn(pointer: HostKernelBridgeFn) -> Self {
        Self(pointer as usize as u64)
    }

    /// Invoke the kernel bridge function pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure the bridge pointer is still valid (i.e. the host
    /// process has not been terminated or the function unmapped).
    pub fn invoke(
        self,
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        // pointer was stored as u64 for serialization; fits in usize on supported 64-bit targets
        let bridge: HostKernelBridgeFn = unsafe { std::mem::transmute(self.0 as usize) };
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
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKernelBridgeResponse {
    pub payload: Vec<u8>,
}

/// Capability required for host-dispatched core CLI command execution.
pub const CORE_CLI_COMMAND_CAPABILITY: &str = "bmux.commands";
/// Service interface for host-dispatched core CLI command execution.
pub const CORE_CLI_COMMAND_INTERFACE_V1: &str = "cli-command/v1";
/// Service operation for executing a core CLI command path.
pub const CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1: &str = "run_path";
/// Marker prefix for host-kernel bridge CLI command payloads.
pub const CORE_CLI_BRIDGE_MAGIC_V1: &[u8] = b"BMUXCMD1";
/// Protocol version for `CoreCliCommandRequest`/`CoreCliCommandResponse`.
pub const CORE_CLI_BRIDGE_PROTOCOL_V1: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreCliCommandRequest {
    pub protocol_version: u16,
    pub command_path: Vec<String>,
    pub arguments: Vec<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreCliCommandResponse {
    pub protocol_version: u16,
    pub exit_code: i32,
}

impl CoreCliCommandResponse {
    #[must_use]
    pub const fn new(exit_code: i32) -> Self {
        Self {
            protocol_version: CORE_CLI_BRIDGE_PROTOCOL_V1,
            exit_code,
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

#[cfg(test)]
mod tests {
    use super::{
        CORE_CLI_BRIDGE_PROTOCOL_V1, CoreCliCommandRequest,
        decode_host_kernel_bridge_cli_command_payload,
        encode_host_kernel_bridge_cli_command_payload,
    };

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
        assert_eq!(decoded, request);
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
}
