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
    /// Activate a second pane runtime on the running server's runtime directory, as a
    /// command run in a client (or the CLI) does.
    Bystander,
    /// Observe a pane as a client does (its frames, applied to a replica of its grid), or
    /// stop observing it.
    Observe,
    /// Attach to a pane's session as a client does and take a snapshot of the pane's grid, or
    /// detach.
    Attach,
    /// Run a program through the execution service as its clients do (reserve, launch,
    /// retry, reconcile, retire).
    Execute,
    /// Persist the server's panes as the snapshot orchestration does (a crash's successor
    /// restores them).
    Snapshot,
}

impl Kind {
    /// Every kind, in the order profiles list them.
    pub const ALL: [Self; 15] = [
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
        Self::Bystander,
        Self::Observe,
        Self::Attach,
        Self::Execute,
        Self::Snapshot,
    ];

    /// Whether it takes the server to itself: drivers pause, and it waits for every action
    /// under way to end (a crash cancels them), rather than run beside them and be cancelled by
    /// a crash.
    #[must_use]
    pub const fn exclusive(self) -> bool {
        matches!(self, Self::Crash | Self::PowerLoss | Self::Bystander)
    }

    /// The draw epoch its weight belongs to (see [`LAST_EPOCH`]).
    const fn epoch(self) -> u8 {
        match self {
            Self::Bystander => 2,
            Self::Observe => 3,
            Self::Attach => 4,
            Self::Execute => 5,
            Self::Snapshot => 6,
            _ => 0,
        }
    }

    /// Draw its weight (0 for off) with `draw(bound)`.
    fn draw_weight(self, draw: &mut impl FnMut(u64) -> u64) -> u64 {
        if self.epoch() > 0 {
            // Added later: on in one profile in three, and lightly.
            return if draw(3) == 0 { 1 + draw(4) } else { 0 };
        }
        // Opening and waiting are always on: without panes nothing else happens, and without
        // waits nothing has time to.
        let always = matches!(self, Self::Open | Self::Wait);
        if always || draw(2) == 0 {
            1 + draw(8)
        } else {
            0
        }
    }

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
            Self::Bystander => "bystander",
            Self::Observe => "observe",
            Self::Attach => "attach",
            Self::Execute => "execute",
            Self::Snapshot => "snapshot",
        }
    }
}

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

/// The last of profiles' draw epochs.
///
/// A drawn profile draws its fields epoch by epoch: epoch 0 holds the fields profiles began
/// with, 1 the thread-spawn and wall-clock faults, 2 the bystander. A field added once profiles
/// are in use belongs to a new epoch, drawn after every earlier one, so every earlier field (and
/// so every regression seed's profile) draws as before it existed.
const LAST_EPOCH: u8 = 6;

/// The draw epoch of [`Profile::managed`].
const MANAGED_EPOCH: u8 = 5;

/// How a rate fault is drawn.
#[derive(Clone, Copy)]
enum RateDraw {
    /// On in one profile in this many, at one of these rates.
    OneIn(u64, &'static [f64]),
    /// Always one of these rates.
    OneOf(&'static [f64]),
}

/// A rate fault in a profile: its name there, its field, how it is drawn, and its draw epoch.
struct Rate {
    name: &'static str,
    get: fn(&FaultRates) -> f64,
    set: fn(&mut FaultRates, f64),
    draw: RateDraw,
    epoch: u8,
}

/// Every rate fault, in the order profiles list them (and, within an epoch, draw them).
const RATES: [Rate; 11] = [
    Rate {
        name: "eintr",
        get: |f| f.interrupted,
        set: |f, rate| f.interrupted = rate,
        draw: RateDraw::OneIn(3, &[0.01, 0.05, 0.2]),
        epoch: 0,
    },
    Rate {
        name: "short",
        get: |f| f.short,
        set: |f, rate| f.short = rate,
        draw: RateDraw::OneIn(3, &[0.05, 0.2, 0.5]),
        epoch: 0,
    },
    Rate {
        name: "spawn",
        get: |f| f.spawn,
        set: |f, rate| f.spawn = rate,
        draw: RateDraw::OneIn(4, &[0.01, 0.05]),
        epoch: 0,
    },
    Rate {
        name: "eio",
        get: |f| f.read_error,
        set: |f, rate| f.read_error = rate,
        draw: RateDraw::OneIn(6, &[0.001, 0.01]),
        epoch: 0,
    },
    Rate {
        name: "stall",
        get: |f| f.write_stall,
        set: |f, rate| f.write_stall = rate,
        draw: RateDraw::OneIn(4, &[0.01, 0.1]),
        epoch: 0,
    },
    Rate {
        name: "kill",
        get: |f| f.kill,
        set: |f, rate| f.kill = rate,
        draw: RateDraw::OneIn(5, &[0.05, 0.2, 1.0]),
        epoch: 0,
    },
    Rate {
        name: "late",
        get: |f| f.signal_delay,
        set: |f, rate| f.signal_delay = rate,
        draw: RateDraw::OneIn(4, &[0.2, 1.0]),
        epoch: 0,
    },
    Rate {
        name: "query",
        get: |f| f.startup_query,
        set: |f, rate| f.startup_query = rate,
        draw: RateDraw::OneIn(3, &[0.3, 1.0]),
        epoch: 0,
    },
    Rate {
        name: "reuse",
        get: |f| f.pid_reuse,
        set: |f, rate| f.pid_reuse = rate,
        draw: RateDraw::OneOf(&[0.0, 0.3, 0.6, 1.0]),
        epoch: 0,
    },
    Rate {
        name: "threadspawn",
        get: |f| f.thread_spawn,
        set: |f, rate| f.thread_spawn = rate,
        draw: RateDraw::OneIn(5, &[0.005, 0.02]),
        epoch: 1,
    },
    Rate {
        name: "clockjump",
        get: |f| f.wall_clock,
        set: |f, rate| f.wall_clock = rate,
        draw: RateDraw::OneIn(4, &[0.0001, 0.001, 0.01]),
        epoch: 1,
    },
];

impl Rate {
    /// Draw its rate with `draw(bound)`.
    fn draw(&self, draw: &mut impl FnMut(u64) -> u64) -> f64 {
        let one_of = |choices: &[f64], draw: &mut dyn FnMut(u64) -> u64| {
            choices[usize::try_from(draw(choices.len() as u64)).unwrap_or(0)]
        };
        match self.draw {
            RateDraw::OneIn(on_one_in, choices) => {
                if draw(on_one_in) == 0 {
                    one_of(choices, draw)
                } else {
                    0.0
                }
            }
            RateDraw::OneOf(choices) => one_of(choices, draw),
        }
    }
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
    /// Whether the server runs as a managed companion of one execution target (its
    /// executions' store is the target's, held by one writer at a time).
    pub managed: bool,
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
            weights: [3, 4, 2, 2, 1, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0],
            faults: FaultRates {
                pid_reuse: 0.3,
                ..FaultRates::default()
            },
            programs: [1, 0, 0, 0, 0, 0, 0, 0, 0],
            sweep: true,
            managed: false,
        }
    }
}

impl Profile {
    /// This profile without what makes a run's outcome hang on timing: one driver, no faults,
    /// no crashes, restarts, interrupts, power losses or bystanders, and no programs that exit
    /// at once (whether one ended before its pane adopted it is a race). What is left should
    /// end the same on every system, which `--metamorphic` checks.
    #[must_use]
    pub fn calm(mut self) -> Self {
        self.drivers = 1;
        self.faults = FaultRates::default();
        for (kind, weight) in Kind::ALL.iter().zip(self.weights.iter_mut()) {
            if matches!(
                kind,
                Kind::Crash | Kind::Restart | Kind::Interrupt | Kind::PowerLoss | Kind::Bystander
            ) {
                *weight = 0;
            }
        }
        for (kind, weight) in programs::Kind::ALL.iter().zip(self.programs.iter_mut()) {
            if matches!(kind, programs::Kind::Instant | programs::Kind::Orphaner) {
                *weight = 0;
            }
        }
        if self.programs.iter().all(|weight| *weight == 0) {
            self.programs[0] = 1;
        }
        self
    }

    /// Draw a profile with `draw(bound)`, which returns a number below `bound`.
    pub fn draw(draw: impl FnMut(u64) -> u64) -> Self {
        Self::draw_through(draw, LAST_EPOCH)
    }

    /// Draw a profile's fields of epochs up to `last` (see [`LAST_EPOCH`]); later epochs'
    /// fields stay off.
    fn draw_through(mut draw: impl FnMut(u64) -> u64, last: u8) -> Self {
        let mut weights = [0; Kind::ALL.len()];
        for (kind, weight) in Kind::ALL.iter().zip(weights.iter_mut()) {
            if kind.epoch() == 0 {
                *weight = kind.draw_weight(&mut draw);
            }
        }
        let mut faults = FaultRates::default();
        for rate in RATES.iter().filter(|rate| rate.epoch == 0) {
            (rate.set)(&mut faults, rate.draw(&mut draw));
        }
        faults.file = if draw(4) == 0 {
            [20, 100, 500][usize::try_from(draw(3)).unwrap_or(0)]
        } else {
            0
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
            sweep: draw(4) != 0,
            managed: false,
            drivers: 1 + draw(4),
            steps: 5 + draw(56),
            panes: 1 + usize::try_from(draw(5)).unwrap_or(0),
            clock: [1, 10, 100][usize::try_from(draw(3)).unwrap_or(0)],
            weights,
            faults,
        };
        for epoch in 1..=last {
            for rate in RATES.iter().filter(|rate| rate.epoch == epoch) {
                (rate.set)(&mut profile.faults, rate.draw(&mut draw));
            }
            for kind in Kind::ALL.into_iter().filter(|kind| kind.epoch() == epoch) {
                let weight = kind.draw_weight(&mut draw);
                profile.set_weight(kind, weight);
            }
            if epoch == MANAGED_EPOCH {
                profile.managed = draw(2) == 0;
            }
        }
        profile
    }

    /// Set `kind`'s weight.
    fn set_weight(&mut self, kind: Kind, weight: u64) {
        if let Some(index) = Kind::ALL.iter().position(|k| *k == kind) {
            self.weights[index] = weight;
        }
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
        for rate in &RATES {
            if (rate.get)(&self.faults) > 0.0 && rate.name != "reuse" {
                tags.push(format!("fault:{}", rate.name));
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
        for rate in &RATES {
            write!(f, " {}={}", rate.name, (rate.get)(&self.faults))?;
        }
        write!(f, " fsfault={}", self.faults.file)?;
        write!(f, " sweep={}", u8::from(self.sweep))?;
        write!(f, " managed={}", u8::from(self.managed))?;
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
                name if let Some(field) = RATES.iter().find(|field| field.name == name) => {
                    (field.set)(faults, rate()?);
                }
                "fsfault" => faults.file = number()?,
                "sweep" => profile.sweep = number()? != 0,
                "managed" => profile.managed = number()? != 0,
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

    /// Every drawn profile, for 300 seeds, as it printed when this test was written. Changing a
    /// profile's draws changes every run's (and every regression seed's) shape, so it must be
    /// deliberate: regenerate with `UPDATE_PROFILE_DRAWS=1`.
    #[test]
    fn drawn_profiles_are_pinned() {
        use std::fmt::Write as _;

        let mut drawn = String::new();
        for seed in 0..300_u64 {
            // Scrambled, so neighbouring seeds draw unlike profiles.
            let mut state = seed.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let profile = Profile::draw(|bound| {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                (state >> 33) % bound
            });
            writeln!(drawn, "{seed} {profile}").unwrap();
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/profile-draws.txt");
        if std::env::var_os("UPDATE_PROFILE_DRAWS").is_some() {
            std::fs::write(path, &drawn).unwrap();
        }
        let pinned = std::fs::read_to_string(path).unwrap();
        for (line, (now, then)) in drawn.lines().zip(pinned.lines()).enumerate() {
            assert_eq!(now, then, "profile {line} draws differently");
        }
        assert_eq!(drawn.lines().count(), pinned.lines().count());
    }

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
        assert!(spawns > 0 && jumps > 0, "{spawns} {jumps}");
        let forced: Profile = "threadspawn=0.01 clockjump=0.001".parse().unwrap();
        assert!(forced.faults.thread_spawn > 0.0 && forced.faults.wall_clock > 0.0);
        assert_eq!(forced.to_string().parse::<Profile>().unwrap(), forced);
    }

    /// `profile` with the fields of epochs from `epoch` on turned off.
    fn before_epoch(mut profile: Profile, epoch: u8) -> Profile {
        for rate in RATES.iter().filter(|rate| rate.epoch >= epoch) {
            (rate.set)(&mut profile.faults, 0.0);
        }
        for kind in Kind::ALL.into_iter().filter(|kind| kind.epoch() >= epoch) {
            profile.set_weight(kind, 0);
        }
        if epoch <= MANAGED_EPOCH {
            profile.managed = false;
        }
        profile
    }

    #[test]
    fn each_epoch_leaves_earlier_epochs_draws_as_they_were() {
        for epoch in 1..=LAST_EPOCH {
            for seed in 0..100_u64 {
                let drawn = |last| {
                    let mut state = seed;
                    let mut draws = Vec::new();
                    let profile = Profile::draw_through(
                        |bound| {
                            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                            draws.push(bound);
                            (state >> 33) % bound
                        },
                        last,
                    );
                    (profile, draws)
                };
                let (before, before_draws) = drawn(epoch - 1);
                let (after, after_draws) = drawn(epoch);
                assert!(after_draws.starts_with(&before_draws), "epoch {epoch} seed {seed}");
                assert_eq!(before_epoch(after, epoch), before, "epoch {epoch} seed {seed}");
            }
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
