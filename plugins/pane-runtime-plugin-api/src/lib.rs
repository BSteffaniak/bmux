//! Typed public API of the bmux pane-runtime plugin.
//!
//! This crate is the stable contract core + other plugins depend on
//! for the pane-runtime domain. Five modules are generated from
//! `bpdl/pane-runtime-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro:
//! - [`pane_runtime_state`] — queries over pane/session runtime.
//! - [`pane_runtime_commands`] — mutating pane + session-runtime commands.
//! - [`attach_runtime_commands`] — per-client attach lifecycle.
//! - [`attach_runtime_state`] — attach-view queries.
//! - [`pane_runtime_events`] — lifecycle event stream.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// BPDL-generated service methods accept one argument per record
// field, which trips `too_many_arguments` on rich commands like
// `launch-pane` (8 args: session_id, target, direction, ratio_percent,
// name, program, args, cwd). The macro-generated code cannot be
// refactored; allow at the crate level.
#![allow(clippy::too_many_arguments)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/pane-runtime-plugin.bpdl",
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneRuntimePluginConfig {
    pub shell: String,
    pub pane_term: String,
    #[serde(default = "default_true")]
    pub bracketed_paste: bool,
    #[serde(default)]
    pub shell_integration_root: Option<std::path::PathBuf>,
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{pane_runtime_commands, pane_runtime_state};
    use pane_runtime_commands::client::SetPanePaddingRequest;

    #[test]
    fn pane_padding_generated_models_preserve_complete_contract_values() {
        let spec = pane_runtime_state::PanePaddingSpec {
            left: 1,
            right: 2,
            top: 3,
            bottom: 4,
            max_content_width: Some(120),
            max_content_height: Some(40),
            horizontal_alignment: "center".to_string(),
            vertical_alignment: "bottom".to_string(),
        };
        let state = pane_runtime_state::PanePaddingState {
            session_id: uuid::Uuid::new_v4(),
            pane_id: uuid::Uuid::new_v4(),
            declarative: spec.clone(),
            matched_rule_index: Some(2),
            runtime_override: Some(spec.clone()),
            effective: spec.clone(),
            source: "runtime_override".to_string(),
            outer_rect: Some(pane_runtime_state::PanePaddingRect {
                x: 0,
                y: 0,
                w: 160,
                h: 50,
            }),
            base_content_rect: None,
            effective_content_rect: Some(pane_runtime_state::PanePaddingRect {
                x: 20,
                y: 5,
                w: 120,
                h: 40,
            }),
            persist_runtime_overrides: true,
        };

        let encoded = bmux_codec::to_vec(&state).expect("encode generated padding state");
        let decoded: pane_runtime_state::PanePaddingState =
            bmux_codec::from_bytes(&encoded).expect("decode generated padding state");
        assert_eq!(decoded, state);

        let request = SetPanePaddingRequest {
            session_id: state.session_id,
            pane_id: Some(state.pane_id),
            padding: spec,
        };
        let encoded = bmux_codec::to_vec(&request).expect("encode generated set request");
        let decoded: SetPanePaddingRequest =
            bmux_codec::from_bytes(&encoded).expect("decode generated set request");
        assert_eq!(decoded.session_id, request.session_id);
        assert_eq!(decoded.pane_id, request.pane_id);
        assert_eq!(decoded.padding, request.padding);
    }
}
