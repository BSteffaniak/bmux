//! Versioned native pointer output. This is metadata, never terminal escape input.
use bmux_terminal_protocol::pointer_shape::PointerShape;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::connection::TerminalChunk;
use crate::error::{MobileCoreError, Result};

pub const NATIVE_POINTER_VERSION: u32 = 1;

/// A native consumer must distinguish hiding the pointer from the default shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalPointerShape {
    Hidden,
    Shape(PointerShape),
}

/// Complete replaceable pointer state for one backend attachment lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPointerOutput {
    pub version: u32,
    pub generation: Uuid,
    pub revision: u64,
    pub shape: TerminalPointerShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPresentation {
    pub chunks: Vec<TerminalChunk>,
    /// Applies only after every preceding byte chunk in this presentation is accepted.
    pub pointer: Option<TerminalPointerOutput>,
}

/// Reject unknown contracts before draining any output.
///
/// # Errors
/// Returns an error for unsupported versions; version zero is not inferred as v1.
pub fn validate_pointer_version(version: u32) -> Result<()> {
    if version != NATIVE_POINTER_VERSION {
        return Err(MobileCoreError::TerminalBackendFailure(format!(
            "unsupported native pointer output version {version}; supported: {NATIVE_POINTER_VERSION}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_is_explicit() {
        assert!(validate_pointer_version(1).is_ok());
        assert!(validate_pointer_version(0).is_err());
        assert!(validate_pointer_version(2).is_err());
    }

    #[test]
    fn hidden_is_not_default() {
        assert_ne!(
            TerminalPointerShape::Hidden,
            TerminalPointerShape::Shape(PointerShape::Default)
        );
    }
}
