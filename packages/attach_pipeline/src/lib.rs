#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod cursor;
pub mod reconcile;
pub mod render;
pub mod scene_pipeline;
pub mod types;

pub use bmux_attach_pipeline_models::{
    AttachChunkApplyOutcome, AttachOutputChunkMeta, AttachPipelineDiagnosticCode,
    AttachPipelineDiagnosticEvent, AttachViewport,
};

use std::collections::BTreeMap;
use uuid::Uuid;

pub use scene_pipeline::AttachScenePipeline;
pub use types::{
    AttachCursorState, AttachPaneMouseProtocolHints, AttachScrollbackCursor,
    AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
};

pub fn apply_attach_output_chunk(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    pane_mouse_protocol_hints: &mut BTreeMap<Uuid, bmux_ipc::AttachMouseProtocolState>,
    pane_input_mode_hints: &mut BTreeMap<Uuid, bmux_ipc::AttachInputModeState>,
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
            bmux_ipc::AttachMouseProtocolState {
                mode: mouse_protocol_mode_to_ipc(screen.mouse_protocol_mode()),
                encoding: mouse_protocol_encoding_to_ipc(screen.mouse_protocol_encoding()),
            },
        );
        pane_input_mode_hints.insert(
            pane_id,
            bmux_ipc::AttachInputModeState {
                application_cursor: screen.application_cursor(),
                application_keypad: screen.application_keypad(),
            },
        );
        true
    })
}

#[must_use]
pub const fn mouse_protocol_mode_to_ipc(
    mode: vt100::MouseProtocolMode,
) -> bmux_ipc::AttachMouseProtocolMode {
    match mode {
        vt100::MouseProtocolMode::None => bmux_ipc::AttachMouseProtocolMode::None,
        vt100::MouseProtocolMode::Press => bmux_ipc::AttachMouseProtocolMode::Press,
        vt100::MouseProtocolMode::PressRelease => bmux_ipc::AttachMouseProtocolMode::PressRelease,
        vt100::MouseProtocolMode::ButtonMotion => bmux_ipc::AttachMouseProtocolMode::ButtonMotion,
        vt100::MouseProtocolMode::AnyMotion => bmux_ipc::AttachMouseProtocolMode::AnyMotion,
    }
}

#[must_use]
pub const fn mouse_protocol_encoding_to_ipc(
    encoding: vt100::MouseProtocolEncoding,
) -> bmux_ipc::AttachMouseProtocolEncoding {
    match encoding {
        vt100::MouseProtocolEncoding::Default => bmux_ipc::AttachMouseProtocolEncoding::Default,
        vt100::MouseProtocolEncoding::Utf8 => bmux_ipc::AttachMouseProtocolEncoding::Utf8,
        vt100::MouseProtocolEncoding::Sgr => bmux_ipc::AttachMouseProtocolEncoding::Sgr,
    }
}
