use crate::{ApiVersion, PluginCapability};
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

    #[error("plugin '{plugin_id}' is missing native entry path")]
    MissingEntryPath { plugin_id: String },

    #[error("plugin '{plugin_id}' entry file does not exist: {path:?}")]
    MissingEntryFile { plugin_id: String, path: PathBuf },

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

    #[error("plugin '{plugin_id}' requested unsupported capability '{capability}'")]
    UnsupportedCapability {
        plugin_id: String,
        capability: PluginCapability,
    },

    #[error("failed to load native plugin '{plugin_id}' from {path:?}: {details}")]
    NativeLibraryLoad {
        plugin_id: String,
        path: PathBuf,
        details: String,
    },

    #[error("failed to resolve entry symbol '{symbol}' for plugin '{plugin_id}': {details}")]
    NativeEntrySymbol {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error("plugin '{plugin_id}' returned a null native descriptor from symbol '{symbol}'")]
    NullNativeDescriptor { plugin_id: String, symbol: String },

    #[error("plugin '{plugin_id}' returned invalid UTF-8 descriptor text from '{symbol}'")]
    InvalidNativeDescriptorUtf8 { plugin_id: String, symbol: String },

    #[error("plugin '{plugin_id}' returned invalid native descriptor from '{symbol}': {details}")]
    InvalidNativeDescriptor {
        plugin_id: String,
        symbol: String,
        details: String,
    },

    #[error(
        "plugin '{plugin_id}' descriptor field '{field}' does not match manifest (manifest: {manifest_value}, descriptor: {descriptor_value})"
    )]
    NativeDescriptorMismatch {
        plugin_id: String,
        field: &'static str,
        manifest_value: String,
        descriptor_value: String,
    },

    #[error("failed to parse plugin manifest: {0}")]
    ManifestParse(#[from] toml::de::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
