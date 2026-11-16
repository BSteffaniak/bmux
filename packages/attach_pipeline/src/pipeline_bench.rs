//! Client half of the pane-output pipeline benchmark (see
//! `docs/development/pane-output-performance.md`), ignored by default:
//!
//! ```text
//! cargo test --release -p bmux_attach_pipeline pipeline_bench -- --ignored --nocapture
//! ```
//!
//! Each corpus frame is delivered in chunks the way the server hands pane output to a client
//! (`BMUX_BENCH_DELIVERY` bytes each, 8192 by default), applied with [`append_pane_output`], and
//! rendered once with the attach renderer into memory. Reported per corpus: apply and render time
//! per frame, bytes the application wrote against bytes written to the host terminal (and their
//! ratio), and frames left inside an unfinished synchronized update when rendered.
//!
//! Generated corpora run at `BMUX_BENCH_SIZE` (`420x90` by default); captured corpora are listed
//! in `BMUX_BENCH_CORPUS` (comma-separated paths, at the same size).

use crate::reconcile::apply_attach_output_chunk_with;
use crate::render::{
    AttachSceneRenderStats, DamageCoalescingPolicy, FrameDamage, append_pane_output,
    render_attach_scene_with_stats,
};
use crate::types::{PaneRenderBuffer, PaneScrollbackViews};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
};
use bmux_attach_pipeline_models::AttachOutputChunkMeta;
use bmux_bench::corpus::{self, Frame, SYNC_BEGIN, SYNC_END};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn size() -> (u16, u16) {
    std::env::var("BMUX_BENCH_SIZE").map_or((420, 90), |size| {
        let (cols, rows) = size.split_once('x').expect("WIDTHxHEIGHT");
        (cols.parse().expect("width"), rows.parse().expect("height"))
    })
}

fn delivery() -> usize {
    std::env::var("BMUX_BENCH_DELIVERY").map_or(8192, |bytes| bytes.parse().expect("bytes"))
}

fn scene(pane_id: Uuid, (cols, rows): (u16, u16)) -> AttachScene {
    let rect = AttachRect {
        x: 0,
        y: 0,
        w: cols,
        h: rows,
    };
    AttachScene {
        session_id: Uuid::from_u128(1),
        focus: AttachFocusTarget::Pane { pane_id },
        surfaces: vec![AttachSurface {
            id: pane_id,
            kind: AttachSurfaceKind::Pane,
            layer: bmux_attach_layout_protocol::AttachLayer::Pane,
            z: 0,
            rect,
            content_rect: rect,
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(pane_id),
        }],
    }
}

/// Whether a synchronized update is open after `bytes`, given it was `open` before.
fn sync_after(open: bool, bytes: &[u8]) -> bool {
    let last = |marker: &[u8]| {
        bytes
            .windows(marker.len())
            .rposition(|window| window == marker)
    };
    match (last(SYNC_BEGIN), last(SYNC_END)) {
        (Some(begin), Some(end)) => begin > end,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => open,
    }
}

#[derive(Default)]
struct Costs {
    apply: Vec<Duration>,
    render: Vec<Duration>,
    bytes_in: usize,
    bytes_out: usize,
    deferred: usize,
    scrolled: usize,
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    samples
        .get((samples.len() * percent).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or_default()
}

fn ms(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
}

/// `out / in` for the report (precision beyond `u32` bytes does not matter here).
fn ratio(out: usize, input: usize) -> f64 {
    let as_f64 = |bytes: usize| f64::from(u32::try_from(bytes).unwrap_or(u32::MAX));
    as_f64(out) / as_f64(input.max(1))
}

fn average(samples: &[Duration]) -> Duration {
    let count = u32::try_from(samples.len()).unwrap_or(u32::MAX).max(1);
    samples.iter().sum::<Duration>() / count
}

/// A pane fed corpus frames as a client receives them, and rendered once per frame.
struct Replay {
    pane_id: Uuid,
    size: (u16, u16),
    scene: AttachScene,
    buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    policy: DamageCoalescingPolicy,
    chunk_size: usize,
    offset: u64,
    open: bool,
    frames: usize,
}

struct Rendered {
    apply: Duration,
    render: Duration,
    stats: AttachSceneRenderStats,
}

impl Replay {
    fn new(size: (u16, u16), scroll_passthrough: bool, chunk_size: usize) -> Self {
        let pane_id = Uuid::from_u128(2);
        let mut buffer = PaneRenderBuffer::default();
        buffer
            .terminal_grid
            .resize(size.0, size.1)
            .expect("grid size");
        Self {
            pane_id,
            size,
            scene: scene(pane_id, size),
            buffers: BTreeMap::from([(pane_id, buffer)]),
            policy: DamageCoalescingPolicy {
                scroll_passthrough,
                ..DamageCoalescingPolicy::default()
            },
            chunk_size: chunk_size.max(1),
            offset: 0,
            open: false,
            frames: 0,
        }
    }

    /// Apply `frame` and render, writing what the host terminal receives to `output`.
    fn frame(&mut self, frame: &[u8], output: &mut Vec<u8>) -> Rendered {
        let started = Instant::now();
        for chunk in frame.chunks(self.chunk_size) {
            self.open = sync_after(self.open, chunk);
            let end = self.offset + chunk.len() as u64;
            apply_attach_output_chunk_with(
                &mut self.buffers,
                self.pane_id,
                chunk,
                AttachOutputChunkMeta {
                    stream_start: self.offset,
                    stream_end: end,
                    stream_gap: false,
                    sync_update_active: self.open,
                },
                append_pane_output,
            );
            self.offset = end;
        }
        let applied = started.elapsed();
        let mut damage = FrameDamage::default();
        if self.frames == 0 {
            damage.mark_full_frame();
        } else {
            damage.mark_content_surface(self.pane_id);
        }
        self.frames += 1;
        let started = Instant::now();
        let (_, stats) = render_attach_scene_with_stats(
            output,
            &self.scene,
            &[],
            &mut self.buffers,
            &damage,
            0,
            0,
            &PaneScrollbackViews::new(),
            false,
            self.size,
            &bmux_appearance::RuntimeAppearance::default(),
            self.policy,
            &[],
        )
        .expect("render");
        Rendered {
            apply: applied,
            render: started.elapsed(),
            stats,
        }
    }

    fn pane(&self) -> &bmux_terminal_grid::TerminalGrid {
        self.buffers[&self.pane_id].terminal_grid.grid()
    }
}

fn run(name: &str, size: (u16, u16), frames: &[Frame], scroll_passthrough: bool) {
    let mut replay = Replay::new(size, scroll_passthrough, delivery());
    let mut costs = Costs::default();
    let mut output = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        output.clear();
        let rendered = replay.frame(frame, &mut output);
        // The first frame is the full repaint every corpus starts with; it is not a scroll.
        if index > 0 {
            costs.apply.push(rendered.apply);
            costs.render.push(rendered.render);
            costs.bytes_in += frame.len();
            costs.bytes_out += output.len();
            costs.deferred += usize::from(rendered.stats.pane_rows_sync_deferred > 0);
            costs.scrolled += usize::try_from(rendered.stats.pane_rows_scrolled).unwrap_or(0);
        }
    }
    let frames = costs.apply.len();
    println!(
        "{name} {}x{} ({frames} frames, {} B chunks, passthrough {}): apply avg {} p95 {} | render avg {} p95 {} | in {} B/frame, out {} B/frame, x{:.2} | rows scrolled {} | deferred {}",
        size.0,
        size.1,
        replay.chunk_size,
        if scroll_passthrough { "on" } else { "off" },
        ms(average(&costs.apply)),
        ms(percentile(&mut costs.apply, 95)),
        ms(average(&costs.render)),
        ms(percentile(&mut costs.render, 95)),
        costs.bytes_in / frames.max(1),
        costs.bytes_out / frames.max(1),
        ratio(costs.bytes_out, costs.bytes_in),
        costs.scrolled,
        costs.deferred,
    );
}

/// A cell as a terminal shows it: its text (blank when empty) and resolved style.
fn shown(
    grid: &bmux_terminal_grid::TerminalGrid,
    row: usize,
    col: usize,
) -> (String, bmux_terminal_grid::Style) {
    grid.viewport_row_ref(row)
        .and_then(|cells| cells.cells().get(col))
        .map_or_else(
            || (" ".to_string(), bmux_terminal_grid::Style::default()),
            |cell| {
                let text = if cell.text().is_empty() {
                    " "
                } else {
                    cell.text()
                };
                (text.to_string(), grid.palette().get(cell.style()))
            },
        )
}

/// Replay `frames` into a host terminal emulator and check, after every frame that is drawn, that
/// the host shows exactly the pane. Returns the rows moved on the host.
fn assert_host_matches(
    name: &str,
    size: (u16, u16),
    frames: &[Frame],
    scroll_passthrough: bool,
    chunk_size: usize,
) -> u64 {
    let mut replay = Replay::new(size, scroll_passthrough, chunk_size);
    let mut host = bmux_terminal_grid::TerminalGridStream::new(
        size.0,
        size.1,
        bmux_terminal_grid::GridLimits::default(),
    )
    .expect("host terminal");
    let mut scrolled = 0;
    let mut output = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        output.clear();
        let rendered = replay.frame(frame, &mut output);
        host.process(&output);
        scrolled += rendered.stats.pane_rows_scrolled;
        if rendered.stats.pane_rows_sync_deferred > 0 {
            continue;
        }
        for row in 0..usize::from(size.1) {
            for col in 0..usize::from(size.0) {
                assert_eq!(
                    shown(host.grid(), row, col),
                    shown(replay.pane(), row, col),
                    "{name} (passthrough {scroll_passthrough}) frame {index}: host differs from \
                     the pane at row {row} column {col}",
                );
            }
        }
    }
    scrolled
}

/// Scattered edits that change parts of rows: colored words, wide characters written over narrow
/// ones and back, erased spans, all at pseudo-random places.
fn scattered_edits((cols, rows): (u16, u16), frames: usize) -> Vec<Frame> {
    let mut seed = 0x2545_f491_u32;
    let mut next = |bound: u16| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        u16::try_from(seed % u32::from(bound.max(1))).unwrap_or(0)
    };
    let mut out = vec![b"\x1b[H\x1b[2J".to_vec()];
    for _ in 0..frames {
        let mut frame = SYNC_BEGIN.to_vec();
        for _ in 0..6 {
            let (row, col) = (next(rows) + 1, next(cols) + 1);
            let text = match next(5) {
                0 => format!("\x1b[38;5;{}mword\x1b[0m", next(255)),
                1 => "\u{4e2d}\u{6587}x".to_string(),
                2 => "ab".to_string(),
                3 => format!("\x1b[{}X", next(12) + 1),
                _ => format!("\x1b[48;5;{}m  \x1b[0m\x1b[K", next(255)),
            };
            frame.extend_from_slice(format!("\x1b[{row};{col}H{text}").as_bytes());
        }
        frame.extend_from_slice(SYNC_END);
        out.push(frame);
    }
    out
}

#[test]
fn the_host_screen_matches_the_pane_after_every_frame() {
    let size = (60, 16);
    for scroll_passthrough in [false, true] {
        for corpus in corpus::generated(size, 24) {
            // Small deliveries, so frames also arrive split across chunks.
            let scrolled =
                assert_host_matches(corpus.name, size, &corpus.frames, scroll_passthrough, 97);
            let scrolls = matches!(corpus.name, "su-scroll" | "region-linefeed" | "plain-stream");
            assert_eq!(
                scrolled > 0,
                scroll_passthrough && scrolls,
                "{}: rows moved on the host: {scrolled}",
                corpus.name
            );
        }
        assert_host_matches(
            "scattered-edits",
            size,
            &scattered_edits(size, 200),
            scroll_passthrough,
            97,
        );
    }
}

/// The same check over captured corpora (`BMUX_BENCH_CORPUS` at `BMUX_BENCH_SIZE`).
#[test]
#[ignore = "checks captured corpora; run explicitly with --ignored"]
fn the_host_screen_matches_the_pane_for_captured_corpora() {
    let size = size();
    let paths = std::env::var("BMUX_BENCH_CORPUS").unwrap_or_default();
    for path in paths.split(',').filter(|path| !path.is_empty()) {
        let frames = corpus::read(std::path::Path::new(path)).expect("corpus");
        for scroll_passthrough in [false, true] {
            assert_host_matches(path, size, &frames, scroll_passthrough, delivery());
        }
    }
}

#[test]
fn nothing_scrolls_on_the_host_under_a_surface_drawn_over_the_pane() {
    let size = (60, 16);
    let mut replay = Replay::new(size, true, 8192);
    let mut floating = replay.scene.surfaces[0].clone();
    floating.id = Uuid::from_u128(3);
    floating.kind = AttachSurfaceKind::FloatingPane;
    floating.z = 1;
    floating.pane_id = Some(Uuid::from_u128(4));
    floating.cursor_owner = false;
    floating.rect = AttachRect {
        x: 10,
        y: 4,
        w: 20,
        h: 6,
    };
    floating.content_rect = floating.rect;
    replay.scene.surfaces.push(floating);
    let mut output = Vec::new();
    for frame in corpus::su_scroll(size, 5) {
        output.clear();
        let rendered = replay.frame(&frame, &mut output);
        assert_eq!(rendered.stats.pane_rows_scrolled, 0);
    }
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn pipeline_bench() {
    let size = size();
    for corpus in corpus::generated(size, 300) {
        run(corpus.name, corpus.size, &corpus.frames, false);
        run(corpus.name, corpus.size, &corpus.frames, true);
    }
    if let Ok(paths) = std::env::var("BMUX_BENCH_CORPUS") {
        for path in paths.split(',').filter(|path| !path.is_empty()) {
            let frames = corpus::read(std::path::Path::new(path)).expect("corpus");
            let name = std::path::Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("captured");
            run(name, size, &frames, false);
            run(name, size, &frames, true);
        }
    }
}
