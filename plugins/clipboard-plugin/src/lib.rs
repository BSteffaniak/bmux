#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clipboard::ClipboardError;
use bmux_plugin::{NativeServiceContext, RustPlugin, ServiceResponse, handle_service};
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct ClipboardPlugin;

impl RustPlugin for ClipboardPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("clipboard-write/v1", "copy_text") => {
                handle_service(&context, |req: ClipboardCopyRequest, _ctx| {
                    bmux_clipboard::copy_text(&req.text).map_err(|error| match error {
                        ClipboardError::BackendUnavailable { .. } => ServiceResponse::error(
                            "backend_unavailable",
                            "clipboard backend unavailable",
                        ),
                        ClipboardError::BackendFailed { message, .. } => ServiceResponse::error(
                            "backend_failed",
                            format!("clipboard copy failed: {message}"),
                        ),
                    })?;
                    Ok(())
                })
            }
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

bmux_plugin::export_plugin!(ClipboardPlugin, include_str!("../plugin.toml"));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardCopyRequest {
    text: String,
}
