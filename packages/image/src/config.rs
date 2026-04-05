//! Image configuration types.

use serde::{Deserialize, Serialize};

/// How image decoding is distributed between server and client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDecodeMode {
    /// Server decodes images to pixel buffers; client encodes for host protocol.
    #[default]
    Server,
    /// Server sends raw protocol bytes; client decodes + re-encodes.
    Client,
    /// Raw bytes forwarded with coordinate translation (same-protocol only).
    Passthrough,
}

/// Top-level image configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// Master switch for image protocol support.
    pub enabled: bool,
    /// Default decode mode.
    pub decode_mode: ImageDecodeMode,
    /// Maximum image payload size in bytes.
    pub max_image_bytes: usize,
    /// Maximum number of images kept per pane.
    pub max_images_per_pane: usize,

    /// Per-protocol decode mode overrides.
    #[cfg(feature = "sixel")]
    pub sixel: ProtocolConfig,
    #[cfg(feature = "kitty")]
    pub kitty: ProtocolConfig,
    #[cfg(feature = "iterm2")]
    pub iterm2: ProtocolConfig,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decode_mode: ImageDecodeMode::default(),
            max_image_bytes: 10 * 1024 * 1024, // 10 MiB
            max_images_per_pane: 100,
            #[cfg(feature = "sixel")]
            sixel: ProtocolConfig::default(),
            #[cfg(feature = "kitty")]
            kitty: ProtocolConfig::default(),
            #[cfg(feature = "iterm2")]
            iterm2: ProtocolConfig::default(),
        }
    }
}

/// Per-protocol configuration overrides.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtocolConfig {
    /// Override the decode mode for this protocol.
    /// `None` means use the top-level `decode_mode`.
    pub decode_mode: Option<ImageDecodeMode>,
}
