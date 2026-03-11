#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clipboard::ClipboardError;
use bmux_plugin::{
    HostScope, NativeDescriptor, NativeServiceContext, PluginFeature, PluginService, RustPlugin,
    ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Default)]
struct ClipboardPlugin;

impl RustPlugin for ClipboardPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor {
            id: "bmux.clipboard".to_string(),
            display_name: "bmux Clipboard".to_string(),
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_api: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            native_abi: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            description: Some("Shipped bmux clipboard plugin".to_string()),
            homepage: None,
            required_capabilities: BTreeSet::new(),
            provided_capabilities: BTreeSet::from([
                HostScope::new("bmux.clipboard.write").expect("host scope should parse")
            ]),
            provided_features: BTreeSet::from([
                PluginFeature::new("bmux.clipboard").expect("plugin feature should parse")
            ]),
            services: vec![PluginService {
                capability: HostScope::new("bmux.clipboard.write")
                    .expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "clipboard-write/v1".to_string(),
            }],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            },
        }
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("clipboard-write/v1", "copy_text") => run_copy_text_service(&context),
            _ => ServiceResponse::error(
                "unsupported_service_operation",
                format!(
                    "unsupported clipboard service invocation '{}:{}'",
                    context.request.service.interface_id, context.request.operation,
                ),
            ),
        }
    }
}

bmux_plugin::export_plugin!(ClipboardPlugin);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardCopyRequest {
    text: String,
}

fn run_copy_text_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = match decode_service_message::<ClipboardCopyRequest>(&context.request.payload) {
        Ok(request) => request,
        Err(error) => {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
    };

    if let Err(error) = bmux_clipboard::copy_text(&request.text) {
        return match error {
            ClipboardError::BackendUnavailable { .. } => {
                ServiceResponse::error("backend_unavailable", "clipboard backend unavailable")
            }
            ClipboardError::BackendFailed { message, .. } => ServiceResponse::error(
                "backend_failed",
                format!("clipboard copy failed: {message}"),
            ),
        };
    }

    match encode_service_message(&()) {
        Ok(payload) => ServiceResponse::ok(payload),
        Err(error) => ServiceResponse::error("response_encode_failed", error.to_string()),
    }
}
