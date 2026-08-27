//! Deterministic structural benchmarks for variable-height TUI collections.

use std::time::{Duration, Instant};

use bmux_tui::buffer::Buffer;
use bmux_tui::component::LayoutCx;
use bmux_tui::composition::TextContent;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};

fn main() {
    for count in [100usize, 1_000, 10_000] {
        let started = Instant::now();
        let list = build_list(count);
        let build = started.elapsed();
        let mut state = VirtualListState::new(1);
        let mut layout_cx = LayoutCx::new();
        let started = Instant::now();
        list.sync(80, &mut state, &mut layout_cx);
        let layout = started.elapsed();
        let visible = state.index().visible_range(count / 2, 40);
        state.scroll.set_vertical_offset(count / 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 40));
        let mut frame = Frame::new(&mut buffer);
        let started = Instant::now();
        let render = list.paint(
            Rect::new(0, 0, 80, 40),
            &state,
            &mut PaintCx::new(&mut frame),
        );
        let paint = started.elapsed();
        println!(
            "items={count} build_us={} layout_us={} paint_us={} measured_nodes={} painted_items={} registered_items={} visible_items={} total_rows={}",
            micros(build),
            micros(layout),
            micros(paint),
            layout_cx.measured_nodes(),
            render.painted_items,
            render.registered_items,
            visible.end.saturating_sub(visible.start),
            state.index().total_height(),
        );
    }
}

fn build_list(count: usize) -> VirtualList<'static, usize> {
    (0..count).fold(VirtualList::new("benchmark"), |list, index| {
        let text = match index % 3 {
            0 => format!("short item {index}"),
            1 => format!("medium item {index} with enough words to exercise wrapping"),
            _ => format!(
                "long item {index} with several words that exercise variable-height exact measurement across a constrained terminal width"
            ),
        };
        list.item(index, 0, TextContent::new(text))
    })
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}
