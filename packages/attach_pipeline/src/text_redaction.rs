//! Configured redaction: pane text matching the user's patterns is painted as `*`, never written
//! to the host terminal (`behavior.redact_patterns`).

use std::sync::Arc;

use bmux_plugin::{
    AttachRenderExtension, PaneTextAction, PaneTextPattern, PaneTextRule, PaneTextRules,
};

/// The mask redacted text is painted with.
pub const REDACTION_MASK: char = '*';

/// A render extension that only redacts: every pane's text matching one of its patterns.
pub struct PaneTextRedaction {
    rules: Arc<PaneTextRules>,
}

impl PaneTextRedaction {
    /// The redaction of `patterns` (regular expressions); `None` when there are none.
    ///
    /// # Errors
    ///
    /// A pattern does not compile.
    pub fn from_patterns(patterns: &[String]) -> Result<Option<Self>, String> {
        if patterns.is_empty() {
            return Ok(None);
        }
        let rules =
            PaneTextRules::new(
                patterns
                    .iter()
                    .enumerate()
                    .map(|(index, pattern)| PaneTextRule {
                        id: format!("behavior.redact_patterns[{index}]"),
                        pattern: PaneTextPattern::Regex(pattern.clone()),
                        action: PaneTextAction {
                            mask: Some(REDACTION_MASK),
                            style: None,
                        },
                    }),
            )?;
        Ok(Some(Self {
            rules: Arc::new(rules),
        }))
    }
}

impl AttachRenderExtension for PaneTextRedaction {
    fn name(&self) -> &'static str {
        "bmux.redaction"
    }

    fn pane_text_rules(&self) -> Option<Arc<PaneTextRules>> {
        Some(self.rules.clone())
    }
}
