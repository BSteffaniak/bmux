#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::fs;
use std::path::Path;

use bmux_clipboard::{ClipboardError, ClipboardPayload};
use bmux_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct ClipboardPlugin;

impl RustPlugin for ClipboardPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        let span = tracing::debug_span!(
            "clipboard_plugin_service",
            bmux.component = "clipboard.remote_sync",
            bmux.operation = %context.request.operation,
            interface_id = %context.request.service.interface_id,
        );
        let _guard = span.enter();
        bmux_plugin_sdk::route_service!(context, {
            "clipboard-write/v1", "copy_text" => |req: ClipboardCopyRequest, _ctx| {
                tracing::debug!("clipboard copy_text invoked");
                bmux_clipboard::copy_text(&req.text).map_err(map_clipboard_error)?;
                Ok(())
            },
            "clipboard-write/v1", "copy_image_png" => |req: ClipboardImageRequest, _ctx| {
                tracing::debug!(bytes_len = req.bytes.len(), "clipboard copy_image_png invoked");
                bmux_clipboard::copy_png_image(&req.bytes).map_err(map_clipboard_error)?;
                Ok(())
            },
            "clipboard-read/v1", "read_payload" => |req: ClipboardReadRequest, _ctx| {
                tracing::debug!(prefer_image = req.prefer_image, "clipboard read_payload invoked");
                let payload = bmux_clipboard::read_payload(req.prefer_image).map_err(map_clipboard_error)?;
                Ok(ClipboardPayloadResponse::from_payload(payload))
            },
            "clipboard-remote-sync/v1", "materialize_payload" => |req: ClipboardPayloadResponse, ctx| {
                tracing::debug!(mime = %req.mime, bytes_len = req.bytes.len(), source = req.source.as_deref().unwrap_or("unknown"), attempts = req.attempts.as_deref().unwrap_or(""), "clipboard materialize_payload invoked");
                materialize_payload(&req, &ctx.connection.state_dir).map_err(|message| {
                    ServiceResponse::error("materialize_failed", message)
                })
            },
        })
    }
}

bmux_plugin_sdk::export_plugin!(ClipboardPlugin, include_str!("../plugin.toml"));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardCopyRequest {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardImageRequest {
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardReadRequest {
    prefer_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardPayloadResponse {
    mime: String,
    bytes: Vec<u8>,
    text: Option<String>,
    source: Option<String>,
    attempts: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClipboardMaterializeResponse {
    mime: String,
    bytes_len: usize,
    path: Option<String>,
    clipboard_written: bool,
    backend: Option<String>,
    warning: Option<String>,
}

impl ClipboardPayloadResponse {
    fn from_payload(payload: ClipboardPayload) -> Self {
        match payload {
            ClipboardPayload::Text(text) => Self {
                mime: "text/plain".to_string(),
                bytes: text.as_bytes().to_vec(),
                text: Some(text),
                source: None,
                attempts: None,
            },
            ClipboardPayload::ImagePng(bytes) => Self {
                mime: "image/png".to_string(),
                bytes,
                text: None,
                source: None,
                attempts: None,
            },
        }
    }
}

fn materialize_payload(
    payload: &ClipboardPayloadResponse,
    state_dir: &str,
) -> Result<ClipboardMaterializeResponse, String> {
    match payload.mime.as_str() {
        "text/plain" => {
            let text = payload
                .text
                .clone()
                .unwrap_or_else(|| String::from_utf8_lossy(&payload.bytes).into_owned());
            bmux_clipboard::copy_text(&text).map_err(|error| error.to_string())?;
            tracing::info!(
                bytes_len = text.len(),
                source = payload.source.as_deref().unwrap_or("unknown"),
                attempts = payload.attempts.as_deref().unwrap_or(""),
                clipboard_written = true,
                "clipboard text materialized"
            );
            Ok(ClipboardMaterializeResponse {
                mime: payload.mime.clone(),
                bytes_len: text.len(),
                path: None,
                clipboard_written: true,
                backend: Some("arboard_or_command".to_string()),
                warning: None,
            })
        }
        "image/png" => {
            let hash = stable_hash_hex(&payload.bytes);
            let dir = Path::new(state_dir).join("clipboard");
            fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let path = dir.join(format!("{hash}.png"));
            fs::write(&path, &payload.bytes).map_err(|error| error.to_string())?;
            let (clipboard_written, warning) = match bmux_clipboard::copy_png_image(&payload.bytes)
            {
                Ok(()) => {
                    tracing::info!(path = %path.display(), bytes_len = payload.bytes.len(), source = payload.source.as_deref().unwrap_or("unknown"), attempts = payload.attempts.as_deref().unwrap_or(""), clipboard_written = true, "clipboard image materialized and copied");
                    (true, None)
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, source = payload.source.as_deref().unwrap_or("unknown"), attempts = payload.attempts.as_deref().unwrap_or(""), clipboard_written = false, "clipboard image materialized but OS clipboard write failed");
                    (false, Some(error.to_string()))
                }
            };
            Ok(ClipboardMaterializeResponse {
                mime: payload.mime.clone(),
                bytes_len: payload.bytes.len(),
                path: Some(path.to_string_lossy().into_owned()),
                clipboard_written,
                backend: Some("arboard".to_string()),
                warning,
            })
        }
        other => Err(format!("unsupported clipboard payload MIME type '{other}'")),
    }
}

fn stable_hash_hex(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn map_clipboard_error(error: ClipboardError) -> ServiceResponse {
    match error {
        ClipboardError::BackendUnavailable { .. } => {
            ServiceResponse::error("backend_unavailable", "clipboard backend unavailable")
        }
        ClipboardError::BackendFailed { message, .. }
        | ClipboardError::CommandFailed { message, .. } => ServiceResponse::error(
            "backend_failed",
            format!("clipboard operation failed: {message}"),
        ),
    }
}
