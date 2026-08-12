#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod compositor;
pub mod cursor;
pub mod mouse;
pub mod reconcile;
pub mod render;
pub mod scene_pipeline;
pub mod types;

use bmux_attach_layout_protocol::{
    AttachInputModeState, AttachMouseProtocolEncoding, AttachMouseProtocolMode,
    AttachMouseProtocolState,
};
pub use bmux_attach_pipeline_models::{
    AttachChunkApplyOutcome, AttachOutputChunkMeta, AttachPipelineDiagnosticCode,
    AttachPipelineDiagnosticEvent, AttachViewport,
};
use bmux_terminal_grid::{MouseProtocolEncoding, MouseProtocolMode, ProtocolState};

use std::collections::BTreeMap;
use uuid::Uuid;

pub use compositor::{
    RetainedCompositor, RetainedDamage, RetainedOpacity, RetainedRepaintSurface, RetainedSurface,
    RetainedSurfaceBuilder, RetainedSurfacePayload, frame_damage_from_retained_repaint_plan,
    merge_retained_damages, retained_damage_from_absolute_rects,
    retained_frame_damage_from_frame_damage, retained_layer_order,
    retained_repaint_plan_from_frame_damage, retained_surfaces_from_attach_scene,
};
pub use mouse::{
    Button as AttachMouseButton, Event as AttachMouseEvent, EventKind as AttachMouseEventKind,
    Modifiers as AttachMouseModifiers, PaneProtocol as AttachPaneMouseProtocol,
};
pub use render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, DamageCoalescingPolicy,
    DamageRect, ExtensionRenderStats, FrameDamage, FrameDamageStats, frame_damage_overlay_rects,
    frame_damage_overlay_render_ops, queue_frame_damage_overlay,
    queue_frame_damage_overlay_with_trace,
};
pub use scene_pipeline::AttachScenePipeline;
pub use types::{
    AttachCursorState, AttachPaneMouseProtocolHints, AttachScrollbackCursor,
    AttachScrollbackPosition, PaneRect, PaneRenderBuffer, PaneScrollbackView, PaneScrollbackViews,
    PaneScrollbackWindow, ScrollbackViewportBase, TerminalGraphicsCache,
};

pub fn apply_attach_output_chunk(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    pane_mouse_protocol_hints: &mut BTreeMap<Uuid, AttachMouseProtocolState>,
    pane_input_mode_hints: &mut BTreeMap<Uuid, AttachInputModeState>,
    pane_id: Uuid,
    bytes: &[u8],
    meta: AttachOutputChunkMeta,
) -> AttachChunkApplyOutcome {
    reconcile::apply_attach_output_chunk_with(pane_buffers, pane_id, bytes, meta, |buffer, data| {
        if data.is_empty() {
            return false;
        }

        let _ = buffer.protocol_tracker.process(data);
        update_protocol_hints_from_state(
            pane_mouse_protocol_hints,
            pane_input_mode_hints,
            pane_id,
            buffer.protocol_tracker.protocol_state(),
        );
        true
    })
}

#[must_use]
pub const fn mouse_protocol_mode_to_ipc(mode: MouseProtocolMode) -> AttachMouseProtocolMode {
    match mode {
        MouseProtocolMode::None => AttachMouseProtocolMode::None,
        MouseProtocolMode::Press => AttachMouseProtocolMode::Press,
        MouseProtocolMode::PressRelease => AttachMouseProtocolMode::PressRelease,
        MouseProtocolMode::ButtonMotion => AttachMouseProtocolMode::ButtonMotion,
        MouseProtocolMode::AnyMotion => AttachMouseProtocolMode::AnyMotion,
    }
}

#[must_use]
pub const fn mouse_protocol_encoding_to_ipc(
    encoding: MouseProtocolEncoding,
) -> AttachMouseProtocolEncoding {
    match encoding {
        MouseProtocolEncoding::Default => AttachMouseProtocolEncoding::Default,
        MouseProtocolEncoding::Utf8 => AttachMouseProtocolEncoding::Utf8,
        MouseProtocolEncoding::Sgr => AttachMouseProtocolEncoding::Sgr,
    }
}

pub fn update_protocol_hints_from_state(
    pane_mouse_protocol_hints: &mut BTreeMap<Uuid, AttachMouseProtocolState>,
    pane_input_mode_hints: &mut BTreeMap<Uuid, AttachInputModeState>,
    pane_id: Uuid,
    protocol: ProtocolState,
) {
    pane_mouse_protocol_hints.insert(
        pane_id,
        AttachMouseProtocolState {
            mode: mouse_protocol_mode_to_ipc(protocol.mouse_mode()),
            encoding: mouse_protocol_encoding_to_ipc(protocol.mouse_encoding()),
        },
    );
    pane_input_mode_hints.insert(
        pane_id,
        AttachInputModeState {
            application_cursor: protocol.application_cursor,
            application_keypad: protocol.application_keypad,
            bracketed_paste: protocol.bracketed_paste,
        },
    );
}
