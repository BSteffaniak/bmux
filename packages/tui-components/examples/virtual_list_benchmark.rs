//! Deterministic structural benchmarks for variable-height TUI collections.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct CountingAllocator;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every operation delegates to `System` with the original layout and
// pointer; the additional atomics only observe successful allocation requests.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegates the exact allocation request to the system allocator.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOCATION_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: Delegates the exact deallocation request to the system allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: Delegates the exact reallocation request to the system allocator.
        let next = unsafe { System.realloc(pointer, layout, new_size) };
        if !next.is_null() && COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOCATION_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, LayoutCx};
use bmux_tui::composition::TextContent;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::write_ansi_frame;
use bmux_tui::{damage::Damage, damage::DamagePolicy};
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};

fn main() {
    benchmark_index_strategies();
    for count in [100usize, 1_000, 10_000] {
        benchmark_count(count);
    }
}

fn benchmark_index_strategies() {
    for count in [100usize, 1_000, 10_000, 100_000] {
        let heights = (0..count).map(|index| index % 3 + 1).collect::<Vec<_>>();

        let started = Instant::now();
        let mut prefixes = Vec::with_capacity(count.saturating_add(1));
        prefixes.push(0usize);
        for height in &heights {
            prefixes.push(
                prefixes
                    .last()
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(*height)
                    .saturating_add(1),
            );
        }
        let prefix_build = started.elapsed();
        let target = prefixes.last().copied().unwrap_or(0) / 2;
        let started = Instant::now();
        for _ in 0..10_000 {
            black_box(prefixes.partition_point(|offset| *offset <= target));
        }
        let prefix_lookup = started.elapsed();

        let started = Instant::now();
        let changed = count / 2;
        for offset in prefixes.iter_mut().skip(changed.saturating_add(1)) {
            *offset = offset.saturating_add(1);
        }
        let prefix_update = started.elapsed();

        let started = Instant::now();
        let mut logarithmic = MeasuredListIndex::new(1);
        logarithmic.sync((0..count).map(|index| (index, 0)), 80, 0, |index| {
            heights[*index]
        });
        let logarithmic_build = started.elapsed();
        let started = Instant::now();
        for _ in 0..10_000 {
            black_box(logarithmic.item_at_offset(target));
        }
        let logarithmic_lookup = started.elapsed();
        let started = Instant::now();
        logarithmic.update_height(&changed, heights[changed].saturating_add(1));
        let logarithmic_update = started.elapsed();

        println!(
            "index_items={count} prefix_build_us={} prefix_lookup_10k_us={} prefix_update_us={} logarithmic_build_us={} logarithmic_lookup_10k_us={} logarithmic_update_us={}",
            micros(prefix_build),
            micros(prefix_lookup),
            micros(prefix_update),
            micros(logarithmic_build),
            micros(logarithmic_lookup),
            micros(logarithmic_update),
        );
    }
}

fn benchmark_count(count: usize) {
    let started = Instant::now();
    let list = build_list(count, 0);
    let build = started.elapsed();
    let mut state = VirtualListState::new(1);
    let mut layout_cx = LayoutCx::new();
    let started = Instant::now();
    list.sync(80, &mut state, &mut layout_cx);
    let initial_layout = started.elapsed();
    let initial_measured = layout_cx.measured_nodes();

    let started = Instant::now();
    list.sync(80, &mut state, &mut layout_cx);
    let steady_layout = started.elapsed();
    let steady_measured = layout_cx.measured_nodes().saturating_sub(initial_measured);

    let viewport = Rect::new(0, 0, 80, 40);
    let middle = state.total_height() / 2;
    state.scroll.set_vertical_offset(middle);
    let initial_paint = paint_once(&list, &state, viewport);
    state.scroll.set_vertical_offset(middle.saturating_add(1));
    let row_scroll = paint_once(&list, &state, viewport);
    state.scroll.set_vertical_offset(middle.saturating_add(40));
    let page_scroll = paint_once(&list, &state, viewport);

    let started = Instant::now();
    let appended = build_list(count.saturating_add(1), 0);
    appended.sync(80, &mut state, &mut layout_cx);
    let append = started.elapsed();
    let append_measured = layout_cx
        .measured_nodes()
        .saturating_sub(initial_measured.saturating_add(steady_measured));

    let before_insert = layout_cx.measured_nodes();
    let started = Instant::now();
    let inserted = build_list_with_prefix(count, 0);
    inserted.sync(80, &mut state, &mut layout_cx);
    let insert = started.elapsed();
    let insert_measured = layout_cx.measured_nodes().saturating_sub(before_insert);

    let before_reorder = layout_cx.measured_nodes();
    let started = Instant::now();
    let reordered = build_reordered_list(count, 0);
    reordered.sync(80, &mut state, &mut layout_cx);
    let reorder = started.elapsed();
    let reorder_measured = layout_cx.measured_nodes().saturating_sub(before_reorder);

    let before_paint_revision = layout_cx.measured_nodes();
    let started = Instant::now();
    let paint_changed = build_list(count, 1);
    paint_changed.sync(80, &mut state, &mut layout_cx);
    let paint_revision = started.elapsed();
    let paint_revision_measured = layout_cx
        .measured_nodes()
        .saturating_sub(before_paint_revision);

    let before_resize = layout_cx.measured_nodes();
    let started = Instant::now();
    list.sync(64, &mut state, &mut layout_cx);
    let resize = started.elapsed();
    let resize_measured = layout_cx.measured_nodes().saturating_sub(before_resize);

    println!(
        "items={count} build_us={} initial_layout_us={} initial_measured={} steady_layout_us={} steady_measured={} initial_paint_us={} initial_painted={} row_scroll_us={} row_painted={} page_scroll_us={} page_painted={} allocations={} allocation_bytes={} hit_regions={} focus_targets={} semantic_regions={} selection_fragments={} image_contributions={} damage_regions={} damaged_cells={} frame_output_bytes={} append_us={} append_measured={} insert_us={} insert_measured={} reorder_us={} reorder_measured={} paint_revision_us={} paint_revision_measured={} resize_us={} resize_measured={} cache_hits={} cache_misses={} cache_released={} total_rows={}",
        micros(build),
        micros(initial_layout),
        initial_measured,
        micros(steady_layout),
        steady_measured,
        micros(initial_paint.elapsed),
        initial_paint.rendered.painted_items,
        micros(row_scroll.elapsed),
        row_scroll.rendered.painted_items,
        micros(page_scroll.elapsed),
        page_scroll.rendered.painted_items,
        row_scroll.allocations,
        row_scroll.allocation_bytes,
        row_scroll.hit_regions,
        row_scroll.focus_targets,
        row_scroll.semantic_regions,
        row_scroll.selection_fragments,
        row_scroll.image_contributions,
        row_scroll.damage_regions,
        row_scroll.damaged_cells,
        row_scroll.frame_output_bytes,
        micros(append),
        append_measured,
        micros(insert),
        insert_measured,
        micros(reorder),
        reorder_measured,
        micros(paint_revision),
        paint_revision_measured,
        micros(resize),
        resize_measured,
        state.layout_cache().stats().hits,
        state.layout_cache().stats().misses,
        state.layout_cache().stats().released,
        state.total_height(),
    );
}

struct PaintReport {
    elapsed: Duration,
    allocations: usize,
    allocation_bytes: usize,
    rendered: bmux_tui_components::virtual_list::VirtualListRenderStats,
    hit_regions: usize,
    focus_targets: usize,
    semantic_regions: usize,
    selection_fragments: usize,
    image_contributions: usize,
    damage_regions: usize,
    damaged_cells: usize,
    frame_output_bytes: usize,
}

fn paint_once<K>(
    list: &VirtualList<'_, K>,
    state: &VirtualListState<K>,
    viewport: Rect,
) -> PaintReport
where
    K: Clone + Ord + ToString,
{
    let mut buffer = Buffer::empty(viewport);
    let mut frame = Frame::new(&mut buffer);
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    ALLOCATION_BYTES.store(0, Ordering::Relaxed);
    COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
    let started = Instant::now();
    let rendered = list.paint(viewport, state, &mut PaintCx::new(&mut frame));
    let elapsed = started.elapsed();
    COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
    let allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    let allocation_bytes = ALLOCATION_BYTES.load(Ordering::Relaxed);
    let hit_regions = frame.hits().regions().len();
    let focus_targets = frame
        .hits()
        .regions()
        .iter()
        .filter(|region| region.focusable)
        .count();
    let semantic_regions = frame.semantics().regions().len();
    let selection_fragments = frame.selection().fragments().len();
    let image_contributions = frame.images().len();
    let (damage_regions, damaged_cells) = damage_counts(
        frame.damage(DamagePolicy {
            max_regions: usize::MAX,
            max_area_percent: 101,
        }),
        viewport,
    );
    let mut output = Vec::new();
    write_ansi_frame(&mut output, frame.buffer(), frame.cursor()).expect("in-memory ANSI write");
    black_box(frame.buffer());
    PaintReport {
        elapsed,
        allocations,
        allocation_bytes,
        rendered,
        hit_regions,
        focus_targets,
        semantic_regions,
        selection_fragments,
        image_contributions,
        damage_regions,
        damaged_cells,
        frame_output_bytes: output.len(),
    }
}

fn damage_counts(damage: Damage, viewport: Rect) -> (usize, usize) {
    match damage {
        Damage::None => (0, 0),
        Damage::Regions(regions) => (
            regions.len(),
            regions
                .iter()
                .map(|area| usize::from(area.width) * usize::from(area.height))
                .sum(),
        ),
        Damage::Full => (
            1,
            usize::from(viewport.width) * usize::from(viewport.height),
        ),
    }
}

fn build_list(count: usize, paint_revision: u64) -> VirtualList<'static, usize> {
    (0..count).fold(VirtualList::new("benchmark"), |list, index| {
        list.item(index, 0, benchmark_item(index, paint_revision))
    })
}

fn build_list_with_prefix(count: usize, paint_revision: u64) -> VirtualList<'static, usize> {
    (0..count).fold(
        VirtualList::new("benchmark").item(
            usize::MAX,
            0,
            benchmark_item(usize::MAX, paint_revision),
        ),
        |list, index| list.item(index, 0, benchmark_item(index, paint_revision)),
    )
}

fn build_reordered_list(count: usize, paint_revision: u64) -> VirtualList<'static, usize> {
    (0..count)
        .rev()
        .fold(VirtualList::new("benchmark"), |list, index| {
            list.item(index, 0, benchmark_item(index, paint_revision))
        })
}

fn benchmark_item(index: usize, paint_revision: u64) -> impl Component {
    let text = match index % 3 {
        0 => format!("short item {index}"),
        1 => format!("medium item {index} with enough words to exercise wrapping"),
        _ => format!(
            "long item {index} with several words that exercise variable-height exact measurement across a constrained terminal width"
        ),
    };
    TextContent::new(text).style(if paint_revision == 0 {
        bmux_tui::style::Style::new()
    } else {
        bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Blue)
    })
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}
