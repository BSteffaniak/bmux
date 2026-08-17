use bmux_attach_layout_protocol::AttachRect;
use bmux_pane_runtime_plugin_api::pane_runtime_state::{
    PanePaddingRect, PanePaddingSpec as ApiPanePaddingSpec, PanePaddingState,
};

use crate::padding::{HorizontalAlignment, PanePaddingSpec, VerticalAlignment};

pub(crate) fn spec_from_api(spec: &ApiPanePaddingSpec) -> Result<PanePaddingSpec, String> {
    Ok(PanePaddingSpec {
        left: spec.left,
        right: spec.right,
        top: spec.top,
        bottom: spec.bottom,
        max_content_width: spec.max_content_width,
        max_content_height: spec.max_content_height,
        horizontal_alignment: HorizontalAlignment::parse(&spec.horizontal_alignment)?,
        vertical_alignment: VerticalAlignment::parse(&spec.vertical_alignment)?,
    })
}

pub(crate) fn spec_to_api(spec: PanePaddingSpec) -> ApiPanePaddingSpec {
    ApiPanePaddingSpec {
        left: spec.left,
        right: spec.right,
        top: spec.top,
        bottom: spec.bottom,
        max_content_width: spec.max_content_width,
        max_content_height: spec.max_content_height,
        horizontal_alignment: spec.horizontal_alignment.as_str().to_string(),
        vertical_alignment: spec.vertical_alignment.as_str().to_string(),
    }
}

pub(crate) const fn rect_to_api(rect: AttachRect) -> PanePaddingRect {
    PanePaddingRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimePanePaddingState {
    pub session_id: uuid::Uuid,
    pub pane_id: uuid::Uuid,
    pub declarative: PanePaddingSpec,
    pub matched_rule_index: Option<usize>,
    pub runtime_override: Option<PanePaddingSpec>,
    pub effective: PanePaddingSpec,
    pub outer_rect: AttachRect,
    pub base_content_rect: AttachRect,
    pub effective_content_rect: AttachRect,
    pub persist_runtime_overrides: bool,
}

impl RuntimePanePaddingState {
    pub(crate) fn into_api(self) -> PanePaddingState {
        PanePaddingState {
            session_id: self.session_id,
            pane_id: self.pane_id,
            declarative: spec_to_api(self.declarative),
            matched_rule_index: self
                .matched_rule_index
                .and_then(|index| u32::try_from(index).ok()),
            runtime_override: self.runtime_override.map(spec_to_api),
            effective: spec_to_api(self.effective),
            source: if self.runtime_override.is_some() {
                "runtime_override"
            } else if self.matched_rule_index.is_some() {
                "rule"
            } else {
                "global"
            }
            .to_string(),
            outer_rect: Some(rect_to_api(self.outer_rect)),
            base_content_rect: Some(rect_to_api(self.base_content_rect)),
            effective_content_rect: Some(rect_to_api(self.effective_content_rect)),
            persist_runtime_overrides: self.persist_runtime_overrides,
        }
    }
}
