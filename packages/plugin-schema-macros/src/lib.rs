//! Proc macros that embed a BPDL schema into a Rust plugin-api crate and
//! generate the typed bindings at compile time.
//!
//! The `schema!` macro accepts a braced argument block describing the
//! schema source and any cross-plugin imports. Example:
//!
//! ```ignore
//! // Simple schema with no imports.
//! bmux_plugin_schema_macros::schema! {
//!     source: "bpdl/windows-plugin.bpdl",
//! }
//!
//! // Schema that imports types from another plugin.
//! bmux_plugin_schema_macros::schema! {
//!     source: "bpdl/my-plugin.bpdl",
//!     imports: {
//!         windows: {
//!             source: "../windows-plugin-api/bpdl/windows-plugin.bpdl",
//!             crate_path: ::bmux_windows_plugin_api,
//!         },
//!     },
//! }
//! ```
//!
//! Paths are resolved relative to the invoking crate's root (matching
//! `include_str!` semantics).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use bmux_plugin_schema::codegen_rust::{ImportInfo, ImportMap};
use proc_macro::TokenStream;
use quote::ToTokens;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Path, Token, braced, parse_macro_input};

/// Embed a BPDL schema and emit the generated Rust bindings in place.
///
/// See module docs for the invocation grammar.
#[proc_macro]
pub fn schema(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as SchemaArgs);
    match expand(&args) {
        Ok(tokens) => tokens,
        Err(msg) => quote::quote!(compile_error!(#msg);).into(),
    }
}

/// Embed an inline BPDL schema source string and emit the generated
/// Rust bindings in place. No file I/O — the entire schema is the
/// macro argument. Useful for small self-contained schemas and for
/// proc-macro tests where path resolution against `CARGO_MANIFEST_DIR`
/// is awkward.
///
/// # Example
///
/// ```ignore
/// bmux_plugin_schema_macros::schema_inline!(r#"
///     plugin my.plugin version 1;
///     interface iface { record r { id: uuid } }
/// "#);
/// ```
#[proc_macro]
pub fn schema_inline(input: TokenStream) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    let source = lit.value();
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

fn expand(args: &SchemaArgs) -> Result<TokenStream, String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|err| format!("CARGO_MANIFEST_DIR not set: {err}"))?;
    let manifest_dir = PathBuf::from(manifest_dir);

    // Resolve imports first so codegen can emit qualified type paths.
    let mut imports = ImportMap::new();
    let mut import_schemas: BTreeMap<String, bmux_plugin_schema::ast::Schema> = BTreeMap::new();
    for import in &args.imports {
        let path = manifest_dir.join(&import.source);
        let source = std::fs::read_to_string(&path).map_err(|err| {
            format!(
                "failed to read imported BPDL schema `{}`: {err}",
                path.display()
            )
        })?;
        let schema = bmux_plugin_schema::compile(&source)
            .map_err(|err| format!("imported schema `{}` failed: {err}", path.display()))?;
        import_schemas.insert(import.alias.clone(), schema.clone());
        imports.insert(
            import.alias.clone(),
            ImportInfo {
                crate_path: import.crate_path.clone(),
                schema,
            },
        );
    }

    // Parse the primary schema with imports resolved.
    let path = manifest_dir.join(&args.source);
    let source = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read BPDL schema `{}`: {err}", path.display()))?;
    let schema = bmux_plugin_schema::compile_with_imports(&source, &import_schemas)
        .map_err(|err| err.to_string())?;

    let rust = bmux_plugin_schema::codegen_rust::emit_with_imports(&schema, &imports);
    rust.parse::<proc_macro2::TokenStream>()
        .map(Into::into)
        .map_err(|err| format!("bmux_plugin_schema codegen produced invalid Rust: {err}"))
}

/// Parsed arguments to the `schema!` macro.
struct SchemaArgs {
    source: String,
    imports: Vec<ImportEntry>,
}

struct ImportEntry {
    alias: String,
    source: String,
    crate_path: String,
}

impl Parse for SchemaArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        // Parse a `key: value` list separated by commas, inside an
        // ambient brace (the macro invocation's outer braces). `syn`
        // already strips the outer braces when parsing the TokenStream,
        // so we're reading the key/value pairs directly here.
        let mut source: Option<String> = None;
        let mut imports: Vec<ImportEntry> = Vec::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "source" => {
                    let lit: LitStr = input.parse()?;
                    source = Some(lit.value());
                }
                "imports" => {
                    let content;
                    braced!(content in input);
                    while !content.is_empty() {
                        let alias: Ident = content.parse()?;
                        content.parse::<Token![:]>()?;
                        let body;
                        braced!(body in content);
                        let mut entry_source: Option<String> = None;
                        let mut entry_path: Option<String> = None;
                        while !body.is_empty() {
                            let field: Ident = body.parse()?;
                            body.parse::<Token![:]>()?;
                            match field.to_string().as_str() {
                                "source" => {
                                    let lit: LitStr = body.parse()?;
                                    entry_source = Some(lit.value());
                                }
                                "crate_path" => {
                                    let path: Path = body.parse()?;
                                    entry_path = Some(path.to_token_stream().to_string());
                                }
                                other => {
                                    return Err(syn::Error::new(
                                        field.span(),
                                        format!(
                                            "unknown import field `{other}`; expected `source` or `crate_path`"
                                        ),
                                    ));
                                }
                            }
                            if body.peek(Token![,]) {
                                body.parse::<Token![,]>()?;
                            }
                        }
                        let src = entry_source.ok_or_else(|| {
                            syn::Error::new(alias.span(), "import missing `source` field")
                        })?;
                        let cp = entry_path.ok_or_else(|| {
                            syn::Error::new(alias.span(), "import missing `crate_path` field")
                        })?;
                        imports.push(ImportEntry {
                            alias: alias.to_string(),
                            source: src,
                            // Normalize: syn emits `:: foo :: bar`
                            // (spaces); remove them so downstream
                            // string ops see a tidy path.
                            crate_path: cp.replace(' ', ""),
                        });
                        if content.peek(Token![,]) {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown schema argument `{other}`; expected `source` or `imports`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let source = source
            .ok_or_else(|| syn::Error::new(input.span(), "schema! requires a `source:` field"))?;
        Ok(Self { source, imports })
    }
}
