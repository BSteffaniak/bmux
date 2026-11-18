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
    OutputFanoutBuffer, PaneCursorTracker, PaneShellMetadataParser, PaneTerminalModeTracker,
    ProtocolProfile, TerminalProtocolEngine, process_pane_output_with_pointer_queries,
    protocol_reply_for_chunk,
};
use bmux_bench::corpus::{self, Frame};
use bmux_bench::metrics::{PhaseRecorder, Probe, Report, Scenario};
use bmux_terminal_grid::{GridLimits, TerminalGridStream};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bytes one PTY read delivers at most (the reader's buffer).
const READ: usize = 8192;

// Allocations are counted in this test binary: `ingest_metrics` reports them per frame.
#[global_allocator]
static ALLOCATOR: bmux_bench::allocations::CountingAllocator =
    bmux_bench::allocations::CountingAllocator;

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

/// Frames before a scenario's metrics are taken (buffers grow while the reader warms up).
const WARMUP: usize = 30;

/// Read `frames` as the pane reader does and measure each stage per frame after warm-up: shell
/// metadata filtering, terminal-mode tracking, the grid parse with the push into the output
/// buffer, and the protocol replies (with the image registry built in, the image interceptor and
/// registry, which produce the replies, instead).
fn ingest(name: &str, (cols, rows): (u16, u16), frames: &[Frame]) -> Scenario {
    let probe = Probe::new();
    let grid =
        Mutex::new(TerminalGridStream::new(cols, rows, GridLimits::default()).expect("grid"));
    let output = Mutex::new(OutputFanoutBuffer::new(1 << 20));
    let mut metadata = PaneShellMetadataParser::default();
    let mut modes = PaneTerminalModeTracker::default();
    let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
    let mut tracker = PaneCursorTracker::new(rows, cols);
    #[cfg(feature = "image-registry")]
    let (mut interceptor, mut registry) = (
        bmux_image::ImageInterceptor::new(),
        bmux_image::ImageRegistry::default(),
    );
    let mut stages: BTreeMap<String, PhaseRecorder> = BTreeMap::new();
    let mut live_after_warmup = 0;
    let mut measured = 0_u64;
    for (index, frame) in frames.iter().enumerate() {
        if index == WARMUP {
            live_after_warmup = bmux_bench::allocations::live_bytes();
        }
        let mut costs: BTreeMap<&str, bmux_bench::metrics::Cost> = BTreeMap::new();
        let mut add = |stage: &'static str, cost: bmux_bench::metrics::Cost| {
            let total = costs.entry(stage).or_default();
            total.allocations += cost.allocations;
            total.instructions = match (total.instructions, cost.instructions) {
                (Some(a), Some(b)) => Some(a + b),
                (None, b) => b,
                (a, None) => a,
            };
        };
        for chunk in frame.chunks(READ) {
            #[cfg(feature = "image-registry")]
            let filtered = {
                let mark = probe.mark();
                let mut result = interceptor.process(chunk);
                let mut replies = Vec::new();
                super::ordered_image_output(
                    &mut tracker,
                    &mut engine,
                    &mut registry,
                    &mut result,
                    (8, 16),
                    &mut replies,
                )
                .expect("image output");
                add("images", probe.since(mark));
                result.filtered
            };
            #[cfg(feature = "image-registry")]
            let chunk = filtered.as_slice();
            let mark = probe.mark();
            let parsed = metadata.process_chunk(chunk);
            add("metadata", probe.since(mark));
            let chunk = &*parsed.filtered;
            let mark = probe.mark();
            modes.process(chunk);
            add("modes", probe.since(mark));
            let mark = probe.mark();
            process_pane_output_with_pointer_queries(&grid, &output, chunk, ProtocolProfile::Bmux)
                .expect("parse");
            add("parse", probe.since(mark));
            #[cfg(not(feature = "image-registry"))]
            {
                let mark = probe.mark();
                let _ = protocol_reply_for_chunk(&mut engine, &mut tracker, chunk);
                add("replies", probe.since(mark));
            }
        }
        if index < WARMUP {
            continue;
        }
        measured += 1;
        for (stage, cost) in costs {
            stages.entry(stage.to_owned()).or_default().record(cost);
        }
    }
    let live_growth = i64::try_from(bmux_bench::allocations::live_bytes()).unwrap_or(i64::MAX)
        - i64::try_from(live_after_warmup).unwrap_or(i64::MAX);
    Scenario {
        name: name.to_owned(),
        frames: measured,
        phases: stages
            .into_iter()
            .map(|(stage, recorder)| (stage, recorder.finish()))
            .collect(),
        output_bytes_per_frame: 0,
        live_growth_bytes: live_growth,
    }
}

/// Text streaming with a 64×64 Kitty image (raw RGBA, transmitted and shown at once) every 30
/// frames, as an image-printing program writes.
fn kitty_images((cols, rows): (u16, u16), frames: usize) -> Vec<Frame> {
    let pixels: Vec<u8> = (0..64_u32 * 64)
        .flat_map(|index| [u8::try_from(index % 256).unwrap_or(0), 64, 128, 255])
        .collect();
    let image = format!("\x1b_Ga=T,f=32,s=64,v=64,q=2;{}\x1b\\", base64(&pixels));
    corpus::plain_stream((cols, rows), frames)
        .into_iter()
        .enumerate()
        .map(|(index, mut frame)| {
            if index % 30 == 0 {
                frame.extend_from_slice(image.as_bytes());
            }
            frame
        })
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let value = group
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | u32::from(*byte) << (16 - 8 * index)
            });
        for index in 0..4 {
            if index <= group.len() {
                out.push(char::from(
                    ALPHABET[(value >> (18 - 6 * index)) as usize & 63],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Reproducible metrics of the pane reader, per frame and stage: allocations and allocated
/// bytes, instructions retired where there is a counter. Run single-threaded in release:
///
/// ```text
/// cargo test --release -p bmux_pane_runtime_plugin --features image-registry --lib \
///     ingest_bench::ingest_metrics -- --ignored --exact --nocapture --test-threads 1
/// ```
///
/// `BMUX_BENCH_JSON` and `BMUX_BENCH_BASELINE` write and check the report as the client
/// pipeline's metrics do (`docs/perf-baselines/render/ingest.json`).
#[test]
#[ignore = "reproducible metrics; run explicitly with --ignored --nocapture"]
fn ingest_metrics() {
    let size = size();
    let mut report = Report::default();
    for corpus in corpus::generated(size, 300) {
        report
            .scenarios
            .push(ingest(corpus.name, corpus.size, &corpus.frames));
    }
    report
        .scenarios
        .push(ingest("kitty-images", size, &kitty_images(size, 300)));
    for scenario in &report.scenarios {
        let stages: Vec<String> = scenario
            .phases
            .iter()
            .map(|(stage, phase)| {
                format!(
                    "{stage} {} allocs, {} B",
                    phase.allocations_per_frame, phase.allocated_bytes_per_frame
                )
            })
            .collect();
        println!(
            "{} ({} frames) | {} | live growth {} B",
            scenario.name,
            scenario.frames,
            stages.join(" | "),
            scenario.live_growth_bytes
        );
    }
    report.write_and_check();
}

/// Memory the pane reader keeps stops growing: replayed ten times over (text, and text with
/// Kitty images), its live bytes in the second half of the rounds stay within 256 KiB of where
/// the half started. Run in release:
///
/// ```text
/// cargo test --release -p bmux_pane_runtime_plugin --features image-registry --lib \
///     runtime::ingest_bench::ingest_soak -- --ignored --exact --nocapture --test-threads 1
/// ```
#[test]
#[ignore = "memory soak; run explicitly with --ignored --nocapture"]
fn ingest_soak() {
    const ROUNDS: usize = 10;
    const SLACK: usize = 256 * 1024;
    let (cols, rows) = size();
    let plain = corpus::plain_stream((cols, rows), 300);
    let images = kitty_images((cols, rows), 300);
    let mut failures = Vec::new();
    for (name, frames) in [("plain-stream", plain), ("kitty-images", images)] {
        let grid =
            Mutex::new(TerminalGridStream::new(cols, rows, GridLimits::default()).expect("grid"));
        let output = Mutex::new(OutputFanoutBuffer::new(1 << 20));
        let mut modes = PaneTerminalModeTracker::default();
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut tracker = PaneCursorTracker::new(rows, cols);
        #[cfg(feature = "image-registry")]
        let (mut interceptor, mut registry) = (
            bmux_image::ImageInterceptor::new(),
            bmux_image::ImageRegistry::default(),
        );
        let live: Vec<usize> = (0..ROUNDS)
            .map(|_| {
                for chunk in frames.iter().flat_map(|frame| frame.chunks(READ)) {
                    #[cfg(feature = "image-registry")]
                    let filtered = {
                        let mut result = interceptor.process(chunk);
                        let mut replies = Vec::new();
                        super::ordered_image_output(
                            &mut tracker,
                            &mut engine,
                            &mut registry,
                            &mut result,
                            (8, 16),
                            &mut replies,
                        )
                        .expect("image output");
                        result.filtered
                    };
                    #[cfg(feature = "image-registry")]
                    let chunk = filtered.as_slice();
                    modes.process(chunk);
                    process_pane_output_with_pointer_queries(
                        &grid,
                        &output,
                        chunk,
                        ProtocolProfile::Bmux,
                    )
                    .expect("parse");
                    #[cfg(not(feature = "image-registry"))]
                    let _ = protocol_reply_for_chunk(&mut engine, &mut tracker, chunk);
                }
                bmux_bench::allocations::live_bytes()
            })
            .collect();
        let start = live[ROUNDS / 2];
        let peak = live[ROUNDS / 2..].iter().copied().max().unwrap_or(start);
        println!(
            "{name}: live bytes per round {live:?}, second-half growth {} B",
            peak - start
        );
        if peak - start > SLACK {
            failures.push(format!("{name} kept growing: {live:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
