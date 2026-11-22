//! A run's profile (swarm testing): which actions it does and how often, how many drivers do
//! them at once, how hostile its kernel is, and how fast its clock runs.
//!
//! Each run draws its own profile from its seed, so one run is all crashes and restarts,
//! another a resize storm, another a flood of input under failing reads. Some bugs need a lot
//! of one thing and none of another, which one uniform mix almost never produces.
//!
//! A profile prints as `key=value` words and parses back, so `PANE_RUNTIME_SIM_PROFILE` can
//! force one (for a run a guided fleet mutated, or to pin a failing run's mix while the
//! scheduler explores).

use std::fmt;

use crate::programs;

/// What a driver can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Open a pane.
    Open,
    /// Type a line into a pane.
    Type,
    /// Resize a pane.
    Resize,
    /// Close a pane.
    Close,
    /// Crash the server and start a new one on the same runtime directory.
    Crash,
    /// Restart a pane's program.
    Restart,
    /// Do nothing for a while.
    Wait,
    /// Split a pane: a new pane, in its session, runs a new program.
    Split,
    /// Interrupt a pane's program (`ctrl+c`).
    Interrupt,
    /// Lose power: every process dies, and the file system keeps only what was made durable
    /// (and a random part of the rest), then a new server recovers.
    PowerLoss,
}

impl Kind {
    /// Every kind, in the order profiles list them.
    pub const ALL: [Self; 10] = [
        Self::Open,
        Self::Type,
        Self::Resize,
        Self::Close,
        Self::Crash,
        Self::Restart,
        Self::Wait,
        Self::PowerLoss,
        Self::Split,
        Self::Interrupt,
    ];

    /// Its name in a profile.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Type => "type",
            Self::Resize => "resize",
            Self::Close => "close",
            Self::Crash => "crash",
            Self::Restart => "restart",
            Self::Wait => "wait",
            Self::PowerLoss => "power",
            Self::Split => "split",
            Self::Interrupt => "interrupt",
        }
    }
}

/// Whether profiles turn thread spawn failures on (drawn, or by a guided mutation).
///
/// Off for now: `switchy_async`'s simulator expects the spawns of its own threads to succeed
/// (`spawn_blocking`'s thread, and its executor workers), so a failing one panics the run there
/// before the pane runtime's own failure paths matter. The draws stay, so turning it on shifts
/// nothing. A profile with `threadspawn=` (or `SIMULATOR_THREAD_SPAWN_FAILURES`) still forces
/// it.
pub const THREAD_SPAWN_FAULTS: bool = false;

/// Whether profiles turn wall clock jumps on (drawn, or by a guided mutation).
///
/// Off for now: simvar's harness measures each run's elapsed time on the simulated wall clock
/// (`switchy::time::now()`, then `duration_since(..).unwrap()`) after the run, outside its
/// panic catching, so a run whose clock ends behind where it started panics the fleet worker:
/// the run disappears and the runs it cut short fail determinism checks. The draws stay, so
/// turning it on shifts nothing. A profile with `clockjump=` (or `SIMULATOR_WALL_CLOCK_JUMPS`)
/// still forces it.
pub const WALL_CLOCK_JUMPS: bool = false;

/// Rates of the simulated kernel's faults (see `switchy_pty::simulator::Faults`), and how
/// often process ids are reused.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FaultRates {
    /// Interrupted master reads, writes and polls.
    pub interrupted: f64,
    /// Short master reads and writes.
    pub short: f64,
    /// Failed spawns.
    pub spawn: f64,
    /// Master reads failing with an input/output error.
    pub read_error: f64,
    /// Master writes stalling.
    pub write_stall: f64,
    /// Outside kills per simulated second.
    pub kill: f64,
    /// Signals delivered late.
    pub signal_delay: f64,
    /// Pseudo-consoles asking for the cursor late or never.
    pub startup_query: f64,
    /// One file operation in this many fails (`EIO`, `ENOSPC`, short writes); 0 for none.
    pub file: u64,
    /// New processes reusing an old id.
    pub pid_reuse: f64,
    /// Simulated thread spawns failing (`switchy_thread`'s `set_spawn_failures`).
    pub thread_spawn: f64,
    /// The wall clock jumping, at each clock advance (`switchy_time`'s
    /// `set_wall_clock_jumps`).
    pub wall_clock: f64,
}

/// One run's profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Drivers acting at once.
    pub drivers: u64,
    /// Actions taken in all, across drivers.
    pub steps: u64,
    /// Panes open (or opening) at most.
    pub panes: usize,
    /// Milliseconds of simulated time per scheduling step.
    pub clock: u64,
    /// Each kind's weight (0 for off), in [`Kind::ALL`]'s order.
    pub weights: [u64; Kind::ALL.len()],
    /// The kernel's faults.
    pub faults: FaultRates,
    /// Whether panes' programs' leftover jobs are swept when the program exits.
    pub sweep: bool,
    /// Each program's weight among those panes run (0 for never), in
    /// [`programs::Kind::ALL`]'s order.
    pub programs: [u64; programs::Kind::ALL.len()],
}

impl Default for Profile {
    /// The mix every run had before profiles: one driver, every action, no faults.
    fn default() -> Self {
        Self {
            drivers: 1,
            steps: 20,
            panes: 3,
            clock: 1,
            weights: [3, 4, 2, 2, 1, 1, 3, 0, 0, 0],
            faults: FaultRates {
                pid_reuse: 0.3,
                ..FaultRates::default()
            },
            programs: [1, 0, 0, 0, 0, 0, 0, 0, 0],
            sweep: true,
        }
    }
}

impl Profile {
    /// Draw a profile with `draw(bound)`, which returns a number below `bound`.
    pub fn draw(mut draw: impl FnMut(u64) -> u64) -> Self {
        let mut weights = [0; Kind::ALL.len()];
        for (index, weight) in weights.iter_mut().enumerate() {
            // Opening and waiting are always on: without panes nothing else happens, and
            // without waits nothing has time to.
            let always = matches!(Kind::ALL[index], Kind::Open | Kind::Wait);
            if always || draw(2) == 0 {
                *weight = 1 + draw(8);
            }
        }
        let mut chosen = |on_one_in: u64, choices: &[f64]| {
            if draw(on_one_in) == 0 {
                choices[usize::try_from(draw(choices.len() as u64)).unwrap_or(0)]
            } else {
                0.0
            }
        };
        let faults = FaultRates {
            interrupted: chosen(3, &[0.01, 0.05, 0.2]),
            short: chosen(3, &[0.05, 0.2, 0.5]),
            spawn: chosen(4, &[0.01, 0.05]),
            read_error: chosen(6, &[0.001, 0.01]),
            write_stall: chosen(4, &[0.01, 0.1]),
            kill: chosen(5, &[0.05, 0.2, 1.0]),
            signal_delay: chosen(4, &[0.2, 1.0]),
            startup_query: chosen(3, &[0.3, 1.0]),
            pid_reuse: [0.0, 0.3, 0.6, 1.0][usize::try_from(draw(4)).unwrap_or(0)],
            file: if draw(4) == 0 {
                [20, 100, 500][usize::try_from(draw(3)).unwrap_or(0)]
            } else {
                0
            },
            // Drawn last (below).
            thread_spawn: 0.0,
            wall_clock: 0.0,
        };
        let mut programs = [0; programs::Kind::ALL.len()];
        for (index, weight) in programs.iter_mut().enumerate() {
            // Mostly well-behaved shells; each misbehaving program in one run in three.
            *weight = if index == 0 {
                2 + draw(5)
            } else if draw(3) == 0 {
                1 + draw(3)
            } else {
                0
            };
        }
        let mut profile = Self {
            programs,
            // Kept on (the draw stays, so turning it back on shifts nothing): with it off, a
            // Windows pane whose shell left an attached job can fail to close (an open finding
            // of phase 18; see the plan). Force it off with `sweep=0` to explore that.
            sweep: {
                let _off = draw(4) == 0;
                true
            },
            drivers: 1 + draw(4),
            steps: 5 + draw(56),
            panes: 1 + usize::try_from(draw(5)).unwrap_or(0),
            clock: [1, 10, 100][usize::try_from(draw(3)).unwrap_or(0)],
            weights,
            faults,
        };
        // Drawn after everything else, so every other field draws as before they existed (and
        // regression seeds without a profile keep theirs).
        if draw(5) == 0 {
            let rate = [0.005, 0.02][usize::try_from(draw(2)).unwrap_or(0)];
            if THREAD_SPAWN_FAULTS {
                profile.faults.thread_spawn = rate;
            }
        }
        if draw(4) == 0 {
            let rate = [0.0001, 0.001, 0.01][usize::try_from(draw(3)).unwrap_or(0)];
            if WALL_CLOCK_JUMPS {
                profile.faults.wall_clock = rate;
            }
        }
        profile
    }

    /// `kind`'s weight.
    #[must_use]
    pub fn weight(&self, kind: Kind) -> u64 {
        Kind::ALL
            .iter()
            .position(|k| *k == kind)
            .map_or(0, |index| self.weights[index])
    }

    /// Choose a kind by weight with `draw(bound)`.
    pub fn choose(&self, draw: impl FnOnce(u64) -> u64) -> Kind {
        let total: u64 = self.weights.iter().sum();
        let mut at = draw(total.max(1));
        for (kind, weight) in Kind::ALL.iter().zip(self.weights) {
            if at < weight {
                return *kind;
            }
            at -= weight;
        }
        Kind::Wait
    }

    /// Tags describing the profile, for grouping runs: the kinds it leans on, its drivers,
    /// and the faults it has on.
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut tags = vec![format!("drivers:{}", self.drivers)];
        let total: u64 = self.weights.iter().sum();
        for (kind, weight) in Kind::ALL.iter().zip(self.weights) {
            if *kind != Kind::Wait && weight * 3 > total {
                tags.push(format!("mostly:{}", kind.name()));
            }
        }
        for (name, rate) in self.fault_list() {
            if rate > 0.0 && name != "reuse" {
                tags.push(format!("fault:{name}"));
            }
        }
        if self.faults.file > 0 {
            tags.push("fault:file".into());
        }
        if !self.sweep {
            tags.push("no sweep".into());
        }
        for (kind, weight) in programs::Kind::ALL.iter().zip(self.programs).skip(1) {
            if weight > 0 {
                tags.push(format!("program:{}", kind.name()));
            }
        }
        tags
    }

    /// The program mix, for [`programs::set_mix`].
    #[must_use]
    pub fn program_mix(&self) -> Vec<(programs::Kind, u64)> {
        programs::Kind::ALL
            .iter()
            .copied()
            .zip(self.programs)
            .filter(|(_, weight)| *weight > 0)
            .collect()
    }

    const fn fault_list(&self) -> [(&'static str, f64); 11] {
        let f = &self.faults;
        [
            ("eintr", f.interrupted),
            ("short", f.short),
            ("spawn", f.spawn),
            ("eio", f.read_error),
            ("stall", f.write_stall),
            ("kill", f.kill),
            ("late", f.signal_delay),
            ("query", f.startup_query),
            ("reuse", f.pid_reuse),
            ("threadspawn", f.thread_spawn),
            ("clockjump", f.wall_clock),
        ]
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "drivers={} steps={} panes={} clock={}",
            self.drivers, self.steps, self.panes, self.clock
        )?;
        for (kind, weight) in Kind::ALL.iter().zip(self.weights) {
            write!(f, " {}={weight}", kind.name())?;
        }
        for (name, rate) in self.fault_list() {
            write!(f, " {name}={rate}")?;
        }
        write!(f, " fsfault={}", self.faults.file)?;
        write!(f, " sweep={}", u8::from(self.sweep))?;
        for (kind, weight) in programs::Kind::ALL.iter().zip(self.programs) {
            write!(f, " p.{}={weight}", kind.name())?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Profile {
    type Err = String;

    /// Parse `key=value` words (separated by spaces or commas); keys left out keep
    /// [`Profile::default`]'s values.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut profile = Self::default();
        // Words are separated by spaces or commas (commas fit a profile in one environment
        // variable on a regression seed's line).
        for word in text.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty()) {
            let (key, value) = word
                .split_once('=')
                .ok_or_else(|| format!("expected key=value, got '{word}'"))?;
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("{key}: expected a whole number, got '{value}'"))
            };
            let rate = || {
                value
                    .parse::<f64>()
                    .map_err(|_| format!("{key}: expected a number, got '{value}'"))
            };
            let faults = &mut profile.faults;
            match key {
                "drivers" => profile.drivers = number()?.max(1),
                "steps" => profile.steps = number()?,
                "panes" => {
                    profile.panes = usize::try_from(number()?).map_err(|e| e.to_string())?;
                }
                "clock" => profile.clock = number()?.max(1),
                "eintr" => faults.interrupted = rate()?,
                "short" => faults.short = rate()?,
                "spawn" => faults.spawn = rate()?,
                "eio" => faults.read_error = rate()?,
                "stall" => faults.write_stall = rate()?,
                "kill" => faults.kill = rate()?,
                "late" => faults.signal_delay = rate()?,
                "query" => faults.startup_query = rate()?,
                "reuse" => faults.pid_reuse = rate()?,
                "threadspawn" => faults.thread_spawn = rate()?,
                "clockjump" => faults.wall_clock = rate()?,
                "fsfault" => faults.file = number()?,
                "sweep" => profile.sweep = number()? != 0,
                other if other.starts_with("p.") => {
                    let index = programs::Kind::ALL
                        .iter()
                        .position(|kind| kind.name() == &other[2..])
                        .ok_or_else(|| format!("unknown program '{other}'"))?;
                    profile.programs[index] = number()?;
                }
                other => {
                    let index = Kind::ALL
                        .iter()
                        .position(|kind| kind.name() == other)
                        .ok_or_else(|| format!("unknown profile key '{other}'"))?;
                    profile.weights[index] = number()?;
                }
            }
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_prints_and_parses_back() {
        let mut state = 7_u64;
        let profile = Profile::draw(|bound| {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 33) % bound
        });
        let parsed: Profile = profile.to_string().parse().unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn profiles_with_the_new_faults_print_and_parse_back() {
        // Enough seeds that some draw each new fault (while profiles turn them on).
        let (mut spawns, mut jumps) = (0, 0);
        for seed in 0..200_u64 {
            let mut state = seed;
            let profile = Profile::draw(|bound| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                (state >> 33) % bound
            });
            spawns += usize::from(profile.faults.thread_spawn > 0.0);
            jumps += usize::from(profile.faults.wall_clock > 0.0);
            let parsed: Profile = profile.to_string().parse().unwrap();
            assert_eq!(parsed, profile, "seed {seed}");
        }
        assert_eq!(spawns > 0, THREAD_SPAWN_FAULTS, "{spawns}");
        assert_eq!(jumps > 0, WALL_CLOCK_JUMPS, "{jumps}");
        let forced: Profile = "threadspawn=0.01 clockjump=0.001".parse().unwrap();
        assert!(forced.faults.thread_spawn > 0.0 && forced.faults.wall_clock > 0.0);
        assert_eq!(forced.to_string().parse::<Profile>().unwrap(), forced);
    }

    #[test]
    fn the_new_faults_are_drawn_after_every_other_field() {
        // So every other field draws as before they existed.
        for seed in 0..50_u64 {
            let mut state = seed;
            let mut draws: Vec<(u64, u64)> = Vec::new();
            Profile::draw(|bound| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let value = (state >> 33) % bound;
                draws.push((bound, value));
                value
            });
            let mut at = draws.len();
            // The wall clock: on (then a rate, of three) or not.
            if draws[at - 1].0 == 3 && draws[at - 2] == (4, 0) {
                at -= 2;
            } else {
                at -= 1;
                assert_eq!(draws[at].0, 4, "seed {seed}: {draws:?}");
                assert_ne!(draws[at].1, 0, "seed {seed}: {draws:?}");
            }
            // Thread spawns: on (then a rate, of two) or not.
            if draws[at - 1].0 == 2 && draws[at - 2] == (5, 0) {
                at -= 2;
            } else {
                at -= 1;
                assert_eq!(draws[at].0, 5, "seed {seed}: {draws:?}");
                assert_ne!(draws[at].1, 0, "seed {seed}: {draws:?}");
            }
            // The clock's draw (one of three rates) comes just before.
            assert_eq!(draws[at - 1].0, 3, "seed {seed}: {draws:?}");
        }
    }

    #[test]
    fn choices_follow_the_weights() {
        let profile: Profile = "open=1 type=0 resize=0 close=0 crash=0 restart=0 wait=1"
            .parse()
            .unwrap();
        assert_eq!(profile.choose(|_| 0), Kind::Open);
        assert_eq!(profile.choose(|_| 1), Kind::Wait);
    }

    #[test]
    fn tags_name_what_a_profile_leans_on() {
        let profile: Profile = "drivers=3 open=1 type=9 resize=0 close=0 crash=0 restart=0 wait=1 kill=0.2"
            .parse()
            .unwrap();
        let tags = profile.tags();
        assert!(tags.contains(&"drivers:3".to_owned()), "{tags:?}");
        assert!(tags.contains(&"mostly:type".to_owned()), "{tags:?}");
        assert!(tags.contains(&"fault:kill".to_owned()), "{tags:?}");
    }
}
