//! Shared playbook domain library, not neutral host infrastructure.
//!
//! Owns the single existing DSL/TOML parser and model implementation consumed by
//! CLI execution and the managed execution plugin. No plugin activation, service
//! transport, process lifecycle or terminal ownership belongs here.
pub mod diff;
mod escape;
pub mod parse_dsl;
pub mod parse_toml;
pub mod parser;
pub mod subst;
pub mod types;
