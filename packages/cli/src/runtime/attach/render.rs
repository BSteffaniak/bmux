#[cfg(test)]
pub use bmux_attach_pipeline::render::append_pane_output;
pub use bmux_attach_pipeline::render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, opaque_row_text,
    queue_frame_damage_overlay_with_trace, queue_render_ops,
    render_attach_scene_with_stats_and_trace, visible_scene_pane_ids,
};
