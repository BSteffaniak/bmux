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

    #[error("plugin '{plugin_id}' entry file does not exist: {}", path.display())]
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

    #[error("failed to parse plugin manifest: {0}")]
    ManifestParse(#[from] toml::de::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
