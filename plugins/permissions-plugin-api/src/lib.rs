//! Typed public API of the bmux permissions plugin.
//!
//! This crate is the stable contract other crates depend on for
//! permissions and session-policy services. Modules are generated from
//! `bpdl/permissions-plugin.bpdl` at compile time.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// BPDL-generated service methods accept one argument per record field,
// which trips `too_many_arguments` on rich policy checks. The generated
// client surface mirrors the schema and cannot be usefully refactored here.
#![allow(clippy::too_many_arguments)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/permissions-plugin.bpdl",
}
