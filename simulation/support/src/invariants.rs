//! A simulation's invariants: declared once, as an enum implementing [`Invariant`], then
//! counted as runs check them ([`checked`]), reported when they fail ([`violated`]), and shown
//! in the dashboard ([`status`]).
//!
//! Each invariant's number comes from its place among the numbered ones in [`Invariant::ALL`],
//! so the crate documentation's list, the dashboard and failure messages agree without
//! repeating numbers by hand.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::Display;

/// One of a simulation's invariants.
pub trait Invariant: Copy + Ord + Send + Sync + 'static {
    /// Every invariant, in the order they are numbered and shown.
    const ALL: &'static [Self];

    /// What it says, briefly, without its number.
    fn statement(self) -> &'static str;

    /// Whether it has a number (in the crate's documentation and its failures' messages). An
    /// invariant that only guards the harness itself (an API misused, say) need not.
    fn numbered(self) -> bool {
        true
    }

    /// Its number: its place among the numbered invariants in [`Self::ALL`], from 1.
    fn number(self) -> Option<usize> {
        if !self.numbered() {
            return None;
        }
        Self::ALL
            .iter()
            .filter(|invariant| invariant.numbered())
            .position(|invariant| *invariant == self)
            .map(|index| index + 1)
    }

    /// How the dashboard and failure messages name it: `6. a crash's sessions end`.
    fn describe(self) -> String {
        self.number().map_or_else(
            || self.statement().to_owned(),
            |number| format!("{number}. {}", self.statement()),
        )
    }
}

/// Each invariant's checks and failures in the current run. A std mutex is never a switch
/// point, so keeping count never changes the run.
struct Checks<I>(
    // simulation-safe: the run's own invariant bookkeeping, locked briefly with no switch point.
    std::sync::Mutex<BTreeMap<I, (u64, Vec<String>)>>,
);

impl<I> Default for Checks<I> {
    fn default() -> Self {
        Self(std::sync::Mutex::default())
    }
}

fn with_checks<I: Invariant, R>(f: impl FnOnce(&mut BTreeMap<I, (u64, Vec<String>)>) -> R) -> R {
    let checks = switchy_simulation::state::<Checks<I>>();
    let mut checks = checks
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut checks)
}

/// Count a check of `invariant` in the current run.
pub fn checked<I: Invariant>(invariant: I) {
    with_checks(|checks| checks.entry(invariant).or_default().0 += 1);
}

/// Record that `invariant` failed, saying `what`.
///
/// The failure goes in the invariants' status and to a watching dashboard (which may pause
/// here). Returns the failure's message (`what`, after the invariant's number) for the run's
/// own list of violations.
pub fn violated<I: Invariant>(invariant: I, what: impl Display) -> String {
    let message = invariant
        .number()
        .map_or_else(|| what.to_string(), |number| format!("{number}. {what}"));
    with_checks(|checks| {
        checks.entry(invariant).or_default().1.push(message.clone());
    });
    crate::failure(&message);
    message
}

/// Every invariant's checks and failures in the current run so far, for the dashboard.
#[must_use]
pub fn status<I: Invariant>() -> bmux_simvar_tui::Invariants {
    with_checks(|checks: &mut BTreeMap<I, (u64, Vec<String>)>| {
        bmux_simvar_tui::Invariants(
            I::ALL
                .iter()
                .map(|invariant| {
                    let (checked, failures) = checks.get(invariant).cloned().unwrap_or_default();
                    bmux_simvar_tui::InvariantStatus {
                        name: invariant.describe(),
                        checked,
                        failures,
                    }
                })
                .collect(),
        )
    })
}

/// [`status`], as a dashboard telemetry payload (for a simulation's `extra`).
#[must_use]
pub fn telemetry<I: Invariant>() -> std::sync::Arc<dyn Any + Send + Sync> {
    std::sync::Arc::new(status::<I>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Example {
        First,
        Unnumbered,
        Second,
    }

    impl Invariant for Example {
        const ALL: &'static [Self] = &[Self::First, Self::Unnumbered, Self::Second];

        fn statement(self) -> &'static str {
            match self {
                Self::First => "first holds",
                Self::Unnumbered => "the harness holds",
                Self::Second => "second holds",
            }
        }

        fn numbered(self) -> bool {
            self != Self::Unnumbered
        }
    }

    #[test]
    fn numbers_skip_unnumbered_invariants() {
        assert_eq!(Example::First.number(), Some(1));
        assert_eq!(Example::Unnumbered.number(), None);
        assert_eq!(Example::Second.number(), Some(2));
        assert_eq!(Example::Second.describe(), "2. second holds");
        assert_eq!(Example::Unnumbered.describe(), "the harness holds");
    }

    #[test]
    fn checks_and_failures_are_counted_per_invariant() {
        checked(Example::First);
        checked(Example::First);
        assert_eq!(violated(Example::Second, "it did not"), "2. it did not");
        let status = status::<Example>().0;
        assert_eq!(status[0].checked, 2);
        assert_eq!(status[2].failures, ["2. it did not"]);
        assert_eq!(status[1].name, "the harness holds");
    }
}
