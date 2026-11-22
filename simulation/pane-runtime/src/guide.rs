//! Guided runs (`--guided`, or `PANE_RUNTIME_SIM_GUIDED=1`): a fleet that steers toward what it
//! has not seen yet.
//!
//! It keeps a corpus of profiles whose runs did something new: a new **outcome** (the kernel's
//! personality, and how many panes each run opened, closed, restarted, crashed, cancelled and
//! failed to open, each bucketed by powers of two). Each new run either takes a corpus entry and mutates it (toggles an action or a
//! program, scales a weight, changes a fault rate, the drivers, the steps or the clock) or draws
//! a fresh profile, both decided by its seed.
//!
//! A guided run's profile depends on the fleet's history, not only on its seed, so it is the
//! run's recorded input: the bootstrap reports it as the run's `PANE_RUNTIME_SIM_PROFILE`
//! property, which goes in the run's replay command, and replays in the same process find it
//! by seed.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, PoisonError},
};

use crate::{
    profile::{Kind, Profile},
    programs,
};

/// The fleet's guide: process-wide (it chooses runs' inputs, as a seed does, outside any run).
static GUIDE: Mutex<Option<Guide>> = Mutex::new(None);

#[derive(Debug, Default)]
struct Guide {
    /// Whether it chooses runs' profiles (or only counts outcomes, to compare).
    steering: bool,
    /// The profile chosen for each seed, for replays.
    assigned: BTreeMap<u64, Profile>,
    corpus: Vec<Profile>,
    shapes: BTreeSet<String>,
}

fn with<R>(f: impl FnOnce(&mut Option<Guide>) -> R) -> R {
    f(&mut GUIDE.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Guide the runs from now on.
pub fn enable() {
    with(|guide| guide.get_or_insert_with(Guide::default).steering = true);
}

/// Count runs' outcomes from now on without guiding them (to compare with guided runs).
pub fn track() {
    with(|guide| {
        guide.get_or_insert_with(Guide::default);
    });
}

/// Whether runs are guided.
#[must_use]
pub fn enabled() -> bool {
    with(|guide| guide.as_ref().is_some_and(|guide| guide.steering))
}

/// Whether runs' outcomes are counted (guided or not).
#[must_use]
pub fn tracking() -> bool {
    with(|guide| guide.is_some())
}

/// A generator fixed by `seed` (`SplitMix64`).
struct Draws(u64);

impl Draws {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % bound.max(1)
    }
}

/// The profile for the run of `seed`: the one chosen before (a replay), or a new one, from the
/// corpus or fresh. `None` when runs are not guided.
#[must_use]
pub fn profile_for(seed: u64) -> Option<Profile> {
    with(|guide| {
        let guide = guide.as_mut().filter(|guide| guide.steering)?;
        if let Some(profile) = guide.assigned.get(&seed) {
            return Some(profile.clone());
        }
        let mut draws = Draws(seed ^ 0x6775_6964_6564);
        let profile = if guide.corpus.is_empty() || draws.below(3) == 0 {
            Profile::draw(|bound| draws.below(bound))
        } else {
            let index = usize::try_from(draws.below(guide.corpus.len() as u64)).unwrap_or(0);
            mutate(&guide.corpus[index], &mut draws)
        };
        guide.assigned.insert(seed, profile.clone());
        Some(profile)
    })
}

/// Record what the run of `profile` did (its outcome: what happened, coarsely): a new one keeps
/// the profile in the corpus.
pub fn observe(profile: &Profile, outcome: &[String]) {
    let mut tags: Vec<&str> = outcome.iter().map(String::as_str).collect();
    tags.sort_unstable();
    tags.dedup();
    let key = tags.join(" ");
    with(|guide| {
        if let Some(guide) = guide.as_mut()
            && guide.shapes.insert(key)
        {
            guide.corpus.push(profile.clone());
        }
    });
}

/// The corpus's size and the shapes seen.
#[must_use]
pub fn progress() -> (usize, usize) {
    with(|guide| {
        guide
            .as_ref()
            .map_or((0, 0), |guide| (guide.corpus.len(), guide.shapes.len()))
    })
}

/// `profile`, changed in one or two ways.
fn mutate(profile: &Profile, draws: &mut Draws) -> Profile {
    let mut profile = profile.clone();
    for _ in 0..=draws.below(2) {
        match draws.below(8) {
            0 => {
                let index = usize::try_from(draws.below(Kind::ALL.len() as u64)).unwrap_or(0);
                let always = matches!(Kind::ALL[index], Kind::Open | Kind::Wait);
                profile.weights[index] = if profile.weights[index] > 0 && !always {
                    0
                } else {
                    1 + draws.below(8)
                };
            }
            1 => {
                let index = usize::try_from(draws.below(Kind::ALL.len() as u64)).unwrap_or(0);
                profile.weights[index] = (profile.weights[index] * 2).clamp(1, 16);
            }
            2 => {
                let index =
                    usize::try_from(draws.below(programs::Kind::ALL.len() as u64)).unwrap_or(0);
                profile.programs[index] = if index == 0 || profile.programs[index] == 0 {
                    1 + draws.below(3)
                } else {
                    0
                };
            }
            3 => {
                let faults = &mut profile.faults;
                let rate = [0.0, 0.01, 0.05, 0.2][usize::try_from(draws.below(4)).unwrap_or(0)];
                match draws.below(8) {
                    0 => faults.interrupted = rate,
                    1 => faults.short = rate,
                    2 => faults.spawn = rate / 4.0,
                    3 => faults.read_error = rate / 10.0,
                    4 => faults.write_stall = rate,
                    5 => faults.kill = rate * 5.0,
                    6 => faults.signal_delay = rate * 5.0,
                    _ => faults.startup_query = rate * 5.0,
                }
            }
            4 => {
                profile.faults.file = [0, 20, 100, 500][usize::try_from(draws.below(4)).unwrap_or(0)];
            }
            5 => profile.drivers = 1 + draws.below(4),
            6 => {
                profile.steps = if draws.below(2) == 0 {
                    (profile.steps * 2).min(200)
                } else {
                    (profile.steps / 2).max(5)
                };
            }
            _ => profile.clock = [1, 10, 100][usize::try_from(draws.below(3)).unwrap_or(0)],
        }
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mutation_is_a_profile_that_prints_and_parses_back() {
        let base = Profile::default();
        for seed in 0..50 {
            let mutated = mutate(&base, &mut Draws(seed));
            let parsed: Profile = mutated.to_string().parse().unwrap();
            assert_eq!(parsed, mutated, "seed {seed}");
        }
    }
}
