//! Typed attach-layout state-channel protocol.
//!
//! The attach runtime publishes an [`attach_layout_protocol::AttachLayoutSnapshot`]
//! whenever surfaces, visibility, or geometry change. Plugins that
//! consume layout state (decoration renderers, overlay managers) subscribe
//! via `EventBus::subscribe_state::<AttachLayoutSnapshot>` and see the
//! current snapshot on subscribe plus live updates as the layout shifts.
//!
//! The protocol is domain-agnostic: no decoration, overlay, or other
//! plugin is named. Each consumer decides how to react.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/attach-layout-protocol.bpdl",
    imports: {
        scene: {
            source: "../scene-protocol/bpdl/scene-protocol.bpdl",
            crate_path: ::bmux_scene_protocol,
        },
    },
}
