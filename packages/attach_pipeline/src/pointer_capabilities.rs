//! Conservative host support for OSC 22, independent of mouse reporting.

/// Shared neutral capability type; retained here for existing attach consumers.
pub use bmux_terminal_protocol::pointer_output::PointerShapeCapability;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_conservative_and_override_is_deterministic() {
        assert_eq!(
            PointerShapeCapability::from_environment("xterm-256color", "", None),
            PointerShapeCapability::Unsupported
        );
        assert_eq!(
            PointerShapeCapability::from_environment("screen", "", None),
            PointerShapeCapability::Unsupported
        );
        assert_eq!(
            PointerShapeCapability::from_environment("xterm-256color", "ghostty", None),
            PointerShapeCapability::Stack
        );
        assert_eq!(
            PointerShapeCapability::from_environment("xterm", "xterm", None),
            PointerShapeCapability::Set
        );
        assert_eq!(
            PointerShapeCapability::from_environment("dumb", "kitty", None),
            PointerShapeCapability::Unsupported
        );
        assert_eq!(
            PointerShapeCapability::from_environment("dumb", "", Some("stack")),
            PointerShapeCapability::Stack
        );
        assert_eq!(
            PointerShapeCapability::from_environment("xterm-kitty", "kitty", Some("off")),
            PointerShapeCapability::Unsupported
        );
        assert_eq!(
            PointerShapeCapability::from_environment("xterm-kitty", "kitty", Some("typo")),
            PointerShapeCapability::Unsupported
        );
    }
}
