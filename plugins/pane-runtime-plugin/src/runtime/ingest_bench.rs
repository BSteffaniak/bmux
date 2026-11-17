//! Server half of the pane-output pipeline benchmark (see
//! `docs/development/pane-output-performance.md`), ignored by default:
//!
//! ```text
//! cargo test --release -p bmux_pane_runtime_plugin ingest_bench -- --ignored --nocapture
//! ```
//!
//! Each corpus frame is read in 8 KiB pieces, as the pane's PTY reader does, and each piece goes
//! through the stages the reader runs for it, timed separately: terminal-mode tracking, the main
//! grid parse with the push into the output buffer, and the cursor tracker's protocol replies.
//! Generated corpora run at `BMUX_BENCH_SIZE` (`420x90` by default); captured ones are listed in
//! `BMUX_BENCH_CORPUS`.

use super::{
    OutputFanoutBuffer, PaneCursorTracker, PaneTerminalModeTracker, ProtocolProfile,
    TerminalProtocolEngine, process_pane_output_with_pointer_queries, protocol_reply_for_chunk,
};
use bmux_bench::corpus::{self, Frame};
use bmux_terminal_grid::{GridLimits, TerminalGridStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bytes one PTY read delivers at most (the reader's buffer).
const READ: usize = 8192;

fn size() -> (u16, u16) {
    std::env::var("BMUX_BENCH_SIZE").map_or((420, 90), |size| {
        let (cols, rows) = size.split_once('x').expect("WIDTHxHEIGHT");
        (cols.parse().expect("width"), rows.parse().expect("height"))
    })
}

fn ms(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
}

/// Average and 95th percentile of per-frame samples.
fn summary(mut samples: Vec<Duration>) -> String {
    samples.sort_unstable();
    let count = u32::try_from(samples.len()).unwrap_or(u32::MAX).max(1);
    let average = samples.iter().sum::<Duration>() / count;
    let p95 = samples
        .get((samples.len() * 95).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or_default();
    format!("avg {} p95 {}", ms(average), ms(p95))
}

fn run(name: &str, (cols, rows): (u16, u16), frames: &[Frame]) {
    let grid =
        Mutex::new(TerminalGridStream::new(cols, rows, GridLimits::default()).expect("grid"));
    let output = Mutex::new(OutputFanoutBuffer::new(1 << 20));
    let mut modes = PaneTerminalModeTracker::default();
    let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
    let mut tracker = PaneCursorTracker::new(rows, cols);
    let (mut mode, mut parse, mut replies, mut total) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut bytes = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let (mut frame_mode, mut frame_parse, mut frame_replies) =
            (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        for chunk in frame.chunks(READ) {
            let started = Instant::now();
            modes.process(chunk);
            frame_mode += started.elapsed();
            let started = Instant::now();
            process_pane_output_with_pointer_queries(&grid, &output, chunk, ProtocolProfile::Bmux)
                .expect("parse");
            frame_parse += started.elapsed();
            let started = Instant::now();
            let _ = protocol_reply_for_chunk(&mut engine, &mut tracker, chunk);
            frame_replies += started.elapsed();
        }
        // The first frame is the full repaint every corpus starts with.
        if index > 0 {
            bytes += frame.len();
            mode.push(frame_mode);
            parse.push(frame_parse);
            replies.push(frame_replies);
            total.push(frame_mode + frame_parse + frame_replies);
        }
    }
    let frames = total.len().max(1);
    println!(
        "{name} {cols}x{rows} ({frames} frames, {} B/frame): total {} | modes {} | grid parse {} | cursor tracker {}",
        bytes / frames,
        summary(total),
        summary(mode),
        summary(parse),
        summary(replies),
    );
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn ingest_bench() {
    let size = size();
    for corpus in corpus::generated(size, 300) {
        run(corpus.name, corpus.size, &corpus.frames);
    }
    if let Ok(paths) = std::env::var("BMUX_BENCH_CORPUS") {
        for path in paths.split(',').filter(|path| !path.is_empty()) {
            let frames = corpus::read(std::path::Path::new(path)).expect("corpus");
            let name = std::path::Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("captured");
            run(name, size, &frames);
        }
    }
}
