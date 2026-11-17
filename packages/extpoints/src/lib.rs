//! Extension points the BMUX host program declares.
//!
//! Everything public is generated from `bpdl/bmux-extpoints.bpdl`: for each
//! point, `extension_points::<name>_v<N>` holds the declarative `Data`, the
//! `Behavior` trait a contributor implements, the wire `Request` and the
//! `Point` type hosts declare (`ExtensionPoints::declare::<Point>()`) and
//! plugins extend (`Declarations::extend::<Point>(data)`).
//!
//! This crate is the product layer above the generic plugin framework: core
//! crates never depend on it.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/bmux-extpoints.bpdl",
    export: export_schema,
}
