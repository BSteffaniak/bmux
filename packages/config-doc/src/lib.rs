#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

//! Compatibility re-exports for bmux configuration documentation schema.
//!
//! The generic implementation lives in `hyperchad_docs_config` so other
//! HyperChad documentation sites can reuse the same doc-comment-driven config
//! reference generation.

pub use hyperchad_docs_config::*;
