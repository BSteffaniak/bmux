#[cfg(test)]
pub fn append_pane_output(
    buffer: &mut bmux_attach_pipeline::PaneRenderBuffer,
    bytes: &[u8],
) -> bool {
    let was_alternate = buffer.protocol_tracker.alternate_screen();
    let _ = buffer.protocol_tracker.process(bytes);
    buffer.terminal_grid.process(bytes);
    let toggled = was_alternate != buffer.protocol_tracker.alternate_screen();
    if toggled {
        buffer.prev_rows.clear();
    }
    toggled
}

pub use bmux_attach_pipeline::render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, opaque_row_text,
    queue_frame_damage_overlay_with_trace, queue_render_ops,
    render_attach_scene_with_stats_and_trace, visible_scene_pane_ids,
};
