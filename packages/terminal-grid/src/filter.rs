//! Output filters: code that drops escape sequences from a pane's output before its grid
//! applies them, in the same pass that parses them.
//!
//! A filter declares which sequences it wants to see ([`FilterInterest`]); [`SequenceFilters`]
//! asks only the filters interested in a sequence, so a pane with no interested filter pays one
//! bit test per sequence. Filters change what panes display (grids, and the frames clients
//! receive); the raw output stream (recording) stays as the program wrote it.

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

/// Decides whether sequences a pane's program wrote reach its grid. Each method answers only for
/// the sequences [`Self::interest`] names; `false` drops the sequence (the grid does not apply
/// it, and nothing replies to it).
pub trait SequenceFilter: Send + Sync {
    /// The sequences this filter decides on. Read when the filter is registered.
    fn interest(&self) -> FilterInterest;

    /// Whether to keep a CSI sequence.
    fn keep_csi(&self, _sequence: &SequenceHeader<'_>) -> bool {
        true
    }

    /// Whether to keep an OSC string (split at `;`, at most 16 parameters; the first is its
    /// number).
    fn keep_osc(&self, _params: &[&[u8]]) -> bool {
        true
    }

    /// Whether to keep an ESC sequence.
    fn keep_esc(&self, _intermediates: &[u8], _byte: u8) -> bool {
        true
    }
}

/// The registered output filters, and the union of their interests.
///
/// Panes share one set through plugin state, keyed by this type: a native plugin registers a
/// filter with `states.get_or_insert_with(SequenceFilters::default).write()?.register(filter)`,
/// and every pane consults it from its next chunk of output.
#[derive(Default, Clone)]
pub struct SequenceFilters {
    filters: Vec<(FilterInterest, Arc<dyn SequenceFilter>)>,
    interest: FilterInterest,
}

impl SequenceFilters {
    /// Add `filter`; it is asked after those already registered.
    pub fn register(&mut self, filter: Arc<dyn SequenceFilter>) {
        let interest = filter.interest();
        self.interest = self.interest.union(interest);
        self.filters.push((interest, filter));
    }

    /// Whether no filter is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Whether every interested filter keeps this CSI sequence.
    #[must_use]
    pub fn keep_csi(&self, sequence: &SequenceHeader<'_>) -> bool {
        let action = sequence.action();
        !self.interest.wants_csi(action)
            || self
                .filters
                .iter()
                .filter(|(interest, _)| interest.wants_csi(action))
                .all(|(_, filter)| filter.keep_csi(sequence))
    }

    /// Whether every interested filter keeps this OSC string.
    #[must_use]
    pub fn keep_osc(&self, params: &[&[u8]]) -> bool {
        let number = params.first().and_then(|first| osc_number(first));
        !self.interest.wants_osc(number)
            || self
                .filters
                .iter()
                .filter(|(interest, _)| interest.wants_osc(number))
                .all(|(_, filter)| filter.keep_osc(params))
    }

    /// Whether every interested filter keeps this ESC sequence.
    #[must_use]
    pub fn keep_esc(&self, intermediates: &[u8], byte: u8) -> bool {
        !self.interest.wants_esc(byte)
            || self
                .filters
                .iter()
                .filter(|(interest, _)| interest.wants_esc(byte))
                .all(|(_, filter)| filter.keep_esc(intermediates, byte))
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
