//! Typed public API of the bmux cluster plugin.
//!
//! This crate is the stable contract other plugins and clients use for
//! cluster inventory, orchestration commands, and connection lifecycle
//! events. Runtime cluster state and behavior remain in
//! `bmux_cluster_plugin`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/cluster-plugin.bpdl",
}
