//! Typed public API of the bmux sessions plugin.
//!
//! This crate is the stable contract other plugins depend on for the
//! session domain. The [`sessions_state`], [`sessions_commands`], and
//! [`sessions_events`] modules are generated from
//! `bpdl/sessions-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro.
//!
//! Consumers pattern:
//!
//! ```ignore
//! use bmux_sessions_plugin_api::sessions_state::SessionsStateService;
//!
//! fn somewhere(state: &dyn SessionsStateService) {
//!     let sessions = state.list_sessions();
//!     // ...
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/sessions-plugin.bpdl",
}

#[cfg(test)]
mod tests {
    #[test]
    fn direct_program_contract_preserves_argv_and_geometry() {
        use super::sessions_program_v1::{ProgramOptions, client::NewProgramSessionRequest};
        let request = NewProgramSessionRequest {
            name: None,
            options: ProgramOptions {
                program: "printf".into(),
                args: vec![String::new(), "a;$(literal)".into()],
                cwd: None,
                env: std::collections::BTreeMap::new(),
                cols: 91,
                rows: 37,
            },
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: NewProgramSessionRequest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.options.args, request.options.args);
        assert_eq!((decoded.options.cols, decoded.options.rows), (91, 37));
    }
}
