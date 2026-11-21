//! Output filter rules callers set (`pane-runtime-output-filters-v1`): declarative keep, drop and
//! replace rules for the sequences panes' programs write, evaluated in the grid's own parse.
//!
//! Each caller has one rule list, registered in the panes' shared [`SequenceFilters`] under the
//! caller's plugin id (the empty id is the host and its clients), so a pane consults it from its
//! next read. Rules decide what grids and frames show; raw output stays as the program wrote it.

use bmux_pane_runtime_plugin_api::pane_runtime_output_filters_v1::{
    OutputAction, OutputFilterError, OutputMatch, OutputRule,
};
use bmux_terminal_grid::{
    FilterInterest, FilterVerdict, SequenceFilter, SequenceFilters, SequenceHeader,
};
use switchy_thread::sync::Arc;

/// Most rules one caller may set.
pub(crate) const MAX_RULES: usize = 256;
/// Longest replacement a rule may carry.
const MAX_REPLACEMENT_BYTES: usize = 4096;

/// Set `caller`'s rules for every pane in `filters` (an empty list removes them), after checking
/// each.
pub(crate) fn set_rules(
    // In core's state registry, so behind std's lock.
    filters: &std::sync::RwLock<SequenceFilters>,
    caller: &str,
    rules: Vec<OutputRule>,
) -> Result<(), OutputFilterError> {
    if rules.len() > MAX_RULES {
        return Err(OutputFilterError::TooManyRules {
            max: u32::try_from(MAX_RULES).unwrap_or(u32::MAX),
        });
    }
    let filter = RuleFilter::new(rules)?;
    let mut filters = filters
        .write()
        .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner);
    filters.set(
        caller,
        (!filter.rules.is_empty()).then(|| Arc::new(filter) as Arc<dyn SequenceFilter>),
    );
    Ok(())
}

/// One caller's checked rules, in order.
struct RuleFilter {
    rules: Vec<OutputRule>,
    interest: FilterInterest,
}

impl RuleFilter {
    fn new(rules: Vec<OutputRule>) -> Result<Self, OutputFilterError> {
        let mut interest = FilterInterest::none();
        for (index, rule) in rules.iter().enumerate() {
            let invalid = |reason: &str| OutputFilterError::InvalidRule {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                reason: reason.to_owned(),
            };
            interest = match &rule.sequence {
                OutputMatch::Csi {
                    final_byte,
                    intermediates,
                    params,
                } => {
                    if !(0x40..=0x7e).contains(final_byte) {
                        return Err(invalid("a CSI final byte is 0x40..=0x7e"));
                    }
                    let (private, rest) = match intermediates.split_first() {
                        Some((first, rest)) if (0x3c..=0x3f).contains(first) => (true, rest),
                        _ => (false, intermediates.as_slice()),
                    };
                    if rest.len() + usize::from(private) > 2
                        || !rest.iter().all(|byte| (0x20..=0x2f).contains(byte))
                    {
                        return Err(invalid(
                            "CSI intermediates are a private marker and 0x20..=0x2f, at most two",
                        ));
                    }
                    if params.as_ref().is_some_and(|params| params.len() > 32) {
                        return Err(invalid("a CSI sequence has at most 32 parameters"));
                    }
                    interest.csi(*final_byte)
                }
                OutputMatch::Osc { number } => interest.osc(*number),
                OutputMatch::Esc {
                    final_byte,
                    intermediates,
                } => {
                    if !(0x30..=0x7e).contains(final_byte) {
                        return Err(invalid("an ESC final byte is 0x30..=0x7e"));
                    }
                    if intermediates.len() > 2
                        || !intermediates
                            .iter()
                            .all(|byte| (0x20..=0x2f).contains(byte))
                    {
                        return Err(invalid("ESC intermediates are 0x20..=0x2f, at most two"));
                    }
                    interest.esc(*final_byte)
                }
            };
            if let OutputAction::Replace { bytes } = &rule.action
                && bytes.len() > MAX_REPLACEMENT_BYTES
            {
                return Err(invalid("a replacement is at most 4096 bytes"));
            }
        }
        Ok(Self { rules, interest })
    }

    /// The first matching rule's verdict, its replacement written to `replacement`.
    fn decide(
        &self,
        matches: impl Fn(&OutputMatch) -> bool,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        let Some(rule) = self.rules.iter().find(|rule| matches(&rule.sequence)) else {
            return FilterVerdict::Keep;
        };
        match &rule.action {
            OutputAction::Keep => FilterVerdict::Keep,
            OutputAction::Drop => FilterVerdict::Drop,
            OutputAction::Replace { bytes } => {
                replacement.extend_from_slice(bytes);
                FilterVerdict::Replace
            }
        }
    }
}

impl SequenceFilter for RuleFilter {
    fn interest(&self) -> FilterInterest {
        self.interest
    }

    fn filter_csi(
        &self,
        sequence: &SequenceHeader<'_>,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        let values = sequence.values();
        self.decide(
            |rule| match rule {
                OutputMatch::Csi {
                    final_byte,
                    intermediates,
                    params,
                } => {
                    u32::from(*final_byte) == u32::from(sequence.action())
                        && intermediates.as_slice() == sequence.intermediates()
                        && params
                            .as_ref()
                            .is_none_or(|params| values.as_deref() == Some(params.as_slice()))
                }
                _ => false,
            },
            replacement,
        )
    }

    fn filter_osc(&self, params: &[&[u8]], replacement: &mut Vec<u8>) -> FilterVerdict {
        let number = params
            .first()
            .and_then(|first| std::str::from_utf8(first).ok()?.parse::<u32>().ok());
        self.decide(
            |rule| matches!(rule, OutputMatch::Osc { number: wanted } if Some(*wanted) == number),
            replacement,
        )
    }

    fn filter_esc(
        &self,
        intermediates: &[u8],
        byte: u8,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        self.decide(
            |rule| {
                matches!(rule, OutputMatch::Esc { final_byte, intermediates: wanted }
                    if *final_byte == byte && wanted.as_slice() == intermediates)
            },
            replacement,
        )
    }
}
