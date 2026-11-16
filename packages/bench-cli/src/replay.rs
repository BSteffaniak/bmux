//! `bmux-bench replay`: play a frame corpus into this terminal at a fixed rate and, run inside a
//! BMUX pane whose attach client logs replay markers (`BMUX_REPLAY_LOG`), report how long each
//! frame took from being written here to being handed to the host terminal.

use crate::args::ReplayArgs;
use bmux_bench::corpus::{self, Frame};
use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

fn frames(args: &ReplayArgs) -> Result<Vec<Frame>, String> {
    if let Some(path) = &args.corpus {
        return corpus::read(path).map_err(|error| format!("read {}: {error}", path.display()));
    }
    let size =
        bmux_terminal::terminal::size().map_err(|error| format!("terminal size: {error}"))?;
    let name = args.generated.as_deref().unwrap_or("su-scroll");
    corpus::generated(size, args.frames)
        .into_iter()
        .find(|corpus| corpus.name == name)
        .map(|corpus| corpus.frames)
        .ok_or_else(|| format!("unknown generated corpus: {name}"))
}

/// Play the frames, one per tick, each with its marker. Returns each frame's write time.
fn play(frames: &[Frame], fps: u32) -> Result<BTreeMap<u64, u128>, String> {
    let tick = Duration::from_secs(1) / fps.max(1);
    let mut stdout = std::io::stdout().lock();
    let mut written = BTreeMap::new();
    let write = |stdout: &mut std::io::StdoutLock<'_>, bytes: &[u8]| {
        stdout
            .write_all(bytes)
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("write stdout: {error}"))
    };
    write(&mut stdout, b"\x1b[?1049h\x1b[?25l")?;
    let started = Instant::now();
    for (index, frame) in (0u64..).zip(frames) {
        let due = started + tick * u32::try_from(index).unwrap_or(u32::MAX);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let at = now_ns();
        write(
            &mut stdout,
            &corpus::with_marker(frame, &corpus::marker(index, at)),
        )?;
        written.insert(index, at);
    }
    // Let the last frames reach the host before leaving the screen they are on.
    std::thread::sleep(Duration::from_millis(500));
    write(&mut stdout, b"\x1b[?25h\x1b[?1049l")?;
    Ok(written)
}

fn percentile(sorted: &[f64], percent: usize) -> f64 {
    sorted
        .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or(0.0)
}

/// Latencies in milliseconds for this run's frames found in the log, and how many host frames
/// they arrived in.
fn latencies(log: &str, written: &BTreeMap<u64, u128>) -> (Vec<f64>, usize) {
    let mut found = Vec::new();
    let mut presentations = std::collections::BTreeSet::new();
    for line in log.lines() {
        let Some(client) = line.strip_prefix("C ") else {
            continue;
        };
        let mut fields = client.split(' ').map(str::parse::<u128>);
        let (
            Some(Ok(frame)),
            Some(Ok(at)),
            Some(Ok(_applied)),
            Some(Ok(_started)),
            Some(Ok(shown)),
        ) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        )
        else {
            continue;
        };
        let Ok(frame) = u64::try_from(frame) else {
            continue;
        };
        // Only this run's frames: same number and write time.
        if written.get(&frame) == Some(&at) {
            presentations.insert(shown);
            let nanos = u64::try_from(shown.saturating_sub(at)).unwrap_or(u64::MAX);
            found.push(Duration::from_nanos(nanos).as_secs_f64() * 1_000.0);
        }
    }
    found.sort_by(f64::total_cmp);
    (found, presentations.len())
}

pub(crate) fn run(args: &ReplayArgs) -> Result<String, String> {
    let frames = frames(args)?;
    let written = play(&frames, args.fps)?;
    let mut report = format!("played {} frames at {} fps\n", written.len(), args.fps);
    let Some(path) = &args.log else {
        report.push_str("no --log given: run the attach client with BMUX_REPLAY_LOG=<file> and pass the same file to report latency\n");
        return Ok(report);
    };
    let log = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let (latency, presentations) = latencies(&log, &written);
    let missing = written.len() - latency.len();
    report.push_str(&format!(
        "end to end (written here -> handed to the host terminal): p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms\n{} frames shown in {presentations} host frames; {missing} not seen\n",
        percentile(&latency, 50),
        percentile(&latency, 95),
        percentile(&latency, 99),
        latency.last().copied().unwrap_or(0.0),
        latency.len(),
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_runs_frames_are_measured() {
        let written = BTreeMap::from([(0, 1_000_000), (1, 2_000_000), (2, 3_000_000)]);
        // Frame 1 from another run (other write time), a torn line, and two frames shown together.
        let log = "C 0 1000000 2000000 2000000 3000000\nC 1 999 0 0 4000000\ngarbage\n\
                   S 2 3000000 4000000\nC 2 3000000 4000000 4000000 5000000\n\
                   C 1 2000000 3000000 3000000 5000000\n";
        let (latency, presentations) = latencies(log, &written);
        assert_eq!(latency, [2.0, 2.0, 3.0]);
        assert_eq!(presentations, 2);
        assert!((percentile(&latency, 95) - 3.0).abs() < f64::EPSILON);
    }
}
