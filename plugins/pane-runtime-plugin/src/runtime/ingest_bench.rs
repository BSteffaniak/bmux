//! Server half of the pane-output pipeline benchmark (see
//! `docs/development/pane-output-performance.md`), ignored by default:
//!
//! ```text
//! cargo test --release -p bmux_pane_runtime_plugin ingest_bench -- --ignored --nocapture
//! ```
//!
//! Each corpus frame is read in 8 KiB pieces, as the pane's PTY reader does, and each piece goes
//! through what the reader runs for it ([`super::follow_chunk`], then mode tracking and the push
//! into the output buffer): images intercepted, shell metadata filtered, protocol queries
//! answered and the pane's grid parsed in one pass. Generated corpora run at `BMUX_BENCH_SIZE`
//! (`420x90` by default); captured ones are listed in `BMUX_BENCH_CORPUS`.

use super::{
    AttachInputModeState, AttachMouseProtocolState, OutputFanoutBuffer, PaneReader,
    PaneShellMetadataParser, ProtocolProfile, TerminalProtocolEngine, attach_input_modes,
    attach_mouse_protocol, follow_chunk,
};
use bmux_bench::corpus::{self, Frame};
use bmux_bench::metrics::{PhaseRecorder, Probe, Report, Scenario};
use bmux_terminal_grid::{GridLimits, TerminalGridStream};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Bytes one PTY read delivers at most (the reader's buffer).
const READ: usize = 8192;

/// One pane's reader: what the PTY reader keeps and the pane state it writes, minus the host.
struct Pane {
    grid: TerminalGridStream,
    output: OutputFanoutBuffer,
    reader: PaneReader,
    /// The modes the reader publishes for input routing.
    modes: (AttachMouseProtocolState, AttachInputModeState),
    engine: TerminalProtocolEngine,
    #[cfg(feature = "image-registry")]
    registry: bmux_image::ImageRegistry,
    #[cfg(feature = "image-registry")]
    interceptor: bmux_image::ImageInterceptor,
    replies: Vec<u8>,
}

impl Pane {
    fn new((cols, rows): (u16, u16)) -> Self {
        Self {
            grid: TerminalGridStream::new(cols, rows, GridLimits::default()).expect("grid"),
            output: OutputFanoutBuffer::new(1 << 20),
            reader: PaneReader::new(
                ProtocolProfile::Bmux,
                Some(PaneShellMetadataParser::default()),
            ),
            modes: Default::default(),
            engine: TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            #[cfg(feature = "image-registry")]
            registry: bmux_image::ImageRegistry::default(),
            #[cfg(feature = "image-registry")]
            interceptor: bmux_image::ImageInterceptor::new(),
            replies: Vec::new(),
        }
    }

    /// One PTY read, as the pane reader handles it: followed through the grid, modes tracked, the
    /// visible bytes pushed for clients.
    fn read(&mut self, chunk: &[u8]) {
        self.replies.clear();
        #[cfg(feature = "image-registry")]
        let mut images = super::ChunkImages {
            registry: &mut self.registry,
            interceptor: &mut self.interceptor,
            cell_pixels: (8, 16),
            events: Vec::new(),
        };
        #[cfg(feature = "image-registry")]
        follow_chunk(
            &mut self.reader,
            &mut self.grid,
            &mut self.engine,
            &mut images,
            chunk,
            &mut self.replies,
        )
        .expect("follow the chunk");
        #[cfg(not(feature = "image-registry"))]
        follow_chunk(
            &mut self.reader,
            &mut self.grid,
            &mut self.engine,
            chunk,
            &mut self.replies,
        );
        let modes = self.grid.grid().protocol_state();
        self.modes = (attach_mouse_protocol(modes), attach_input_modes(modes));
        self.output.push_chunk(&self.reader.visible);
        self.replies.extend_from_slice(&self.reader.pointer_replies);
    }
}

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
    let mut reader = Pane::new((cols, rows));
    let mut total = Vec::new();
    let mut bytes = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let started = Instant::now();
        for chunk in frame.chunks(READ) {
            reader.read(chunk);
        }
        // The first frame is the full repaint every corpus starts with.
        if index > 0 {
            bytes += frame.len();
            total.push(started.elapsed());
        }
    }
    let frames = total.len().max(1);
    println!(
        "{name} {cols}x{rows} ({frames} frames, {} B/frame): read {}",
        bytes / frames,
        summary(total),
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

/// Read `frames` as the pane reader does and measure each frame's reading after warm-up.
fn ingest_once(size: (u16, u16), frames: &[Frame]) -> (BTreeMap<String, PhaseRecorder>, u64, i64) {
    let probe = Probe::new();
    let mut reader = Pane::new(size);
    let mut read = PhaseRecorder::default();
    let mut live_after_warmup = 0;
    let mut measured = 0_u64;
    for (index, frame) in frames.iter().enumerate() {
        if index == WARMUP {
            live_after_warmup = bmux_bench::allocations::live_bytes();
        }
        let mark = probe.mark();
        for chunk in frame.chunks(READ) {
            reader.read(chunk);
        }
        let cost = probe.since(mark);
        if index >= WARMUP {
            measured += 1;
            read.record(cost);
        }
    }
    let live_growth = i64::try_from(bmux_bench::allocations::live_bytes()).unwrap_or(i64::MAX)
        - i64::try_from(live_after_warmup).unwrap_or(i64::MAX);
    (
        BTreeMap::from([("read".to_owned(), read)]),
        measured,
        live_growth,
    )
}

/// Measure scenario `name` (when selected, see [`bmux_bench::metrics::selected`]): read through a
/// fresh reader [`bmux_bench::metrics::repeats`] times, which must allocate identically, each
/// frame keeping its fewest instructions.
fn ingest(name: &str, size: (u16, u16), frames: &[Frame]) -> Option<Scenario> {
    if !bmux_bench::metrics::selected(name) {
        return None;
    }
    let (mut stages, measured, live_growth) = ingest_once(size, frames);
    for _ in 1..bmux_bench::metrics::repeats() {
        let (repeat, _, _) = ingest_once(size, frames);
        for (stage, recorder) in &mut stages {
            let other = repeat
                .get(stage)
                .unwrap_or_else(|| panic!("{name}: stage {stage} missing from a repeat"));
            recorder
                .fold_repeat(other)
                .unwrap_or_else(|error| panic!("{name} {stage}: {error}"));
        }
    }
    Some(Scenario {
        name: name.to_owned(),
        frames: measured,
        phases: stages
            .into_iter()
            .map(|(stage, recorder)| (stage, recorder.finish()))
            .collect(),
        output_bytes_per_frame: 0,
        live_growth_bytes: live_growth,
    })
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
            .extend(ingest(corpus.name, corpus.size, &corpus.frames));
    }
    report
        .scenarios
        .extend(ingest("kitty-images", size, &kitty_images(size, 300)));
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

/// A fixed workload for external profilers, as `pipeline_bench_loop` is for the client: the
/// corpus `BMUX_BENCH_CORPUS` (a captured file, or `generated:NAME`, or `generated:kitty-images`)
/// read `BMUX_BENCH_ROUNDS` times (5 by default) as the pane reader does
/// (`scripts/perf-render.py profile ingest`).
#[test]
#[ignore = "instruction-count workload; run explicitly"]
fn ingest_bench_loop() {
    let size = size();
    let path = std::env::var("BMUX_BENCH_CORPUS").expect("BMUX_BENCH_CORPUS");
    let frames = match path.strip_prefix("generated:") {
        Some("kitty-images") => kitty_images(size, 300),
        Some(name) => {
            corpus::generated(size, 300)
                .into_iter()
                .find(|corpus| corpus.name == name)
                .expect("generated corpus")
                .frames
        }
        None => corpus::read(std::path::Path::new(&path)).expect("corpus"),
    };
    let rounds =
        std::env::var("BMUX_BENCH_ROUNDS").map_or(5, |rounds| rounds.parse().expect("rounds"));
    for _ in 0..rounds {
        let mut reader = Pane::new(size);
        for chunk in frames.iter().flat_map(|frame| frame.chunks(READ)) {
            reader.read(chunk);
        }
    }
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
    let size = size();
    let plain = corpus::plain_stream(size, 300);
    let images = kitty_images(size, 300);
    let mut failures = Vec::new();
    for (name, frames) in [("plain-stream", plain), ("kitty-images", images)] {
        let mut reader = Pane::new(size);
        let live: Vec<usize> = (0..ROUNDS)
            .map(|_| {
                for chunk in frames.iter().flat_map(|frame| frame.chunks(READ)) {
                    reader.read(chunk);
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
