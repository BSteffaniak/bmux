//! The programs panes run: a well-behaved shell, and programs that misbehave the way real ones
//! do.
//!
//! Each pane's program is drawn from the run's profile (its mix of [`Kind`]s) when it starts,
//! and recorded, so the workload knows what to expect of it.
//!
//! Every kind but [`Kind::Instant`] and [`Kind::Orphaner`] ends up answering each line it reads
//! with `got:<line>`, as the shell does, after its own antics.

// simulation-safe: see `Programs`.
use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use switchy_pty::{
    Signal,
    simulator::{self, Disposition, Process},
};

/// What a pane's program does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Answers lines; starts a job that ignores hang-ups at `bg`.
    Shell,
    /// A shell that ignores `SIGTERM` and `SIGHUP`: only a kill ends it.
    Stubborn,
    /// Starts a detached child in a session of its own (a daemon), then acts as a shell.
    Daemonizer,
    /// Starts a child that ignores hang-ups and holds the terminal, then exits at once.
    Orphaner,
    /// Exits at once, sometimes after a word.
    Instant,
    /// Writes a flood of output first, so the pane's backpressure kicks in.
    Flood,
    /// Writes invalid UTF-8 and escape sequences split across writes first.
    Garbage,
    /// Asks the terminal where its cursor is and what it is, and waits for the answers.
    Querier,
    /// Switches to the alternate screen and back first.
    AltScreen,
}

impl Kind {
    /// Every kind, in the order profiles list them.
    pub const ALL: [Self; 9] = [
        Self::Shell,
        Self::Stubborn,
        Self::Daemonizer,
        Self::Orphaner,
        Self::Instant,
        Self::Flood,
        Self::Garbage,
        Self::Querier,
        Self::AltScreen,
    ];

    /// Its name in a profile (`p.<name>`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Stubborn => "stubborn",
            Self::Daemonizer => "daemonizer",
            Self::Orphaner => "orphaner",
            Self::Instant => "instant",
            Self::Flood => "flood",
            Self::Garbage => "garbage",
            Self::Querier => "querier",
            Self::AltScreen => "altscreen",
        }
    }
}

/// The run's programs: the mix to draw from, and what each pane's program turned out to be.
#[derive(Debug, Default)]
// simulation-safe: the run's own program mix, locked briefly with no switch point inside.
pub struct Programs(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    mix: Vec<(Kind, u64)>,
    kinds: BTreeMap<u64, Kind>,
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let programs = switchy_simulation::state::<Programs>();
    let mut state = programs.0.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut state)
}

/// Draw each program from `mix` (kinds and weights) for the rest of the run.
pub fn set_mix(mix: Vec<(Kind, u64)>) {
    with(|state| state.mix = mix);
}

/// What the process of `serial` runs, if it is a pane's program.
#[must_use]
pub fn kind_of(serial: u64) -> Option<Kind> {
    with(|state| state.kinds.get(&serial).copied())
}

fn draw(bound: u64) -> u64 {
    switchy_simulation::stream("pane_runtime_programs")
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .below(bound.max(1))
}

fn choose() -> Kind {
    let mix = with(|state| state.mix.clone());
    let total: u64 = mix.iter().map(|(_, weight)| weight).sum();
    let mut at = draw(total);
    for (kind, weight) in mix {
        if at < weight {
            return kind;
        }
        at -= weight;
    }
    Kind::Shell
}

/// The program panes run (registered as `sh`): it draws what to be, records it, and acts so.
pub fn program(process: &mut Process) -> i32 {
    let kind = choose();
    if let Some(serial) = simulator::serial_of(process.pid()) {
        with(|state| state.kinds.insert(serial, kind));
    }
    match kind {
        Kind::Shell => shell(process, Vec::new()),
        Kind::Stubborn => {
            process.handle(Signal::Terminate, Disposition::Ignore);
            process.handle(Signal::Hangup, Disposition::Ignore);
            shell(process, Vec::new())
        }
        Kind::Daemonizer => {
            let _ = process.spawn_session(&["sleep", "3600000"]);
            shell(process, Vec::new())
        }
        Kind::Orphaner => {
            let _ = process.spawn(&["ignore-hup"], false);
            0
        }
        Kind::Instant => {
            if draw(2) == 0 {
                let _ = process.write(b"bye\n");
            }
            i32::try_from(draw(3)).unwrap_or(0)
        }
        Kind::Flood => {
            let total = 4096 * (1 + draw(32));
            let mut written = 0;
            while written < total {
                let size = (1 + draw(4096)).min(total - written);
                let chunk: Vec<u8> = (0..size)
                    .map(|i| b'a' + u8::try_from((written + i) % 26).unwrap_or(0))
                    .collect();
                if process.write(&chunk).is_err() {
                    return 1;
                }
                written += size;
            }
            let _ = process.write(b"\n");
            shell(process, Vec::new())
        }
        Kind::Garbage => {
            let pieces: [&[u8]; 6] = [
                b"\xff\xfe\xc3",
                b"\x28 \x1b[3",
                b"1;4",
                b"m red \x1b]0;ti",
                b"tle\x07 \xe2\x82",
                b"\x1b[0m\n",
            ];
            for piece in pieces {
                if process.write(piece).is_err() {
                    return 1;
                }
            }
            shell(process, Vec::new())
        }
        Kind::Querier => {
            let pending = query(process);
            shell(process, pending)
        }
        Kind::AltScreen => {
            for piece in [&b"\x1b[?1049h"[..], b"in the alternate screen", b"\x1b[?1049l"] {
                if process.write(piece).is_err() {
                    return 1;
                }
            }
            shell(process, Vec::new())
        }
    }
}

/// Ask for the cursor position and the device's attributes, and read until both answers came
/// (or the input ended). Returns what else was typed meanwhile, for the shell.
fn query(process: &mut Process) -> Vec<u8> {
    process.set_raw(true);
    if process.write(b"\x1b[6n\x1b[c").is_err() {
        process.set_raw(false);
        return Vec::new();
    }
    let mut input = Vec::new();
    let mut buffer = [0_u8; 256];
    let (mut cursor, mut attributes) = (false, false);
    while !(cursor && attributes) {
        match process.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => input.extend_from_slice(&buffer[..read]),
        }
        let (rest, found_cursor, found_attributes) = strip_replies(&input);
        input = rest;
        cursor |= found_cursor;
        attributes |= found_attributes;
    }
    process.set_raw(false);
    // Raw mode passed Enter as a carriage return: lines end at either.
    input
        .into_iter()
        .map(|byte| if byte == b'\r' { b'\n' } else { byte })
        .collect()
}

/// `input` without complete cursor reports (`ESC [ n ; m R`) and device attribute replies
/// (`ESC [ ? … c`), and whether each was there.
fn strip_replies(input: &[u8]) -> (Vec<u8>, bool, bool) {
    let mut rest = Vec::new();
    let (mut cursor, mut attributes) = (false, false);
    let mut index = 0;
    while index < input.len() {
        if input[index] == 0x1b && input.get(index + 1) == Some(&b'[') {
            let body = &input[index + 2..];
            if let Some(end) = body.iter().position(u8::is_ascii_alphabetic) {
                match body[end] {
                    b'R' => cursor = true,
                    b'c' => attributes = true,
                    _ => rest.extend_from_slice(&input[index..=index + 2 + end]),
                }
                index += 2 + end + 1;
                continue;
            }
            // An incomplete reply: keep it for the next read.
            rest.extend_from_slice(&input[index..]);
            break;
        }
        rest.push(input[index]);
        index += 1;
    }
    (rest, cursor, attributes)
}

/// The shell: it answers each line it reads with `got:<line>`, starts a job that ignores
/// hang-ups in its session at `bg`, and ends at `exit` or end of input. `pending` is input
/// already read.
fn shell(process: &mut Process, pending: Vec<u8>) -> i32 {
    let mut line = Vec::new();
    let mut buffer = [0_u8; 256];
    let mut input = pending;
    loop {
        for byte in std::mem::take(&mut input) {
            if byte != b'\n' {
                line.push(byte);
                continue;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            line.clear();
            if text == "exit" {
                return 0;
            }
            if text == "bg" && process.spawn(&["ignore-hup"], false).is_err() {
                return 1;
            }
            if process.write(format!("got:{text}\n").as_bytes()).is_err() {
                return 1;
            }
        }
        match process.read(&mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(read) => input.extend_from_slice(&buffer[..read]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_stripped_and_what_was_typed_is_kept() {
        let (rest, cursor, attributes) = strip_replies(b"ab\x1b[3;4Rcd\x1b[?1;2c\r");
        assert_eq!(rest, b"abcd\r");
        assert!(cursor && attributes);
        let (rest, cursor, _) = strip_replies(b"x\x1b[3;");
        assert_eq!(rest, b"x\x1b[3;");
        assert!(!cursor);
    }
}
