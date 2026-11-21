//! Output filters: code that keeps, drops or rewrites escape sequences from a pane's output before
//! its grid applies them, in the same pass that parses them.
//!
//! A filter declares which sequences it wants to see ([`FilterInterest`]); [`SequenceFilters`]
//! asks only the filters interested in a sequence, so a pane with no interested filter pays one
//! bit test per sequence. Filters change what panes display (grids, and the frames clients
//! receive); the raw output stream (raw readers, recording) stays as the program wrote it.

use std::sync::Arc;

use crate::SequenceHeader;

/// Which sequences a filter wants to decide on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterInterest {
    /// CSI final bytes `0x40..=0x7e`, bit `final - 0x40`.
    csi: u64,
    /// ESC final bytes `0x30..=0x7e`, bit `final - 0x30`.
    esc: u128,
    /// OSC numbers below 128, bit `number`.
    osc: u128,
    /// Every OSC number of 128 or more.
    high_osc: bool,
}

impl FilterInterest {
    /// No sequences.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            csi: 0,
            esc: 0,
            osc: 0,
            high_osc: false,
        }
    }

    /// Also CSI sequences ending in `action` (`0x40..=0x7e`; others are ignored).
    #[must_use]
    pub const fn csi(mut self, action: u8) -> Self {
        if matches!(action, 0x40..=0x7e) {
            self.csi |= 1 << (action - 0x40);
        }
        self
    }

    /// Also ESC sequences ending in `byte` (`0x30..=0x7e`; others are ignored).
    #[must_use]
    pub const fn esc(mut self, byte: u8) -> Self {
        if matches!(byte, 0x30..=0x7e) {
            self.esc |= 1 << (byte - 0x30);
        }
        self
    }

    /// Also OSC strings numbered `number`.
    #[must_use]
    pub const fn osc(mut self, number: u32) -> Self {
        if number < 128 {
            self.osc |= 1 << number;
        } else {
            self.high_osc = true;
        }
        self
    }

    const fn union(self, other: Self) -> Self {
        Self {
            csi: self.csi | other.csi,
            esc: self.esc | other.esc,
            osc: self.osc | other.osc,
            high_osc: self.high_osc || other.high_osc,
        }
    }

    fn wants_csi(&self, action: char) -> bool {
        u8::try_from(action)
            .ok()
            .filter(|byte| matches!(byte, 0x40..=0x7e))
            .is_some_and(|byte| self.csi & (1 << (byte - 0x40)) != 0)
    }

    fn wants_esc(&self, byte: u8) -> bool {
        matches!(byte, 0x30..=0x7e) && self.esc & (1 << (byte - 0x30)) != 0
    }

    fn wants_osc(&self, number: Option<u32>) -> bool {
        match number {
            Some(number) if number < 128 => self.osc & (1 << number) != 0,
            Some(_) => self.high_osc,
            None => false,
        }
    }
}

/// What a filter does with a sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FilterVerdict {
    /// The grid applies it as written.
    #[default]
    Keep,
    /// The grid does not apply it, and nothing replies to it.
    Drop,
    /// The grid applies, instead, the bytes the filter wrote to its replacement buffer: parsed on
    /// their own (complete sequences and text; anything left incomplete is discarded), with no
    /// filter consulted and no image recognised.
    Replace,
}

/// Decides what of the sequences a pane's program wrote reaches its grid. Each method answers only
/// for the sequences [`Self::interest`] names, and writes to `replacement` (empty when asked) only
/// to answer [`FilterVerdict::Replace`].
pub trait SequenceFilter: Send + Sync {
    /// The sequences this filter decides on. Read when the filter is registered.
    fn interest(&self) -> FilterInterest;

    /// What to do with a CSI sequence.
    fn filter_csi(
        &self,
        _sequence: &SequenceHeader<'_>,
        _replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        FilterVerdict::Keep
    }

    /// What to do with an OSC string (split at `;`, at most 16 parameters; the first is its
    /// number).
    fn filter_osc(&self, _params: &[&[u8]], _replacement: &mut Vec<u8>) -> FilterVerdict {
        FilterVerdict::Keep
    }

    /// What to do with an ESC sequence.
    fn filter_esc(
        &self,
        _intermediates: &[u8],
        _byte: u8,
        _replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        FilterVerdict::Keep
    }
}

/// One registered filter: its owner's key (if it registered under one), interest, and the filter.
type Registered = (Option<String>, FilterInterest, Arc<dyn SequenceFilter>);

/// The registered output filters, and the union of their interests.
///
/// Panes share one set through plugin state, keyed by this type: a native plugin registers a
/// filter with `states.get_or_insert_with(SequenceFilters::default).write()?.register(filter)`,
/// and every pane consults it from its next chunk of output. Interested filters are asked in
/// registration order; the first that drops or replaces a sequence decides it.
#[derive(Default, Clone)]
pub struct SequenceFilters {
    filters: Vec<Registered>,
    interest: FilterInterest,
}

impl SequenceFilters {
    /// Add `filter`; it is asked after those already registered.
    pub fn register(&mut self, filter: Arc<dyn SequenceFilter>) {
        let interest = filter.interest();
        self.interest = self.interest.union(interest);
        self.filters.push((None, interest, filter));
    }

    /// Set the filter registered under `key` (replacing the one there, in its place), or with
    /// `None` remove it.
    pub fn set(&mut self, key: &str, filter: Option<Arc<dyn SequenceFilter>>) {
        let at = self
            .filters
            .iter()
            .position(|(owner, ..)| owner.as_deref() == Some(key));
        match (at, filter) {
            (Some(at), Some(filter)) => {
                self.filters[at] = (Some(key.to_owned()), filter.interest(), filter);
            }
            (Some(at), None) => {
                self.filters.remove(at);
            }
            (None, Some(filter)) => {
                self.filters
                    .push((Some(key.to_owned()), filter.interest(), filter));
            }
            (None, None) => {}
        }
        self.interest = self
            .filters
            .iter()
            .fold(FilterInterest::none(), |all, (_, interest, _)| {
                all.union(*interest)
            });
    }

    /// Whether no filter is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// The first verdict other than keep among the filters `wants` selects, in order.
    fn decide(
        &self,
        replacement: &mut Vec<u8>,
        wants: impl Fn(&FilterInterest) -> bool,
        ask: impl Fn(&dyn SequenceFilter, &mut Vec<u8>) -> FilterVerdict,
    ) -> FilterVerdict {
        for (_, interest, filter) in &self.filters {
            if !wants(interest) {
                continue;
            }
            replacement.clear();
            let verdict = ask(filter.as_ref(), replacement);
            if verdict != FilterVerdict::Keep {
                return verdict;
            }
        }
        replacement.clear();
        FilterVerdict::Keep
    }

    /// What the interested filters do with this CSI sequence.
    pub fn filter_csi(
        &self,
        sequence: &SequenceHeader<'_>,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        let action = sequence.action();
        if !self.interest.wants_csi(action) {
            return FilterVerdict::Keep;
        }
        self.decide(
            replacement,
            |interest| interest.wants_csi(action),
            |filter, replacement| filter.filter_csi(sequence, replacement),
        )
    }

    /// What the interested filters do with this OSC string.
    pub fn filter_osc(&self, params: &[&[u8]], replacement: &mut Vec<u8>) -> FilterVerdict {
        let number = params.first().and_then(|first| osc_number(first));
        if !self.interest.wants_osc(number) {
            return FilterVerdict::Keep;
        }
        self.decide(
            replacement,
            |interest| interest.wants_osc(number),
            |filter, replacement| filter.filter_osc(params, replacement),
        )
    }

    /// What the interested filters do with this ESC sequence.
    pub fn filter_esc(
        &self,
        intermediates: &[u8],
        byte: u8,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        if !self.interest.wants_esc(byte) {
            return FilterVerdict::Keep;
        }
        self.decide(
            replacement,
            |interest| interest.wants_esc(byte),
            |filter, replacement| filter.filter_esc(intermediates, byte, replacement),
        )
    }
}

fn osc_number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    digits.iter().try_fold(0_u32, |number, digit| {
        digit
            .is_ascii_digit()
            .then(|| number * 10 + u32::from(digit - b'0'))
    })
}
