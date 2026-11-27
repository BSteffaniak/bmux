//! Moving through a run in time.
//!
//! A run's seed replays it exactly, so any moment of it can be reached again by replaying to
//! that decision. To make that fast enough to scrub, a [`TimeTravel`] keeps a few replays
//! *parked*: paused at evenly spaced decisions (keyframes). Going to decision `n` takes the
//! closest replay parked at or before `n` (or the one being shown, when it is behind `n`) and
//! lets it run forward to `n`, so going back costs at most one keyframe interval. The replay
//! taken is replaced by a new one parked at its old place.
//!
//! Replays run in the simulation's process, on threads of their own, through a
//! [`Controller`]; the fleet keeps running beside them.

use std::sync::Arc;

use simvar_observer::{ReplayControl, ReplayMode, RunId};

use crate::Controller;

/// Replays parked along a run, at most.
pub const KEYFRAMES: usize = 8;

/// One replay of the run.
#[derive(Debug, Clone)]
pub struct Replay {
    /// The replay's run, once it started.
    pub run: Option<RunId>,
    /// What holds it.
    pub control: Arc<ReplayControl>,
    /// Where it is going (or parked).
    pub target: u64,
}

impl Replay {
    fn start(controller: &dyn Controller, seed: u64, epoch_offset: u64, target: u64) -> Self {
        let control = Arc::new(ReplayControl::new(ReplayMode::Until(target)));
        let run = controller.replay(seed, epoch_offset, Some(Arc::clone(&control)));
        Self {
            run,
            control,
            target,
        }
    }

    /// The decision it is at (or reached last).
    #[must_use]
    pub fn position(&self) -> u64 {
        self.control.status().1
    }

    /// Whether it is paused where it was sent.
    #[must_use]
    pub fn parked(&self) -> bool {
        let (mode, at) = self.control.status();
        matches!(mode, ReplayMode::Pause) && at == self.target
    }

    /// Let it run to its end, unobserved (a replay is dropped by letting it finish).
    fn release(&self) {
        self.control.clear_breakpoints();
        self.control.set(ReplayMode::Run);
    }
}

/// Where a run is being shown, and the replays that get it anywhere quickly.
#[derive(Debug)]
pub struct TimeTravel {
    /// The run's seed.
    pub seed: u64,
    /// Its epoch offset.
    pub epoch_offset: u64,
    /// Its length in decisions, once known.
    pub length: Option<u64>,
    /// The replay being shown.
    pub cursor: Replay,
    /// Replays parked along the run.
    pub keyframes: Vec<Replay>,
    /// Decisions to advance per frame while playing, when playing.
    pub playing: Option<u64>,
    /// The shown replay's run ended, so it cannot go further: moves start elsewhere.
    pub ended: bool,
}

impl TimeTravel {
    /// Start showing the run of `seed` at decision `at`, parking keyframes along `length`
    /// decisions when it is known.
    pub fn start(
        controller: &dyn Controller,
        seed: u64,
        epoch_offset: u64,
        at: u64,
        length: Option<u64>,
    ) -> Self {
        let mut travel = Self {
            seed,
            epoch_offset,
            length,
            cursor: Replay::start(controller, seed, epoch_offset, at),
            keyframes: Vec::new(),
            playing: None,
            ended: false,
        };
        travel.park_keyframes(controller);
        travel
    }

    /// Where keyframes belong for a run of `length` decisions.
    #[must_use]
    pub fn keyframe_positions(length: u64) -> Vec<u64> {
        if length < 2 {
            return Vec::new();
        }
        let count = u64::try_from(KEYFRAMES).unwrap_or(8);
        let mut positions: Vec<u64> = (0..count).map(|i| i * length / count).collect();
        positions.dedup();
        positions
    }

    fn park_keyframes(&mut self, controller: &dyn Controller) {
        let Some(length) = self.length else {
            return;
        };
        for position in Self::keyframe_positions(length) {
            if self.keyframes.iter().all(|k| k.target != position) {
                self.keyframes.push(Replay::start(
                    controller,
                    self.seed,
                    self.epoch_offset,
                    position,
                ));
            }
        }
        self.keyframes.sort_by_key(|k| k.target);
    }

    /// Learn the run's length (from its index), parking keyframes along it.
    pub fn set_length(&mut self, controller: &dyn Controller, length: u64) {
        if self.length != Some(length) {
            self.length = Some(length);
            self.park_keyframes(controller);
        }
    }

    /// The decision being shown.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.cursor.position()
    }

    /// Where the shown replay is going.
    #[must_use]
    pub const fn target(&self) -> u64 {
        self.cursor.target
    }

    /// Whether the replay shown is where it was sent (or as far as it goes).
    #[must_use]
    pub fn settled(&self) -> bool {
        self.ended || self.cursor.parked()
    }

    /// Note where the shown replay stopped on its own (at a breakpoint, a failure), so that is
    /// where it is, and stop playing: returns whether it had. Playing moves a decision at a
    /// time, so such a stop may fall on the target.
    pub fn adopt_stop(&mut self) -> bool {
        let (mode, at) = self.cursor.control.status();
        if matches!(mode, ReplayMode::Pause)
            && (at != self.cursor.target || (self.playing.is_some() && self.cursor.control.broke()))
        {
            self.cursor.target = at;
            self.playing = None;
            return true;
        }
        false
    }

    /// The shown replay's run ended (it ran to its end): it stays shown where it ended, and
    /// later moves start from a keyframe or a new replay.
    pub fn cursor_ended(&mut self) {
        if !self.ended {
            self.ended = true;
            self.cursor.target = self.cursor.position();
            self.playing = None;
        }
    }

    /// Show decision `target`.
    pub fn seek(&mut self, controller: &dyn Controller, target: u64) {
        let target = self
            .length
            .map_or(target, |length| target.min(length.saturating_sub(1)));
        let here = self.cursor.position();
        // The shown replay only goes forward, while its run lasts.
        if !self.ended && here <= target {
            self.cursor.target = target;
            self.cursor.control.set(ReplayMode::Until(target));
            return;
        }
        let best = self
            .keyframes
            .iter()
            .enumerate()
            .filter(|(_, k)| k.parked() && k.target <= target)
            .max_by_key(|(_, k)| k.target)
            .map(|(index, _)| index);
        let next = match best {
            Some(index) => {
                let keyframe = self.keyframes.remove(index);
                // Refill the keyframe taken.
                self.keyframes.push(Replay::start(
                    controller,
                    self.seed,
                    self.epoch_offset,
                    keyframe.target,
                ));
                self.keyframes.sort_by_key(|k| k.target);
                keyframe
            }
            None => Replay::start(controller, self.seed, self.epoch_offset, target),
        };
        let previous = std::mem::replace(&mut self.cursor, next);
        previous.release();
        self.ended = false;
        self.cursor.target = target;
        self.cursor.control.set(ReplayMode::Until(target));
    }

    /// Show `by` decisions later (or earlier, for a negative `by`).
    pub fn step(&mut self, controller: &dyn Controller, by: i64) {
        let here = self.cursor.target;
        let target = if by < 0 {
            here.saturating_sub(by.unsigned_abs())
        } else {
            here.saturating_add(by.unsigned_abs())
        };
        self.seek(controller, target);
    }

    /// Play (advancing `speed` decisions a frame) or pause.
    pub const fn set_playing(&mut self, speed: Option<u64>) {
        self.playing = speed;
    }

    /// Advance a playing replay by a frame. Returns whether anything moved.
    pub fn frame(&mut self, controller: &dyn Controller) -> bool {
        let Some(speed) = self.playing else {
            return false;
        };
        if !self.settled() {
            return true;
        }
        let end = self.length.map(|l| l.saturating_sub(1));
        if self.ended || end.is_some_and(|end| self.position() >= end) {
            self.playing = None;
            return false;
        }
        self.step(controller, i64::try_from(speed).unwrap_or(i64::MAX));
        true
    }

    /// Let every replay finish: the run is no longer shown.
    pub fn close(&self) {
        self.cursor.release();
        for keyframe in &self.keyframes {
            keyframe.release();
        }
    }

    /// The runs of every replay.
    #[must_use]
    pub fn runs(&self) -> Vec<RunId> {
        std::iter::once(&self.cursor)
            .chain(&self.keyframes)
            .filter_map(|replay| replay.run)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{Controller, FleetState};

    /// A controller whose replays move only when [`Self::settle`] says so.
    #[derive(Default)]
    pub struct FakeController {
        pub replays: Mutex<Vec<(u64, Arc<ReplayControl>)>>,
    }

    impl FakeController {
        /// Move every replay as far as its control lets it.
        pub fn settle(&self) {
            for (_, control) in self.replays.lock().unwrap().iter() {
                let (mode, at) = control.status();
                if let ReplayMode::Until(target) = mode
                    && at <= target
                {
                    for number in at..=target {
                        if control.arrive(number, 0, &Default::default) {
                            break;
                        }
                    }
                }
            }
        }
    }

    impl Controller for FakeController {
        fn status(&self) -> FleetState {
            FleetState::default()
        }
        fn pause(&self) {}
        fn resume(&self) {}
        fn set_max_parallel(&self, _: u64) {}
        fn stop(&self) {}
        fn replay(&self, seed: u64, _: u64, control: Option<Arc<ReplayControl>>) -> Option<RunId> {
            let mut replays = self.replays.lock().unwrap();
            replays.push((seed, control.unwrap()));
            Some(RunId {
                worker: 0,
                run: u64::try_from(replays.len()).unwrap(),
            })
        }
        fn shrink(&self, _: u64, _: u64) -> Option<RunId> {
            None
        }
        fn verify(&self, _: u64, _: u64) -> Option<RunId> {
            None
        }
    }

    #[test]
    fn keyframes_spread_along_the_run() {
        assert_eq!(
            TimeTravel::keyframe_positions(800),
            [0, 100, 200, 300, 400, 500, 600, 700]
        );
        assert_eq!(TimeTravel::keyframe_positions(3), [0, 1, 2]);
        assert_eq!(TimeTravel::keyframe_positions(1), Vec::<u64>::new());
    }

    #[test]
    fn going_back_takes_the_nearest_keyframe_and_refills_it() {
        let controller = FakeController::default();
        let mut travel = TimeTravel::start(&controller, 42, 0, 750, Some(800));
        controller.settle();
        assert_eq!(travel.position(), 750);
        assert!(travel.settled());
        let started = controller.replays.lock().unwrap().len();
        travel.seek(&controller, 330);
        // The keyframe at 300 became the cursor; a new one replaces it.
        assert_eq!(controller.replays.lock().unwrap().len(), started + 1);
        controller.settle();
        assert_eq!(travel.position(), 330);
        assert_eq!(travel.keyframes.len(), KEYFRAMES);
        assert!(travel.keyframes.iter().any(|k| k.target == 300));
        // Forward never needs a new replay.
        travel.seek(&controller, 400);
        controller.settle();
        assert_eq!(travel.position(), 400);
        assert_eq!(controller.replays.lock().unwrap().len(), started + 1);
    }

    #[test]
    fn a_replay_that_stops_on_its_own_or_ends_stays_where_it_stopped() {
        let controller = FakeController::default();
        let mut travel = TimeTravel::start(&controller, 1, 0, u64::MAX, None);
        travel
            .cursor
            .control
            .add_breakpoint(simvar_observer::Breakpoint::Decision(40));
        controller.settle();
        assert!(travel.adopt_stop());
        assert_eq!(travel.position(), 40);
        assert!(travel.settled());
        // Its run ends: going back needs a new replay.
        travel.cursor_ended();
        assert!(travel.settled());
        let before = controller.replays.lock().unwrap().len();
        travel.seek(&controller, 41);
        assert_eq!(controller.replays.lock().unwrap().len(), before + 1);
        assert!(!travel.ended);
    }

    #[test]
    fn playing_stops_at_a_breakpoint() {
        let controller = FakeController::default();
        let mut travel = TimeTravel::start(&controller, 1, 0, 0, Some(100));
        controller.settle();
        travel
            .cursor
            .control
            .add_breakpoint(simvar_observer::Breakpoint::Decision(3));
        travel.set_playing(Some(1));
        for _ in 0..10 {
            travel.adopt_stop();
            travel.frame(&controller);
            controller.settle();
        }
        assert_eq!(travel.playing, None);
        assert_eq!(travel.position(), 3);
        assert_eq!(
            travel.cursor.control.hit().as_deref(),
            Some("break at decision 3")
        );
    }

    #[test]
    fn stepping_and_playing_move_the_cursor() {
        let controller = FakeController::default();
        let mut travel = TimeTravel::start(&controller, 1, 0, 10, Some(100));
        controller.settle();
        travel.step(&controller, 5);
        controller.settle();
        assert_eq!(travel.position(), 15);
        travel.step(&controller, -12);
        controller.settle();
        assert_eq!(travel.position(), 3);
        travel.set_playing(Some(40));
        for _ in 0..3 {
            assert!(travel.frame(&controller));
            controller.settle();
        }
        assert_eq!(travel.position(), 99);
        assert!(!travel.frame(&controller));
        assert_eq!(travel.playing, None);
    }
}
