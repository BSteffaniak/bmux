#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clipboard::ClipboardError;
use bmux_plugin::{
    HostScope, NativeDescriptor, NativeServiceContext, PluginService, RustPlugin, ServiceKind,
    ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct ClipboardPlugin;

impl RustPlugin for ClipboardPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.clipboard", "bmux Clipboard")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux clipboard plugin")
            .provide_capability("bmux.clipboard.write")
            .expect("capability should parse")
            .provide_feature("bmux.clipboard")
            .expect("feature should parse")
            .service(PluginService {
                capability: HostScope::new("bmux.clipboard.write")
                    .expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "clipboard-write/v1".to_string(),
            })
            .lifecycle(bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            })
            .build()
            .expect("descriptor should validate")
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

#[cfg(not(feature = "static-bundled"))]
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
