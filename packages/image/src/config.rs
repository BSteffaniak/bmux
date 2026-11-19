//! Image configuration types.

use serde::{Deserialize, Serialize};

/// Top-level image configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// Master switch for image protocol support.
    pub enabled: bool,
    /// Maximum image payload size in bytes.
    pub max_image_bytes: usize,
    /// Maximum number of images kept per pane.
    pub max_images_per_pane: usize,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_image_bytes: 10 * 1024 * 1024, // 10 MiB
            max_images_per_pane: 100,
        }
    }
}
