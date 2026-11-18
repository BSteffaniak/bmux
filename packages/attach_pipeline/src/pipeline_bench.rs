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
    render_attach_scene_with_stats_and_trace_with_capabilities_and_occluders,
};
use crate::types::{PaneRenderBuffer, PaneScrollbackViews};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
};
use bmux_attach_pipeline_models::AttachOutputChunkMeta;
use bmux_bench::corpus::{self, Frame, SYNC_BEGIN, SYNC_END};
use bmux_bench::metrics::{Cost, PhaseRecorder, Probe, Report, Scenario};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

// Allocations are counted in this test binary: the metrics below report them per frame.
#[global_allocator]
static ALLOCATOR: bmux_bench::allocations::CountingAllocator =
    bmux_bench::allocations::CountingAllocator;

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
    margins: bool,
    /// Presents the frames the renderer paints, as the attach client does.
    presenter: crate::frame_presenter::FramePresenter,
    /// Also prepares and publishes each frame as an observed client does.
    observer: Option<bmux_tui::presentation::PresentationState>,
    /// Published frames an observer still holds, every other one kept (its cells cannot then be
    /// reused for the next frame).
    held: Vec<std::sync::Arc<bmux_tui::presentation::PreparedPresentation>>,
    hold: bool,
    /// Panes the frames' output is applied to (every pane of the scene unless set otherwise).
    fed: Vec<Uuid>,
    /// Images every frame contributes, as the attach client contributes pane images.
    images: Vec<bmux_tui::image::ImagePlacement>,
    /// Columns the images move right each frame (cycling over eight steps); zero keeps them.
    image_motion: u16,
    appearance: bmux_appearance::RuntimeAppearance,
    graphics: crate::TerminalGraphicsCache,
    scrollback: PaneScrollbackViews,
    probe: Probe,
}

struct Rendered {
    apply: Duration,
    render: Duration,
    stats: AttachSceneRenderStats,
    /// What applying the frame's output cost.
    apply_cost: Cost,
    /// What rendering and presenting the frame cost.
    render_cost: Cost,
    /// What preparing and publishing the frame for an observer cost.
    observe_cost: Option<Cost>,
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
            margins: false,
            presenter: crate::frame_presenter::FramePresenter::new(),
            observer: None,
            held: Vec::new(),
            hold: false,
            fed: vec![pane_id],
            images: Vec::new(),
            image_motion: 0,
            appearance: bmux_appearance::RuntimeAppearance::default(),
            graphics: crate::TerminalGraphicsCache::new(),
            scrollback: PaneScrollbackViews::new(),
            probe: Probe::new(),
        }
    }

    /// `panes` panes side by side across `size`, one column apart, every one fed the frames.
    fn split(size: (u16, u16), panes: u16, chunk_size: usize) -> Self {
        let mut replay = Self::new(size, true, chunk_size);
        let width = split_pane_width(size.0, panes);
        replay.scene.surfaces.clear();
        replay.buffers.clear();
        replay.fed.clear();
        for index in 0..panes {
            let pane_id = Uuid::from_u128(2 + u128::from(index));
            let rect = AttachRect {
                x: index * (width + 1),
                y: 0,
                w: width,
                h: size.1,
            };
            let mut surface = scene(pane_id, size).surfaces.remove(0);
            surface.rect = rect;
            surface.content_rect = rect;
            surface.cursor_owner = index == 0;
            replay.scene.surfaces.push(surface);
            let mut buffer = PaneRenderBuffer::default();
            buffer
                .terminal_grid
                .resize(width, size.1)
                .expect("grid size");
            replay.buffers.insert(pane_id, buffer);
            replay.fed.push(pane_id);
        }
        replay.pane_id = replay.fed[0];
        replay
    }

    /// A pane inside a one-cell border, on a host with left/right margins: frames are written for
    /// `size`, the pane's content is two cells smaller each way.
    fn bordered(size: (u16, u16), chunk_size: usize) -> Self {
        let mut replay = Self::new(size, true, chunk_size);
        let content = AttachRect {
            x: 1,
            y: 1,
            w: size.0,
            h: size.1,
        };
        replay.scene.surfaces[0].rect = AttachRect {
            x: 0,
            y: 0,
            w: size.0 + 2,
            h: size.1 + 2,
        };
        replay.scene.surfaces[0].content_rect = content;
        replay.size = (size.0 + 2, size.1 + 2);
        replay.margins = true;
        replay
    }

    /// Apply `frame` and render, writing what the host terminal receives to `output`.
    fn frame(&mut self, frame: &[u8], output: &mut Vec<u8>) -> Rendered {
        let started = Instant::now();
        let mark = self.probe.mark();
        self.apply_only(frame);
        let apply_cost = self.probe.since(mark);
        let applied = started.elapsed();
        let full = self.frames == 0;
        let damage = self.damage(full);
        let step = self.image_step();
        self.frames += 1;
        let started = Instant::now();
        let mark = self.probe.mark();
        let capabilities = bmux_plugin::TerminalRenderCapabilities {
            left_right_margins: self.margins,
            ..bmux_plugin::TerminalRenderCapabilities::default()
        };
        let spec = crate::frame_presenter::FrameSpec {
            full,
            size: self.size,
            margins: self.margins,
            host: false,
        };
        let (scene, policy, size, buffers) =
            (&self.scene, self.policy, self.size, &mut self.buffers);
        let (graphics, scrollback, appearance, images) = (
            &mut self.graphics,
            &self.scrollback,
            &self.appearance,
            &self.images,
        );
        let presented = self
            .presenter
            .present(&spec, |canvas| {
                let rendered =
                    render_attach_scene_with_stats_and_trace_with_capabilities_and_occluders(
                        canvas,
                        scene,
                        &[],
                        buffers,
                        graphics,
                        &damage,
                        0,
                        0,
                        scrollback,
                        false,
                        size,
                        appearance,
                        policy,
                        &[],
                        capabilities,
                        &[],
                        None,
                    );
                if !images.is_empty() {
                    canvas.declare_images();
                    for image in images {
                        let mut image = image.clone();
                        image.destination.x += step;
                        image.clip.x += step;
                        canvas.image(image);
                    }
                }
                rendered
            })
            .expect("render");
        match presented.presented {
            crate::frame_presenter::Presented::Frame(bytes) => {
                output.extend_from_slice(&bytes);
                // Written, as the attach client writes a frame, and given back.
                self.presenter.recycle(bytes);
            }
            crate::frame_presenter::Presented::Unchanged => {}
            crate::frame_presenter::Presented::Unsupported(reason) => {
                panic!("the frame should be presentable: {reason}")
            }
        }
        let render_cost = self.probe.since(mark);
        let (_, mut stats) = presented.result;
        stats.pane_rows_scrolled = self
            .presenter
            .applied_scrolls()
            .iter()
            .map(|scroll| {
                u64::from(scroll.region.height).saturating_sub(scroll.rows.unsigned_abs())
            })
            .sum();
        let mark = self.probe.mark();
        let observed = self.observe();
        let observe_cost = observed.then(|| self.probe.since(mark));
        Rendered {
            apply: applied,
            render: started.elapsed(),
            stats,
            apply_cost,
            render_cost,
            observe_cost,
        }
    }

    /// The damage of a frame: all of it for the first, the fed panes' content after.
    fn damage(&self, full: bool) -> FrameDamage {
        let mut damage = FrameDamage::default();
        if full {
            damage.mark_full_frame();
        } else {
            for pane in &self.fed {
                damage.mark_content_surface(*pane);
            }
        }
        damage
    }

    /// Columns the images move this frame.
    fn image_step(&self) -> u16 {
        u16::try_from(self.frames % 8).unwrap_or(0) * self.image_motion
    }

    /// Prepare and publish the frame just presented as an observed client's observer does, when
    /// this replay has one: repaired over the last frame with the change the host's terminal
    /// made, with its local grids installed (each staging rechecks the frame's limits).
    fn observe(&mut self) -> bool {
        let size = self.size;
        let Some(observer) = &mut self.observer else {
            return false;
        };
        observer.resize(bmux_tui::geometry::Rect::new(0, 0, size.0, size.1));
        let limits = bmux_tui::presentation::PresentationLimits {
            max_cells: 262_144,
            max_entries: 262_144,
            max_bytes: 32 * 1024 * 1024,
        };
        // The host's change, copied from its terminal.
        let shown = self.presenter.shown_changes();
        let mut prepared = observer
            .prepare_repaired_bounded(limits, |cx| {
                assert!(
                    crate::frame_presenter::paint_shown_changes(cx, &shown),
                    "the frame observed was presented"
                );
            })
            .expect("prepared");
        prepared
            .set_grid_fragments(Vec::new(), limits)
            .expect("grids");
        let published = observer
            .present_prepared(Some(prepared), |frame, _| {
                Ok::<_, ()>(std::hint::black_box(std::sync::Arc::new(frame.clone())))
            })
            .expect("published");
        if self.hold && self.frames.is_multiple_of(2) {
            self.held.extend(published);
            if self.held.len() > 2 {
                self.held.remove(0);
            }
        }
        true
    }

    /// Apply `frame` to every fed pane without rendering it.
    fn apply_only(&mut self, frame: &[u8]) {
        for chunk in frame.chunks(self.chunk_size) {
            self.open = sync_after(self.open, chunk);
            let end = self.offset + chunk.len() as u64;
            for pane in &self.fed {
                apply_attach_output_chunk_with(
                    &mut self.buffers,
                    *pane,
                    chunk,
                    AttachOutputChunkMeta {
                        stream_start: self.offset,
                        stream_end: end,
                        stream_gap: false,
                        sync_update_active: self.open,
                    },
                    append_pane_output,
                );
            }
            self.offset = end;
        }
    }

    fn pane(&self) -> &bmux_terminal_grid::TerminalGrid {
        self.buffers[&self.pane_id].terminal_grid.grid()
    }
}

fn run(name: &str, size: (u16, u16), frames: &[Frame], scroll_passthrough: bool) {
    run_replay(
        name,
        Replay::new(size, scroll_passthrough, delivery()),
        frames,
    );
}

fn run_replay(name: &str, mut replay: Replay, frames: &[Frame]) {
    let size = replay.size;
    let scroll_passthrough = replay.policy.scroll_passthrough;
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
        "{name} {}x{} ({frames} frames, {} B chunks, {}): apply avg {} p95 {} | render avg {} p95 {} | in {} B/frame, out {} B/frame, x{:.2} | rows scrolled {} | deferred {}",
        size.0,
        size.1,
        replay.chunk_size,
        if scroll_passthrough {
            "passthrough"
        } else {
            "no passthrough"
        },
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

/// Replay `frames` into a host terminal emulator, presenting what the renderer paints, and
/// check, after every frame that is drawn, that the host shows exactly the pane. Returns the rows
/// moved on the host.
fn assert_host_matches(
    name: &str,
    size: (u16, u16),
    frames: &[Frame],
    scroll_passthrough: bool,
    chunk_size: usize,
) -> u64 {
    assert_replay_matches(
        name,
        Replay::new(size, scroll_passthrough, chunk_size),
        frames,
    )
}

/// [`assert_host_matches`] for a prepared replay.
fn assert_replay_matches(name: &str, mut replay: Replay, frames: &[Frame]) -> u64 {
    let size = replay.size;
    let scroll_passthrough = replay.policy.scroll_passthrough;
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
                    "{name} (passthrough {scroll_passthrough}) frame {index}: host differs \
                     from the pane at row {row} column {col}",
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
            let scrolls = matches!(
                corpus.name,
                "su-scroll" | "region-linefeed" | "plain-stream"
            );
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

#[test]
fn a_presenter_shows_the_pane_after_every_frame() {
    let size = (60, 16);
    for corpus in corpus::generated(size, 24) {
        let scrolled =
            assert_replay_matches(corpus.name, Replay::new(size, true, 97), &corpus.frames);
        let scrolls = matches!(
            corpus.name,
            "su-scroll" | "region-linefeed" | "plain-stream"
        );
        assert_eq!(
            scrolled > 0,
            scrolls,
            "{}: rows moved on the host: {scrolled}",
            corpus.name
        );
    }
    assert_replay_matches(
        "scattered-edits",
        Replay::new(size, true, 97),
        &scattered_edits(size, 200),
    );
}

/// An observer paints the frames the host terminal presents into its own published frames: after
/// every frame both hold the same cells, whether or not the observer still holds earlier frames.
#[test]
fn an_observer_shows_the_cells_the_host_terminal_does() {
    let size = (60, 16);
    let corpora = corpus::generated(size, 24)
        .into_iter()
        .map(|corpus| (corpus.name, corpus.frames))
        .chain([("scattered-edits", scattered_edits(size, 200))]);
    for (name, frames) in corpora {
        for hold in [false, true] {
            let mut replay = Replay::new(size, true, 97);
            replay.observer = Some(bmux_tui::presentation::PresentationState::new(
                bmux_tui::geometry::Rect::new(0, 0, size.0, size.1),
            ));
            replay.hold = hold;
            let mut output = Vec::new();
            for (index, frame) in frames.iter().enumerate() {
                output.clear();
                replay.frame(frame, &mut output);
                let observed = replay.observer.as_ref().and_then(|o| o.retained_buffer());
                assert_eq!(
                    observed,
                    replay.presenter.shown_cells(),
                    "{name} (holding frames {hold}) frame {index}: the observer's cells differ \
                     from the host's"
                );
            }
        }
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
fn a_bordered_pane_scrolls_inside_margins_and_writes_about_what_the_application_did() {
    let size = (60, 16);
    let mut replay = Replay::bordered(size, 8192);
    let frames = corpus::su_scroll(size, 20);
    let mut output = Vec::new();
    let (mut bytes_in, mut bytes_out, mut scrolled) = (0, 0, 0);
    for (index, frame) in frames.iter().enumerate() {
        output.clear();
        let rendered = replay.frame(frame, &mut output);
        if index > 0 {
            bytes_in += frame.len();
            bytes_out += output.len();
            // Every frame moves the pane's rows instead of repainting them: all 14 but the row
            // scrolled out and the new bottom row, which is painted anyway.
            assert!(
                rendered.stats.pane_rows_scrolled >= 12,
                "frame {index} moved {} rows",
                rendered.stats.pane_rows_scrolled
            );
            scrolled += rendered.stats.pane_rows_scrolled;
        }
    }
    assert!(scrolled >= 20 * 12, "{scrolled} rows moved");
    assert!(
        bytes_out < bytes_in * 3 / 2,
        "{bytes_out} bytes out for {bytes_in} in"
    );
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn pipeline_bench() {
    let size = size();
    for corpus in corpus::generated(size, 300) {
        run(corpus.name, corpus.size, &corpus.frames, false);
        run(corpus.name, corpus.size, &corpus.frames, true);
        run_replay(
            corpus.name,
            Replay::new(corpus.size, true, delivery()),
            &corpus.frames,
        );
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
            run_replay(name, Replay::new(size, true, delivery()), &frames);
        }
    }
}

/// A fixed amount of work for counting instructions, which machine load does not change (wall
/// time on a busy machine varies more than the differences being measured):
///
/// ```text
/// BMUX_BENCH_CORPUS=frames BMUX_BENCH_MODE=terminal /usr/bin/time -l <test binary> \
///     --ignored --exact pipeline_bench::pipeline_bench_loop
/// ```
///
/// A corpus named `generated:NAME` is that generated corpus. `BMUX_BENCH_MODE` is `terminal`
/// (rendered and presented, the default), `observed` (presented and also prepared and published
/// as an observed client does) or `apply` (applying only, to subtract). The first
/// corpus in `BMUX_BENCH_CORPUS` is played `BMUX_BENCH_ROUNDS` times (5 by default).
#[test]
#[ignore = "instruction-count workload; run explicitly"]
fn pipeline_bench_loop() {
    let paths = std::env::var("BMUX_BENCH_CORPUS").expect("BMUX_BENCH_CORPUS");
    let path = paths.split(',').next().expect("a corpus");
    let frames = path.strip_prefix("generated:").map_or_else(
        || corpus::read(std::path::Path::new(path)).expect("corpus"),
        |name| {
            corpus::generated(size(), 300)
                .into_iter()
                .find(|corpus| corpus.name == name)
                .expect("generated corpus")
                .frames
        },
    );
    let mode = std::env::var("BMUX_BENCH_MODE").unwrap_or_else(|_| "terminal".to_owned());
    let rounds =
        std::env::var("BMUX_BENCH_ROUNDS").map_or(5, |rounds| rounds.parse().expect("rounds"));
    for _ in 0..rounds {
        let mut replay = Replay::new(size(), true, delivery());
        if mode == "observed" {
            replay.observer = Some(bmux_tui::presentation::PresentationState::new(
                bmux_tui::geometry::Rect::new(0, 0, replay.size.0, replay.size.1),
            ));
        }
        let mut output = Vec::new();
        for frame in &frames {
            output.clear();
            if mode == "apply" {
                replay.apply_only(frame);
            } else {
                replay.frame(frame, &mut output);
            }
        }
    }
}

/// The width of each of `panes` panes side by side across `cols`, one column apart.
fn split_pane_width(cols: u16, panes: u16) -> u16 {
    cols.saturating_sub(panes.saturating_sub(1)) / panes.max(1)
}

/// Frames before a scenario's metrics are taken: buffers grow and caches fill while the
/// replay warms up, which steady frames do not repeat.
const WARMUP: usize = 30;

/// Replay `frames` and measure every frame after warm-up: what applying, rendering and
/// presenting (and observing, with an observer) allocate and execute, and the bytes written.
fn measure(name: &str, mut replay: Replay, frames: &[Frame]) -> Scenario {
    let (mut apply, mut render, mut observe) = (
        PhaseRecorder::default(),
        PhaseRecorder::default(),
        PhaseRecorder::default(),
    );
    let mut output = Vec::new();
    let (mut bytes_out, mut measured, mut observed) = (0_usize, 0_u64, false);
    let mut live_after_warmup = 0;
    for (index, frame) in frames.iter().enumerate() {
        if index == WARMUP {
            live_after_warmup = bmux_bench::allocations::live_bytes();
        }
        output.clear();
        let rendered = replay.frame(frame, &mut output);
        if index < WARMUP {
            continue;
        }
        measured += 1;
        bytes_out += output.len();
        apply.record(rendered.apply_cost);
        render.record(rendered.render_cost);
        if let Some(cost) = rendered.observe_cost {
            observe.record(cost);
            observed = true;
        }
    }
    let live_growth = i64::try_from(bmux_bench::allocations::live_bytes()).unwrap_or(i64::MAX)
        - i64::try_from(live_after_warmup).unwrap_or(i64::MAX);
    let mut phases = BTreeMap::from([
        ("apply".to_owned(), apply.finish()),
        ("render".to_owned(), render.finish()),
    ]);
    if observed {
        phases.insert("observe".to_owned(), observe.finish());
    }
    Scenario {
        name: name.to_owned(),
        frames: measured,
        phases,
        output_bytes_per_frame: u64::try_from(bytes_out).unwrap_or(u64::MAX) / measured.max(1),
        live_growth_bytes: live_growth,
    }
}

/// A replay whose host shows images (Kitty or sixel), with one 40×20-cell image over the right
/// pane of two while the left one is fed the frames; with `motion`, the image moves each frame.
#[cfg(all(feature = "image-kitty", feature = "image-sixel"))]
fn image_replay(size: (u16, u16), kitty: bool, motion: u16) -> Replay {
    use bmux_tui::image::{
        ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };
    let mut replay = Replay::split(size, 2, delivery());
    replay.fed.truncate(1);
    replay.presenter.set_image_host(
        bmux_image::host_caps::HostImageCapabilities {
            kitty_graphics: kitty,
            sixel: !kitty,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..bmux_image::host_caps::HostImageCapabilities::default()
        },
        bmux_image::ImageConfig::default(),
    );
    let right = replay.scene.surfaces[1].content_rect;
    let destination = bmux_tui::geometry::Rect::new(right.x + 4, 4, 40, 20);
    // A gradient, so encoders do real work.
    let (width, height) = (320_u32, 320_u32);
    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|index| {
            let (x, y) = (index % width, index / width);
            [
                u8::try_from(x % 256).unwrap_or(0),
                u8::try_from(y % 256).unwrap_or(0),
                u8::try_from((x + y) % 256).unwrap_or(0),
                255,
            ]
        })
        .collect();
    replay.images.push(ImagePlacement {
        fit: ImageFit::Stretch,
        allocation: None,
        key: ImageKey::new("pane:bench:picture"),
        payload: ImagePayload::Pixels {
            bytes: pixels.into(),
            width,
            height,
            format: ImagePixelFormat::Rgba8,
        },
        destination,
        clip: bmux_tui::geometry::Rect::new(right.x, right.y, right.w, right.h),
        lifecycle: ImageLifecycle::Frame,
        z: 0,
    });
    replay.image_motion = motion;
    replay
}

/// The scenarios the metrics cover: every generated corpus on one pane; the busiest ones with
/// an observer, without scroll passthrough, and across two panes; scattered edits; and (with
/// image features) an image beside streaming output and a moving image, on Kitty and sixel.
fn metrics_report() -> Report {
    let size = size();
    let frames = 300;
    let mut report = Report::default();
    let busiest = |name: &str| matches!(name, "plain-stream" | "full-repaint");
    for corpus in corpus::generated(size, frames) {
        report.scenarios.push(measure(
            corpus.name,
            Replay::new(size, true, delivery()),
            &corpus.frames,
        ));
        if !busiest(corpus.name) {
            continue;
        }
        report.scenarios.push(measure(
            &format!("{}/no-passthrough", corpus.name),
            Replay::new(size, false, delivery()),
            &corpus.frames,
        ));
        let mut observed = Replay::new(size, true, delivery());
        observed.observer = Some(bmux_tui::presentation::PresentationState::new(
            bmux_tui::geometry::Rect::new(0, 0, size.0, size.1),
        ));
        report.scenarios.push(measure(
            &format!("{}/observed", corpus.name),
            observed,
            &corpus.frames,
        ));
    }
    let pane = (split_pane_width(size.0, 2), size.1);
    for corpus in corpus::generated(pane, frames) {
        if busiest(corpus.name) {
            report.scenarios.push(measure(
                &format!("{}/two-panes", corpus.name),
                Replay::split(size, 2, delivery()),
                &corpus.frames,
            ));
        }
    }
    report.scenarios.push(measure(
        "scattered-edits",
        Replay::new(size, true, delivery()),
        &scattered_edits(size, frames),
    ));
    #[cfg(all(feature = "image-kitty", feature = "image-sixel"))]
    {
        let stream = corpus::generated(pane, frames)
            .into_iter()
            .find(|corpus| corpus.name == "plain-stream")
            .expect("plain-stream corpus")
            .frames;
        for (protocol, kitty) in [("kitty", true), ("sixel", false)] {
            report.scenarios.push(measure(
                &format!("image/{protocol}/beside-stream"),
                image_replay(size, kitty, 0),
                &stream,
            ));
            report.scenarios.push(measure(
                &format!("image/{protocol}/moving"),
                image_replay(size, kitty, 1),
                &stream,
            ));
        }
    }
    report
}

fn print_scenario(scenario: &Scenario) {
    let phase = |name: &str| {
        scenario.phases.get(name).map_or_else(String::new, |phase| {
            let instructions = phase
                .instructions_per_frame
                .map_or_else(String::new, |count| format!(", {count} instr"));
            format!(
                " | {name} {} allocs (max {}), {} B{instructions}",
                phase.allocations_per_frame, phase.max_allocations, phase.allocated_bytes_per_frame
            )
        })
    };
    println!(
        "{} ({} frames){}{}{} | out {} B | live growth {} B",
        scenario.name,
        scenario.frames,
        phase("apply"),
        phase("render"),
        phase("observe"),
        scenario.output_bytes_per_frame,
        scenario.live_growth_bytes
    );
}

/// Reproducible metrics of the client pipeline, per frame: allocations and allocated bytes of
/// each phase, bytes written to the host, instructions retired where there is a counter. Run
/// single-threaded in release:
///
/// ```text
/// cargo test --release -p bmux_attach_pipeline --features image-kitty,image-sixel --lib \
///     pipeline_bench::pipeline_metrics -- --ignored --exact --nocapture --test-threads 1
/// ```
///
/// `BMUX_BENCH_JSON=<path>` writes the report; `BMUX_BENCH_BASELINE=<path>` fails on any
/// allocation or output growth past that report (`docs/perf-baselines/render/pipeline.json` is
/// the committed one; see `docs/development/render-performance.md`).
#[test]
#[ignore = "reproducible metrics; run explicitly with --ignored --nocapture"]
fn pipeline_metrics() {
    let report = metrics_report();
    for scenario in &report.scenarios {
        print_scenario(scenario);
    }
    report.write_and_check();
}

/// Live bytes after each of `rounds` replays of `frames` through one replay.
fn soak(mut replay: Replay, frames: &[Frame], rounds: usize) -> Vec<usize> {
    let mut output = Vec::new();
    (0..rounds)
        .map(|_| {
            for frame in frames {
                output.clear();
                replay.frame(frame, &mut output);
            }
            bmux_bench::allocations::live_bytes()
        })
        .collect()
}

/// Memory a replay keeps stops growing: replayed ten times over, a scenario's live bytes in the
/// second half of the rounds stay within 256 KiB of where the half started (history, caches and
/// retained images are bounded). Run in release:
///
/// ```text
/// cargo test --release -p bmux_attach_pipeline --features image-kitty,image-sixel --lib \
///     pipeline_bench::pipeline_soak -- --ignored --exact --nocapture --test-threads 1
/// ```
#[test]
#[ignore = "memory soak; run explicitly with --ignored --nocapture"]
fn pipeline_soak() {
    const ROUNDS: usize = 10;
    const SLACK: usize = 256 * 1024;
    let size = size();
    let pane = (split_pane_width(size.0, 2), size.1);
    let corpus_named = |size, name: &str| {
        corpus::generated(size, 300)
            .into_iter()
            .find(|corpus| corpus.name == name)
            .expect("generated corpus")
            .frames
    };
    let observed = || {
        let mut replay = Replay::new(size, true, delivery());
        replay.observer = Some(bmux_tui::presentation::PresentationState::new(
            bmux_tui::geometry::Rect::new(0, 0, size.0, size.1),
        ));
        replay.hold = true;
        replay
    };
    let scenarios: Vec<(String, Replay, Vec<Frame>)> = vec![
        (
            "plain-stream".into(),
            Replay::new(size, true, delivery()),
            corpus_named(size, "plain-stream"),
        ),
        (
            "full-repaint/observed".into(),
            observed(),
            corpus_named(size, "full-repaint"),
        ),
        (
            "plain-stream/two-panes".into(),
            Replay::split(size, 2, delivery()),
            corpus_named(pane, "plain-stream"),
        ),
    ];
    #[cfg(all(feature = "image-kitty", feature = "image-sixel"))]
    let images: Vec<(String, Replay, Vec<Frame>)> = [("kitty", true), ("sixel", false)]
        .into_iter()
        .map(|(protocol, kitty)| {
            (
                format!("image/{protocol}/moving"),
                image_replay(size, kitty, 1),
                corpus_named(pane, "plain-stream"),
            )
        })
        .collect();
    #[cfg(not(all(feature = "image-kitty", feature = "image-sixel")))]
    let images = Vec::new();
    let mut failures = Vec::new();
    for (name, replay, frames) in scenarios.into_iter().chain(images) {
        let live = soak(replay, &frames, ROUNDS);
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
