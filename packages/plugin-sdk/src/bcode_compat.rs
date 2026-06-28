//! Temporary compatibility helpers for adapting legacy Bcode contribution
//! declarations into canonical BMUX contributions.
//!
//! Compatibility adapters must resolve Bcode's replacement/override behavior
//! before emitting canonical [`PluginContribution`] values. The canonical BMUX
//! registrar intentionally rejects duplicates.

use crate::PluginContribution;
use std::collections::BTreeMap;

/// Resolve legacy Bcode replacement behavior into one canonical contribution
/// per contribution ID.
#[must_use]
pub fn resolve_replacements(
    contributions: impl IntoIterator<Item = PluginContribution>,
) -> Vec<PluginContribution> {
    contributions
        .into_iter()
        .map(|contribution| (contribution.id().to_string(), contribution))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::resolve_replacements;
    use crate::{PluginCommand, PluginContribution};

    #[test]
    fn bcode_replacement_emits_one_resolved_contribution_per_id() {
        let contributions = resolve_replacements([
            PluginContribution::command(PluginCommand::new("hello", "first")),
            PluginContribution::command(PluginCommand::new("hello", "replacement")),
        ]);
        assert_eq!(contributions.len(), 1);
        let PluginContribution::Command { command, .. } = &contributions[0] else {
            panic!("expected command contribution");
        };
        assert_eq!(command.summary, "replacement");
    }
}
