//! Typed application-resurrection host contract.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/application-resurrection.bpdl",
}
