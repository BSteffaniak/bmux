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

use std::collections::BTreeMap;
use uuid::Uuid;

pub use compositor::{RetainedCompositor, RetainedDamage, RetainedRepaintSurface, RetainedSurface};
pub use mouse::{
    Button as AttachMouseButton, Event as AttachMouseEvent, EventKind as AttachMouseEventKind,
    Modifiers as AttachMouseModifiers, PaneProtocol as AttachPaneMouseProtocol,
};
pub use render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, DamageCoalescingPolicy,
    DamageRect, FrameDamage, FrameDamageStats, queue_frame_damage_overlay,
    queue_frame_damage_overlay_with_trace,
};
pub use scene_pipeline::AttachScenePipeline;
pub use types::{
    AttachCursorState, AttachPaneMouseProtocolHints, AttachScrollbackCursor,
    AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
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

        let _ = render::append_pane_output(buffer, data);
        let screen = buffer.parser.screen();
        pane_mouse_protocol_hints.insert(
            pane_id,
            AttachMouseProtocolState {
                mode: mouse_protocol_mode_to_ipc(screen.mouse_protocol_mode()),
                encoding: mouse_protocol_encoding_to_ipc(screen.mouse_protocol_encoding()),
            },
        );
        pane_input_mode_hints.insert(
            pane_id,
            AttachInputModeState {
                application_cursor: screen.application_cursor(),
                application_keypad: screen.application_keypad(),
            },
        );
        true
    })
}

#[must_use]
pub const fn mouse_protocol_mode_to_ipc(mode: vt100::MouseProtocolMode) -> AttachMouseProtocolMode {
    match mode {
        vt100::MouseProtocolMode::None => AttachMouseProtocolMode::None,
        vt100::MouseProtocolMode::Press => AttachMouseProtocolMode::Press,
        vt100::MouseProtocolMode::PressRelease => AttachMouseProtocolMode::PressRelease,
        vt100::MouseProtocolMode::ButtonMotion => AttachMouseProtocolMode::ButtonMotion,
        vt100::MouseProtocolMode::AnyMotion => AttachMouseProtocolMode::AnyMotion,
    }
}

#[must_use]
pub const fn mouse_protocol_encoding_to_ipc(
    encoding: vt100::MouseProtocolEncoding,
) -> AttachMouseProtocolEncoding {
    match encoding {
        vt100::MouseProtocolEncoding::Default => AttachMouseProtocolEncoding::Default,
        vt100::MouseProtocolEncoding::Utf8 => AttachMouseProtocolEncoding::Utf8,
        vt100::MouseProtocolEncoding::Sgr => AttachMouseProtocolEncoding::Sgr,
    }
}
