#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clipboard::ClipboardError;
use bmux_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct ClipboardPlugin;

impl RustPlugin for ClipboardPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "clipboard-write/v1", "copy_text" => |req: ClipboardCopyRequest, _ctx| {
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
            },
        })
    }
}

bmux_plugin_sdk::export_plugin!(ClipboardPlugin, include_str!("../plugin.toml"));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardCopyRequest {
    text: String,
}
