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
    DamageCoalescingPolicy, FrameDamage, append_pane_output, render_attach_scene_with_stats,
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
    let last = |marker: &[u8]| bytes.windows(marker.len()).rposition(|window| window == marker);
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

fn run(name: &str, size: (u16, u16), frames: &[Frame]) {
    let pane_id = Uuid::from_u128(2);
    let scene = scene(pane_id, size);
    let mut buffers = BTreeMap::new();
    let mut buffer = PaneRenderBuffer::default();
    buffer
        .terminal_grid
        .resize(size.0, size.1)
        .expect("grid size");
    buffers.insert(pane_id, buffer);
    let chunk_size = delivery();
    let mut offset = 0u64;
    let mut open = false;
    let mut costs = Costs::default();
    let mut output = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let started = Instant::now();
        for chunk in frame.chunks(chunk_size.max(1)) {
            open = sync_after(open, chunk);
            let end = offset + chunk.len() as u64;
            apply_attach_output_chunk_with(
                &mut buffers,
                pane_id,
                chunk,
                AttachOutputChunkMeta {
                    stream_start: offset,
                    stream_end: end,
                    stream_gap: false,
                    sync_update_active: open,
                },
                append_pane_output,
            );
            offset = end;
        }
        let applied = started.elapsed();
        let mut damage = FrameDamage::default();
        if index == 0 {
            damage.mark_full_frame();
        } else {
            damage.mark_content_surface(pane_id);
        }
        output.clear();
        let started = Instant::now();
        let (_, stats) = render_attach_scene_with_stats(
            &mut output,
            &scene,
            &[],
            &mut buffers,
            &damage,
            0,
            0,
            &PaneScrollbackViews::new(),
            false,
            size,
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )
        .expect("render");
        let rendered = started.elapsed();
        // The first frame is the full repaint every corpus starts with; it is not a scroll.
        if index > 0 {
            costs.apply.push(applied);
            costs.render.push(rendered);
            costs.bytes_in += frame.len();
            costs.bytes_out += output.len();
            costs.deferred += usize::from(stats.pane_rows_sync_deferred > 0);
        }
    }
    let frames = costs.apply.len();
    println!(
        "{name} {}x{} ({frames} frames, {chunk_size} B chunks): apply avg {} p95 {} | render avg {} p95 {} | in {} B/frame, out {} B/frame, x{:.2} | deferred {}",
        size.0,
        size.1,
        ms(average(&costs.apply)),
        ms(percentile(&mut costs.apply, 95)),
        ms(average(&costs.render)),
        ms(percentile(&mut costs.render, 95)),
        costs.bytes_in / frames.max(1),
        costs.bytes_out / frames.max(1),
        ratio(costs.bytes_out, costs.bytes_in),
        costs.deferred,
    );
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn pipeline_bench() {
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
