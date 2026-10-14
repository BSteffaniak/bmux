//! Contract-only BPDL services. Runtime state and credentials belong to the plugin.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
bmux_plugin_schema_macros::schema! { source: "bpdl/broadcast-plugin.bpdl", }
