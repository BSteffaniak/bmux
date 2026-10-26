//! Conservative host support for OSC 22, independent of mouse reporting.

/// Proven host pointer protocol support. Basic xterm support does not imply
/// stack operations or queries are understood.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PointerShapeCapability {
    #[default]
    Unsupported,
    Set,
    Stack,
}

impl PointerShapeCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Set => "set",
            Self::Stack => "stack",
        }
    }

    /// Interpret explicit host evidence without I/O or environment mutation.
    /// Unknown multiplexer profiles cannot establish outer-host support.
    #[must_use]
    pub fn from_environment(term: &str, program: &str, override_value: Option<&str>) -> Self {
        if let Some(value) = override_value {
            return match value {
                "stack" => Self::Stack,
                "set" => Self::Set,
                _ => Self::Unsupported,
            };
        }
        let term = term.to_ascii_lowercase();
        let program = program.to_ascii_lowercase();
        if term == "dumb" {
            return Self::Unsupported;
        }
        if matches!(program.as_str(), "kitty" | "ghostty" | "foot")
            || term.contains("kitty")
            || term.contains("ghostty")
            || term.starts_with("foot")
        {
            return Self::Stack;
        }
        // xterm-256color alone is also advertised by many unrelated emulators.
        if program == "xterm" {
            return Self::Set;
        }
        Self::Unsupported
    }

    #[must_use]
    pub fn detect() -> Self {
        let capability = Self::from_environment(
            &std::env::var("TERM").unwrap_or_default(),
            &std::env::var("TERM_PROGRAM").unwrap_or_default(),
            std::env::var("BMUX_PLAYBOOK_HEADLESS_POINTER_SHAPES")
                .ok()
                .as_deref(),
        );
        let in_tmux = std::env::var_os("TMUX").is_some_and(|value| !value.is_empty());
        if in_tmux && std::env::var("BMUX_POINTER_SHAPE_TMUX_PASSTHROUGH").as_deref() != Ok("1") {
            return Self::Unsupported;
        }
        capability
    }
}

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
