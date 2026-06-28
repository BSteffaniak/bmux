//! Temporary Bcode manifest/protocol/symbol compatibility adapter.
//!
//! TODO(bcode-convergence): remove this module after Bcode plugins migrate to
//! canonical BMUX manifest, BPDL service, and symbol ABIs.
//!
//! This module is migration-only. BMUX's canonical manifest remains
//! [`crate::PluginManifest`]; Bcode-only fields are adapted into generic
//! contributions/extensions here and must not be added to core manifest
//! structs.

use crate::{
    PluginManifest, PluginManifestCompatibility, PluginManifestKeybindings, PluginRuntime,
};
use bmux_plugin_sdk::{PluginCommand, PluginContribution, PluginError, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BcodeManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub concurrency: bmux_plugin_runtime::PluginConcurrencyConfig,
    #[serde(default)]
    pub services: Vec<bmux_plugin_sdk::PluginService>,
    #[serde(default)]
    pub event_subscriptions: Vec<bmux_plugin_sdk::PluginEventSubscription>,
    #[serde(default)]
    pub command_contributions: Vec<PluginCommand>,
    #[serde(default)]
    pub tui_surfaces: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub agent_defaults: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub config: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedBcodeManifest {
    pub manifest: PluginManifest,
    pub contributions: Vec<PluginContribution>,
    pub compatibility_payloads: BTreeMap<String, toml::Value>,
}

/// Parse a Bcode manifest and adapt it into canonical BMUX manifest data.
///
/// # Errors
///
/// Returns when the source is not valid supported Bcode compatibility TOML.
pub fn parse_bcode_manifest(source: &str) -> Result<AdaptedBcodeManifest> {
    let bcode: BcodeManifest = toml::from_str(source)?;
    let mut manifest = PluginManifest {
        id: bcode.id,
        name: bcode.name,
        version: bcode.version,
        description: None,
        homepage: None,
        provider_priority: 0,
        execution_class: crate::PluginExecutionClass::default(),
        concurrency: bcode.concurrency,
        owns_namespaces: BTreeSet::default(),
        owns_paths: BTreeSet::default(),
        runtime: PluginRuntime::Native,
        entry: None,
        entry_args: Vec::new(),
        process_persistent_worker: false,
        entry_symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
        plugin_api: PluginManifestCompatibility::default(),
        native_abi: PluginManifestCompatibility::default(),
        required_capabilities: BTreeSet::default(),
        provided_capabilities: BTreeSet::default(),
        provided_features: BTreeSet::default(),
        services: bcode.services,
        commands: Vec::new(),
        event_subscriptions: bcode.event_subscriptions,
        event_publications: Vec::new(),
        extensions: Vec::new(),
        dependencies: Vec::new(),
        keybindings: PluginManifestKeybindings::default(),
        ready_signals: Vec::new(),
    };

    if !bcode.tui_surfaces.is_empty() {
        manifest.extensions.push(crate::PluginManifestExtension {
            extension_point: "bcode.tui_surface/v1".to_string(),
            payload: toml::to_string(&bcode.tui_surfaces)
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!(
                        "failed to encode bcode tui surface compatibility payload: {error}"
                    ),
                })?
                .into_bytes(),
        });
    }

    let compatibility_payloads = [
        ("bcode.agent_defaults".to_string(), bcode.agent_defaults),
        ("bcode.config".to_string(), bcode.config),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        (!value.is_empty()).then(|| (name, toml::Value::Table(value.into_iter().collect())))
    })
    .collect();

    let contributions = bcode
        .command_contributions
        .into_iter()
        .map(PluginContribution::command)
        .collect();

    Ok(AdaptedBcodeManifest {
        manifest,
        contributions,
        compatibility_payloads,
    })
}

pub const BCODE_PLUGIN_MANIFEST_SYMBOL: &str = "bcode_plugin_manifest_v1";
pub const BCODE_PLUGIN_ACTIVATE_SYMBOL: &str = "bcode_plugin_activate_v1";
pub const BCODE_PLUGIN_REGISTER_COMMANDS_SYMBOL: &str = "bcode_plugin_register_commands_v1";
pub const BCODE_PLUGIN_DEACTIVATE_SYMBOL: &str = "bcode_plugin_deactivate_v1";
pub const BCODE_PLUGIN_INVOKE_SERVICE_SYMBOL: &str = "bcode_plugin_invoke_service_v1";
pub const BCODE_PLUGIN_INVOKE_STREAMING_SERVICE_SYMBOL: &str =
    "bcode_plugin_invoke_service_streaming_v1";
pub const BCODE_PLUGIN_HANDLE_EVENT_SYMBOL: &str = "bcode_plugin_handle_event_v1";

pub const SERVICE_RESPONSE_CHUNK_PREFIX: &[u8] = b"bcode.internal.service_response_chunk.v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BcodeSymbolFallback {
    pub canonical: &'static str,
    pub bcode: &'static str,
}

#[must_use]
pub fn symbol_fallback(canonical: &'static str) -> Option<BcodeSymbolFallback> {
    match canonical {
        bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_MANIFEST_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_ACTIVATE_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_ACTIVATE_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_DEACTIVATE_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_DEACTIVATE_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_SERVICE_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_INVOKE_SERVICE_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_STREAMING_SERVICE_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_INVOKE_STREAMING_SERVICE_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_EVENT_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_HANDLE_EVENT_SYMBOL,
        }),
        crate::DEFAULT_NATIVE_REGISTER_CONTRIBUTIONS_SYMBOL => Some(BcodeSymbolFallback {
            canonical,
            bcode: BCODE_PLUGIN_REGISTER_COMMANDS_SYMBOL,
        }),
        _ => None,
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BcodeJsonServiceContext {
    plugin_id: String,
    caller_plugin_id: String,
    interface_id: String,
    operation: String,
    payload: Vec<u8>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BcodeJsonServiceResponse {
    payload: Vec<u8>,
    #[serde(default)]
    error: Option<String>,
}

/// Encode canonical BMUX service context as temporary Bcode JSON ABI bytes.
///
/// # Errors
///
/// Returns if JSON serialization fails.
pub fn encode_bcode_service_context(
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<Vec<u8>> {
    let json = BcodeJsonServiceContext {
        plugin_id: context.plugin_id.clone(),
        caller_plugin_id: context.request.caller_plugin_id.clone(),
        interface_id: context.request.service.interface_id.clone(),
        operation: context.request.operation.clone(),
        payload: context.request.payload.clone(),
    };
    serde_json::to_vec(&json).map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: format!("failed to encode bcode service context: {error}"),
    })
}

/// Decode temporary Bcode JSON ABI service response into canonical BMUX response.
///
/// # Errors
///
/// Returns if JSON deserialization fails.
pub fn decode_bcode_service_response(bytes: &[u8]) -> Result<bmux_plugin_sdk::ServiceResponse> {
    let response: BcodeJsonServiceResponse = serde_json::from_slice(bytes).map_err(|error| {
        bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: format!("failed to decode bcode service response: {error}"),
        }
    })?;
    Ok(match response.error {
        Some(message) => bmux_plugin_sdk::ServiceResponse {
            payload: response.payload,
            error: Some(bmux_plugin_sdk::ServiceError {
                code: "bcode_error".to_string(),
                message,
            }),
        },
        None => bmux_plugin_sdk::ServiceResponse::ok(response.payload),
    })
}

/// Decode temporary Bcode command registration JSON into canonical contributions.
///
/// # Errors
///
/// Returns if JSON deserialization fails.
pub fn decode_bcode_command_contributions(bytes: &[u8]) -> Result<Vec<PluginContribution>> {
    let commands: Vec<PluginCommand> = serde_json::from_slice(bytes).map_err(|error| {
        bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: format!("failed to decode bcode command registrations: {error}"),
        }
    })?;
    Ok(commands
        .into_iter()
        .map(PluginContribution::command)
        .collect())
}

#[must_use]
pub fn reassemble_bcode_response_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut output = Vec::new();
    for chunk in chunks {
        if let Some(payload) = chunk.strip_prefix(SERVICE_RESPONSE_CHUNK_PREFIX) {
            output.extend_from_slice(payload);
        } else {
            output.extend_from_slice(chunk);
        }
    }
    output
}

#[cfg(test)]
mod protocol_tests {
    use super::{
        SERVICE_RESPONSE_CHUNK_PREFIX, decode_bcode_service_response, encode_bcode_service_context,
        parse_bcode_manifest, reassemble_bcode_response_chunks, symbol_fallback,
    };
    use bmux_plugin_sdk::{
        HostConnectionInfo, HostMetadata, NativeServiceContext, ProviderId, RegisteredService,
        ServiceKind, ServiceRequest,
    };
    use std::collections::BTreeMap;

    #[test]
    fn bcode_symbol_fallbacks_are_declared() {
        assert_eq!(
            symbol_fallback(bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL)
                .expect("entry fallback")
                .bcode,
            "bcode_plugin_manifest_v1"
        );
        assert_eq!(
            symbol_fallback(crate::DEFAULT_NATIVE_SERVICE_SYMBOL)
                .expect("service fallback")
                .bcode,
            "bcode_plugin_invoke_service_v1"
        );
    }

    #[test]
    fn bcode_codec_adapter_translates_service_request_response() {
        let context = NativeServiceContext {
            plugin_id: "bcode.test".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "caller".to_string(),
                service: RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bcode.test.service")
                        .expect("scope"),
                    kind: ServiceKind::Command,
                    interface_id: "bcode.test/v1".to_string(),
                    provider: ProviderId::Plugin("bcode.test".to_string()),
                },
                operation: "run".to_string(),
                payload: vec![1, 2, 3],
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "test".to_string(),
                product_version: "0".to_string(),
                plugin_api_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
                plugin_abi_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        };
        let encoded = encode_bcode_service_context(&context).expect("encode context");
        let json: serde_json::Value = serde_json::from_slice(&encoded).expect("json context");
        assert_eq!(json["interface_id"], "bcode.test/v1");
        assert_eq!(json["operation"], "run");

        let response = decode_bcode_service_response(br#"{"payload":[4,5],"error":null}"#)
            .expect("decode response");
        assert_eq!(response.payload, vec![4, 5]);
        assert!(response.error.is_none());
    }

    #[test]
    fn bcode_response_chunk_adapter_reassembles_chunks() {
        let mut first = SERVICE_RESPONSE_CHUNK_PREFIX.to_vec();
        first.extend_from_slice(b"hel");
        let mut second = SERVICE_RESPONSE_CHUNK_PREFIX.to_vec();
        second.extend_from_slice(b"lo");
        assert_eq!(reassemble_bcode_response_chunks(&[first, second]), b"hello");
    }

    #[test]
    fn current_bcode_manifest_parses_through_compatibility_adapter() {
        let adapted = parse_bcode_manifest(
            r#"
id = "bcode.test"
name = "Bcode Test"
version = "0.1.0"
concurrency = { mode = "concurrent" }

[[command_contributions]]
name = "ask"
summary = "Ask the model"
execution = "provider_exec"

[tui_surfaces.main]
title = "Main"

[agent_defaults]
model = "test-model"

[config]
foo = "bar"
"#,
        )
        .expect("bcode manifest should adapt");
        assert_eq!(adapted.manifest.id, "bcode.test");
        assert_eq!(adapted.contributions.len(), 1);
        assert_eq!(adapted.contributions[0].id(), "command:ask");
        assert_eq!(
            adapted.manifest.extensions[0].extension_point,
            "bcode.tui_surface/v1"
        );
        assert!(
            adapted
                .compatibility_payloads
                .contains_key("bcode.agent_defaults")
        );
        assert!(adapted.compatibility_payloads.contains_key("bcode.config"));
    }

    #[test]
    fn bcode_only_fields_do_not_leak_into_canonical_manifest() {
        let adapted = parse_bcode_manifest(
            r#"
id = "bcode.test"
name = "Bcode Test"
version = "0.1.0"

[tui_surfaces.main]
title = "Main"
"#,
        )
        .expect("bcode manifest should adapt");
        let serialized = toml::to_string(&adapted.manifest).expect("manifest should serialize");
        assert!(!serialized.contains("tui_surfaces"));
        assert!(!serialized.contains("agent_defaults"));
    }
}
