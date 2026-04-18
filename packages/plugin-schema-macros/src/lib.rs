//! Proc macros that embed a BPDL schema into a Rust plugin-api crate and
//! generate the typed bindings at compile time.
//!
//! The `schema!` macro takes a path (relative to the crate root, matching
//! `include_str!` semantics) pointing at a `.bpdl` file, parses and
//! validates it via `bmux_plugin_schema`, and emits the generated Rust
//! as a `TokenStream` directly into the caller's module.
//!
//! # Example
//!
//! ```ignore
//! bmux_plugin_schema_macros::schema!("bpdl/windows-plugin.bpdl");
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::path::PathBuf;

use proc_macro::TokenStream;
use syn::{LitStr, parse_macro_input};

/// Embed a BPDL schema and emit the generated Rust bindings in place.
///
/// The argument is a path to a `.bpdl` file, resolved relative to the
/// crate root (the directory containing `Cargo.toml`) — matching the
/// resolution rule `include_str!` uses.
#[proc_macro]
pub fn schema(input: TokenStream) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    let rel_path = lit.value();

    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(err) => {
            let msg = format!("CARGO_MANIFEST_DIR not set: {err}");
            return quote::quote!(compile_error!(#msg);).into();
        }
    };
    let path = manifest_dir.join(&rel_path);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            let display = path.display().to_string();
            let msg = format!("failed to read BPDL schema `{display}`: {err}");
            return quote::quote!(compile_error!(#msg);).into();
        }
    };

    let schema = match bmux_plugin_schema::compile(&source) {
        Ok(s) => s,
        Err(err) => {
            let msg = err.to_string();
            return quote::quote!(compile_error!(#msg);).into();
        }
    };

    let rust = bmux_plugin_schema::codegen_rust::emit(&schema);
    match rust.parse::<proc_macro2::TokenStream>() {
        Ok(tokens) => tokens.into(),
        Err(err) => {
            let msg = format!("bmux_plugin_schema codegen produced invalid Rust: {err}");
            quote::quote!(compile_error!(#msg);).into()
        }
    }
}
