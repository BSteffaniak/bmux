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

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Preserve the system allocator's zero-initialization contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
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
use bmux_tui::composition::{Column, Surface, TextBlock};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::write_ansi_frame;
use bmux_tui::{damage::Damage, damage::DamagePolicy};
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};

fn main() {
    benchmark_empty();
    benchmark_index_strategies();
    for count in [100usize, 1_000, 10_000] {
        benchmark_count(count);
        benchmark_composed_cards(count);
    }
}

fn benchmark_empty() {
    let list = VirtualList::<usize>::new("empty");
    let mut state = VirtualListState::default();
    let mut cx = LayoutCx::new();
    list.sync(40, &mut state, &mut cx);
    assert_eq!(cx.measured_nodes(), 0);
    assert_eq!(state.total_height(), 0);
    for direction in [-1, 1] {
        assert!(!state.scroll_by(direction, 20));
        assert_eq!(state.scroll.vertical_offset(), 0);
    }
    let report = paint_once(&list, &state, Rect::new(0, 0, 40, 20));
    assert_eq!(report.rendered.painted_items, 0);
    assert_eq!(report.rendered.registered_items, 0);
    assert_eq!(report.hit_regions, 0);
    assert_eq!(report.focus_targets, 0);
    assert_eq!(report.semantic_regions, 0);
    assert_eq!(report.selection_fragments, 0);
    assert_eq!(report.image_contributions, 0);
    println!(
        "empty paint_us={} allocations={} allocation_bytes={}",
        micros(report.elapsed),
        report.allocations,
        report.allocation_bytes,
    );

    let populated = build_list(100, 0);
    populated.sync(40, &mut state, &mut cx);
    assert!(state.scroll_by(10, 20));
    state.capture_anchor();
    let measured = cx.measured_nodes();
    list.sync(40, &mut state, &mut cx);
    state.restore_anchor(20);
    assert_eq!(cx.measured_nodes(), measured);
    assert_eq!(state.total_height(), 0);
    assert_eq!(state.scroll.vertical_offset(), 0);
    assert!(state.item_offset(&0).is_none());
    assert!(state.key_at_offset(0).is_none());
    let cleared = paint_once(&list, &state, Rect::new(0, 0, 40, 20));
    assert_eq!(cleared.rendered.painted_items, 0);
    assert_eq!(cleared.rendered.registered_items, 0);
    assert_eq!(cleared.hit_regions, 0);
    assert_eq!(cleared.selection_fragments, 0);
    assert_eq!(cleared.semantic_regions, 0);
}

fn benchmark_composed_cards(count: usize) {
    let list = (0..count).fold(VirtualList::new("cards"), |list, index| {
        let background = bmux_tui::style::Style::new().bg(bmux_tui::style::Color::Blue);
        list.component(
            index,
            Surface::new(
                Column::new()
                    .child(TextBlock::new(format!("Author {index}")))
                    .child(TextBlock::new(
                        "Wrapped Unicode 界 message body. ".repeat(index % 3 + 1),
                    )),
            )
            .padding(bmux_tui::geometry::Insets::all(1))
            .background(background)
            .content_style(background),
        )
    });
    let mut state = VirtualListState::default();
    let mut cx = LayoutCx::new();
    list.sync(40, &mut state, &mut cx);
    let measured = cx.measured_nodes();
    list.sync(40, &mut state, &mut cx);
    assert_eq!(
        cx.measured_nodes(),
        measured,
        "unchanged cards must reuse layout"
    );
    let viewport = Rect::new(0, 0, 40, 20);
    state.scroll.set_vertical_offset(state.total_height() / 2);
    let paint = paint_once(&list, &state, viewport);
    assert!(paint.rendered.painted_items > 0);
    assert!(
        paint.rendered.painted_items <= 6,
        "paint must remain viewport bounded"
    );
    let old_offset = state.scroll.vertical_offset();
    assert!(state.scroll_by(1, usize::from(viewport.height)));
    assert_eq!(state.scroll.vertical_offset(), old_offset + 1);
    // Pure scrolling consumes retained geometry; synchronization is a separate
    // diagnostic for callers that check unchanged models on every frame.
    let row_scroll = paint_once(&list, &state, viewport);
    let row_sync_started = Instant::now();
    list.sync(40, &mut state, &mut cx);
    let row_sync = row_sync_started.elapsed();
    assert_eq!(
        cx.measured_nodes(),
        measured,
        "unchanged synchronization must not remeasure cards"
    );
    assert!(row_scroll.rendered.painted_items > 0);
    assert!(row_scroll.rendered.painted_items <= 6);
    let mut scroll_elapsed = Duration::ZERO;
    let mut scroll_allocations = 0usize;
    let mut scroll_allocation_bytes = 0usize;
    let mut scroll_max_painted = 0usize;
    let scroll_start = state.scroll.vertical_offset();
    for direction in [1, -1] {
        for _ in 0..64 {
            assert!(state.scroll_by(direction, usize::from(viewport.height)));
            let report = paint_once(&list, &state, viewport);
            assert!(report.rendered.painted_items > 0);
            assert!(report.rendered.painted_items <= 6);
            assert_eq!(
                report.rendered.registered_items,
                report.rendered.painted_items
            );
            scroll_elapsed += report.elapsed;
            scroll_allocations += report.allocations;
            scroll_allocation_bytes += report.allocation_bytes;
            scroll_max_painted = scroll_max_painted.max(report.rendered.painted_items);
        }
    }
    assert_eq!(state.scroll.vertical_offset(), scroll_start);
    assert_eq!(cx.measured_nodes(), measured);
    println!(
        "cards_scroll count={count} steps=128 paint_us={} allocations={scroll_allocations} allocation_bytes={scroll_allocation_bytes} max_painted={scroll_max_painted}",
        micros(scroll_elapsed),
    );
    for (offset, direction) in [
        (0, -1),
        (state.total_height() - usize::from(viewport.height), 1),
    ] {
        state.scroll.set_vertical_offset(offset);
        assert!(!state.scroll_by(direction, usize::from(viewport.height)));
        assert_eq!(state.scroll.vertical_offset(), offset);
        let boundary = paint_once(&list, &state, viewport);
        assert!(boundary.rendered.painted_items > 0);
        assert!(boundary.rendered.painted_items <= 6);
        assert_eq!(
            boundary.rendered.registered_items,
            boundary.rendered.painted_items
        );
    }
    state.scroll.set_vertical_offset(scroll_start);
    state.capture_anchor();
    let anchor_key = *state
        .key_at_offset(state.scroll.vertical_offset())
        .expect("middle card must exist");
    let anchor_row = state.scroll.vertical_offset() - state.item_offset(&anchor_key).unwrap();
    let before_resize = cx.measured_nodes();
    let started = Instant::now();
    list.sync(24, &mut state, &mut cx);
    state.restore_anchor(usize::from(viewport.height));
    let resize = started.elapsed();
    let resize_measured = cx.measured_nodes() - before_resize;
    assert_eq!(
        resize_measured,
        count * 4,
        "each card descendant must reflow"
    );
    assert_eq!(
        state.scroll.vertical_offset(),
        state.item_offset(&anchor_key).unwrap() + anchor_row,
        "narrowing cards must preserve the stable key and intra-card row"
    );
    list.sync(24, &mut state, &mut cx);
    assert_eq!(cx.measured_nodes(), before_resize + resize_measured);
    let resized_paint = paint_once(&list, &state, Rect::new(0, 0, 24, 20));
    assert!(resized_paint.rendered.painted_items > 0);
    assert!(resized_paint.rendered.painted_items <= 6);
    for report in [&paint, &row_scroll, &resized_paint] {
        assert_eq!(
            report.rendered.registered_items, report.rendered.painted_items,
            "composed card registration must remain visible-item bounded"
        );
    }
    println!(
        "cards count={count} measured={measured} paint_us={} painted={} allocations={} allocation_bytes={} row_sync_us={} row_paint_us={} row_sync_and_paint_us={} row_painted={} row_allocations={} row_allocation_bytes={} resize_us={} resize_measured={resize_measured} resized_painted={} resized_allocations={} resized_allocation_bytes={}",
        micros(paint.elapsed),
        paint.rendered.painted_items,
        paint.allocations,
        paint.allocation_bytes,
        micros(row_sync),
        micros(row_scroll.elapsed),
        micros(row_sync + row_scroll.elapsed),
        row_scroll.rendered.painted_items,
        row_scroll.allocations,
        row_scroll.allocation_bytes,
        micros(resize),
        resized_paint.rendered.painted_items,
        resized_paint.allocations,
        resized_paint.allocation_bytes,
    );
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

    state.scroll.set_follow_bottom(true);
    state.restore_anchor(usize::from(viewport.height));
    let started = Instant::now();
    let appended = build_list(count.saturating_add(1), 0);
    appended.sync(80, &mut state, &mut layout_cx);
    state.restore_anchor(usize::from(viewport.height));
    let append_follows_bottom = state.scroll.follows_bottom();
    let append_offset = state.scroll.vertical_offset();
    let append_maximum = state
        .total_height()
        .saturating_sub(usize::from(viewport.height));
    let append = started.elapsed();
    let append_measured = layout_cx
        .measured_nodes()
        .saturating_sub(initial_measured.saturating_add(steady_measured));

    state.scroll.set_follow_bottom(false);
    state.scroll.set_vertical_offset(middle);
    state.capture_anchor();
    let insert_anchor_key = state
        .key_at_offset(state.scroll.vertical_offset())
        .copied()
        .expect("middle offset must resolve to an item");
    let insert_anchor_row = state
        .scroll
        .vertical_offset()
        .saturating_sub(state.item_offset(&insert_anchor_key).unwrap());
    let before_insert = layout_cx.measured_nodes();
    let started = Instant::now();
    let inserted = build_list_with_prefix(count, 0);
    inserted.sync(80, &mut state, &mut layout_cx);
    state.restore_anchor(usize::from(viewport.height));
    let insert_anchor_offset = state.item_offset(&insert_anchor_key).unwrap();
    let insert_restored_offset = state.scroll.vertical_offset();
    let insert = started.elapsed();
    let insert_measured = layout_cx.measured_nodes().saturating_sub(before_insert);

    let before_remove = layout_cx.measured_nodes();
    state.capture_anchor();
    let remove_anchor_key = state
        .key_at_offset(state.scroll.vertical_offset())
        .copied()
        .expect("inserted-list offset must resolve to an item");
    let remove_anchor_row = state
        .scroll
        .vertical_offset()
        .saturating_sub(state.item_offset(&remove_anchor_key).unwrap());
    let started = Instant::now();
    list.sync(80, &mut state, &mut layout_cx);
    state.restore_anchor(usize::from(viewport.height));
    let remove = started.elapsed();
    let remove_measured = layout_cx.measured_nodes().saturating_sub(before_remove);
    let remove_anchor_offset = state.item_offset(&remove_anchor_key).unwrap();
    let remove_restored_offset = state.scroll.vertical_offset();

    state.capture_anchor();
    let reorder_anchor_key = state
        .key_at_offset(state.scroll.vertical_offset())
        .copied()
        .expect("pre-reorder offset must resolve to an item");
    let reorder_anchor_row = state
        .scroll
        .vertical_offset()
        .saturating_sub(state.item_offset(&reorder_anchor_key).unwrap());
    let before_reorder = layout_cx.measured_nodes();
    let started = Instant::now();
    let reordered = build_reordered_list(count, 0);
    reordered.sync(80, &mut state, &mut layout_cx);
    state.restore_anchor(usize::from(viewport.height));
    let reorder_anchor_offset = state.item_offset(&reorder_anchor_key).unwrap();
    let reorder_restored_offset = state.scroll.vertical_offset();
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

    state.capture_anchor();
    let resize_anchor_key = state
        .key_at_offset(state.scroll.vertical_offset())
        .copied()
        .expect("pre-resize offset must resolve to an item");
    let resize_anchor_row = state
        .scroll
        .vertical_offset()
        .saturating_sub(state.item_offset(&resize_anchor_key).unwrap());
    let before_resize = layout_cx.measured_nodes();
    let started = Instant::now();
    list.sync(64, &mut state, &mut layout_cx);
    state.restore_anchor(usize::from(viewport.height));
    let resize_anchor_offset = state.item_offset(&resize_anchor_key).unwrap();
    let resize_restored_offset = state.scroll.vertical_offset();
    let resize = started.elapsed();
    let resize_measured = layout_cx.measured_nodes().saturating_sub(before_resize);

    assert_eq!(
        steady_measured, 0,
        "unchanged layout must reuse measurements"
    );
    assert_eq!(append_measured, 1, "append must measure only the new item");
    assert!(append_follows_bottom, "append must preserve bottom follow");
    assert_eq!(
        append_offset, append_maximum,
        "append must restore the exact bottom anchor"
    );
    assert_eq!(insert_measured, 1, "prepend must measure only the new item");
    assert_eq!(
        insert_restored_offset,
        insert_anchor_offset.saturating_add(insert_anchor_row),
        "prepend must preserve the stable top item and intra-item row"
    );
    assert_eq!(
        remove_measured, 0,
        "removal must retain every unaffected measurement"
    );
    assert_eq!(
        remove_restored_offset,
        remove_anchor_offset.saturating_add(remove_anchor_row),
        "removal above must preserve the stable top item and intra-item row"
    );
    assert_eq!(
        reorder_measured, 0,
        "reorder must retain keyed measurements"
    );
    assert_eq!(
        reorder_restored_offset,
        reorder_anchor_offset.saturating_add(reorder_anchor_row),
        "reorder must preserve the stable top item and intra-item row"
    );
    assert_eq!(
        paint_revision_measured, 0,
        "paint-only revisions must not invalidate geometry"
    );
    assert_eq!(
        resize_measured, count,
        "width changes must remeasure each width-dependent item exactly once"
    );
    assert_eq!(
        resize_restored_offset,
        resize_anchor_offset.saturating_add(resize_anchor_row),
        "width reflow must restore the stable top item and intra-item row"
    );
    for report in [&initial_paint, &row_scroll, &page_scroll] {
        assert!(
            report.rendered.painted_items <= usize::from(viewport.height).saturating_add(2),
            "paint work must remain bounded by viewport-intersecting items"
        );
        assert_eq!(
            report.rendered.registered_items, report.rendered.painted_items,
            "interaction registration must remain visible-item bounded"
        );
    }

    println!(
        "items={count} build_us={} initial_layout_us={} initial_measured={} steady_layout_us={} steady_measured={} initial_paint_us={} initial_painted={} row_scroll_us={} row_painted={} page_scroll_us={} page_painted={} allocations={} allocation_bytes={} hit_regions={} focus_targets={} semantic_regions={} selection_fragments={} image_contributions={} damage_regions={} damaged_cells={} frame_output_bytes={} append_us={} append_measured={} insert_us={} insert_measured={} remove_us={} remove_measured={} reorder_us={} reorder_measured={} paint_revision_us={} paint_revision_measured={} resize_us={} resize_measured={} cache_hits={} cache_misses={} cache_released={} total_rows={}",
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
        micros(remove),
        remove_measured,
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
        list.component(index, benchmark_item(index, paint_revision))
    })
}

fn build_list_with_prefix(count: usize, paint_revision: u64) -> VirtualList<'static, usize> {
    (0..count).fold(
        VirtualList::new("benchmark")
            .component(usize::MAX, benchmark_item(usize::MAX, paint_revision)),
        |list, index| list.component(index, benchmark_item(index, paint_revision)),
    )
}

fn build_reordered_list(count: usize, paint_revision: u64) -> VirtualList<'static, usize> {
    (0..count)
        .rev()
        .fold(VirtualList::new("benchmark"), |list, index| {
            list.component(index, benchmark_item(index, paint_revision))
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
    TextBlock::new(text).style(if paint_revision == 0 {
        bmux_tui::style::Style::new()
    } else {
        bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Blue)
    })
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}
