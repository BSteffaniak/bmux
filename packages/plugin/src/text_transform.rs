//! Rules that redact or restyle pane text as the attach client renders it.
//!
//! A render extension answers [`crate::AttachRenderExtension::pane_text_rules`] with compiled
//! [`PaneTextRules`]; the attach renderer applies them to each pane row it paints from its replica
//! of the pane's screen. The text is matched one row at a time (a match does not span rows), and
//! what a rule masks is never written to the host terminal.
//!
//! Render extensions are compiled into the attach client and run with its authority, so these
//! rules are not gated by a capability: whatever the client loads may transform what it shows.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::RenderStyle;

/// What a rule matches: a literal string, or a regular expression (the `regex` crate's syntax).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneTextPattern {
    Literal(String),
    Regex(String),
}

/// What a rule does to the text it matches: each matched column shows `mask` (one column wide)
/// instead of its text, and/or is painted in `style` instead of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PaneTextAction {
    pub mask: Option<char>,
    pub style: Option<RenderStyle>,
}

/// A rule an extension asks the renderer to apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneTextRule {
    /// The extension's name for the rule, for diagnostics.
    pub id: String,
    pub pattern: PaneTextPattern,
    pub action: PaneTextAction,
}

enum Matcher {
    Literal(String),
    Regex(regex::Regex),
}

/// Compiled rules. Each set has its own identity ([`Self::id`]): the renderer draws a pane's
/// rows again when the set it applies changes.
pub struct PaneTextRules {
    id: u64,
    rules: Vec<(String, Matcher, PaneTextAction)>,
}

impl std::fmt::Debug for PaneTextRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneTextRules")
            .field("id", &self.id)
            .field(
                "rules",
                &self.rules.iter().map(|(id, _, _)| id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// One stretch of a row a rule matched, in byte offsets of the row's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneTextMatch<'a> {
    pub bytes: (usize, usize),
    pub action: &'a PaneTextAction,
}

impl PaneTextRules {
    /// Compile `rules`.
    ///
    /// # Errors
    ///
    /// A regular expression does not compile; the error names the rule.
    pub fn new(rules: impl IntoIterator<Item = PaneTextRule>) -> Result<Self, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let rules = rules
            .into_iter()
            .filter_map(|rule| {
                if rule
                    .action
                    .mask
                    .is_some_and(|mask| crate::render_char_display_width_u16(mask) != 1)
                {
                    return Some(Err(format!(
                        "pane text rule {}: a mask must be one column wide",
                        rule.id
                    )));
                }
                let matcher = match rule.pattern {
                    PaneTextPattern::Literal(text) if text.is_empty() => return None,
                    PaneTextPattern::Literal(text) => Ok(Matcher::Literal(text)),
                    PaneTextPattern::Regex(pattern) => regex::Regex::new(&pattern)
                        .map(Matcher::Regex)
                        .map_err(|error| format!("pane text rule {}: {error}", rule.id)),
                };
                Some(matcher.map(|matcher| (rule.id, matcher, rule.action)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            rules,
        })
    }

    /// This set's identity.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Whether the set has no rules.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Every match of every rule in `text`, rule by rule (a later rule's match over an earlier
    /// one's applies over it). Matches of an empty pattern are skipped.
    pub fn matches<'a>(&'a self, text: &'a str, found: &mut Vec<PaneTextMatch<'a>>) {
        for (_, matcher, action) in &self.rules {
            let mut push = |range: Range<usize>| {
                if !range.is_empty() {
                    found.push(PaneTextMatch {
                        bytes: (range.start, range.end),
                        action,
                    });
                }
            };
            match matcher {
                Matcher::Literal(literal) => {
                    for (start, matched) in text.match_indices(literal.as_str()) {
                        push(start..start + matched.len());
                    }
                }
                Matcher::Regex(regex) => {
                    for matched in regex.find_iter(text) {
                        push(matched.range());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(patterns: &[PaneTextPattern]) -> PaneTextRules {
        PaneTextRules::new(
            patterns
                .iter()
                .enumerate()
                .map(|(index, pattern)| PaneTextRule {
                    id: format!("rule-{index}"),
                    pattern: pattern.clone(),
                    action: PaneTextAction {
                        mask: Some('*'),
                        style: None,
                    },
                }),
        )
        .expect("rules compile")
    }

    #[test]
    fn literal_and_regex_rules_find_every_match() {
        let rules = rules(&[
            PaneTextPattern::Literal("pw".into()),
            PaneTextPattern::Regex("AKIA[0-9A-Z]{16}".into()),
        ]);
        let text = "pw=x AKIAABCDEFGHIJKLMNOP pw";
        let mut found = Vec::new();
        rules.matches(text, &mut found);
        let ranges = found.iter().map(|m| m.bytes).collect::<Vec<_>>();
        assert_eq!(ranges, vec![(0, 2), (26, 28), (5, 25)]);
    }

    #[test]
    fn a_bad_regex_names_its_rule_and_sets_have_distinct_ids() {
        let error = PaneTextRules::new([PaneTextRule {
            id: "broken".into(),
            pattern: PaneTextPattern::Regex("(".into()),
            action: PaneTextAction::default(),
        }])
        .expect_err("does not compile");
        assert!(error.contains("broken"), "{error}");
        assert_ne!(rules(&[]).id(), rules(&[]).id());
    }
}
