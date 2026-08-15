pub use bmux_attach_pipeline::render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, ExtensionRenderStats,
    append_pane_output, collect_visual_projection_updates, frame_damage_overlay_rects,
    frame_damage_overlay_render_ops, opaque_row_text, plugin_scene_items_to_render_items,
    queue_render_items_for_surface, queue_render_ops,
    render_attach_scene_with_stats_and_trace_with_capabilities,
    suppress_terminal_graphics_intersecting, visible_scene_pane_ids,
};
