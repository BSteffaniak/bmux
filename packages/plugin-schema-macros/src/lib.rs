//! Compile-time BPDL bindings and dependency-owned schema exports.
//!
//! File sources are relative to the invoking crate's manifest directory.
//! Cross-crate imports should use a dependency's exported schema macro:
//!
//! ```ignore
//! bmux_plugin_schema_macros::schema! {
//!     source: "bpdl/my-plugin.bpdl",
//!     export: export_schema,
//!     imports: {
//!         windows: {
//!             schema: ::bmux_tabs_plugin_api::export_schema,
//!             crate_path: ::bmux_tabs_plugin_api,
//!         },
//!     },
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use bmux_plugin_schema::codegen_rust::{ImportInfo, ImportMap};
use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Path, Token, braced, parse_macro_input};

/// Embed a BPDL file, optionally exporting its source for dependency consumers.
#[proc_macro]
pub fn schema(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as SchemaArgs);
    result_tokens(expand(args))
}

/// Continue expansion after a dependency supplies its literal schema text.
/// This is an implementation detail of `schema!`, not a stable entry point.
#[doc(hidden)]
#[proc_macro]
pub fn __schema_import(input: TokenStream) -> TokenStream {
    let continuation = parse_macro_input!(input as ImportContinuation);
    let mut args = continuation.args;
    args.imports[continuation.index].source = ImportSource::Inline(continuation.source);
    result_tokens(expand_resolved(&args))
}

fn result_tokens(result: Result<proc_macro2::TokenStream, String>) -> TokenStream {
    result
        .unwrap_or_else(|message| quote!(compile_error!(#message);))
        .into()
}

/// Generate bindings from an inline BPDL literal without reading files.
#[proc_macro]
pub fn schema_inline(input: TokenStream) -> TokenStream {
    let source = parse_macro_input!(input as LitStr).value();
    result_tokens((|| {
        let schema = bmux_plugin_schema::compile(&source).map_err(|err| err.to_string())?;
        bmux_plugin_schema::codegen_rust::emit(&schema)
            .parse()
            .map_err(|err| format!("bmux_plugin_schema codegen produced invalid Rust: {err}"))
    })())
}

fn literal(value: &str) -> LitStr {
    LitStr::new(value, proc_macro2::Span::call_site())
}

fn expand(mut args: SchemaArgs) -> Result<proc_macro2::TokenStream, String> {
    let root = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .map_err(|err| format!("CARGO_MANIFEST_DIR not set: {err}"))?,
    );
    let mut tracking = proc_macro2::TokenStream::new();
    let mut read = |source: &str| {
        let path = root.join(source);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read BPDL schema `{}`: {err}", path.display()))?;
        let path = literal(&path.to_string_lossy());
        // File reads performed by a proc macro alone are not tracked by rustc.
        tracking.extend(quote!(
            const _: &[u8] = include_bytes!(#path);
        ));
        Ok::<_, String>(text)
    };
    args.source = read(&args.source)?;
    for import in &mut args.imports {
        if let ImportSource::File(path) = &import.source {
            import.source = ImportSource::Inline(read(path)?);
        }
    }
    let generated = expand_resolved(&args)?;
    Ok(quote!(#tracking #generated))
}

fn expand_resolved(args: &SchemaArgs) -> Result<proc_macro2::TokenStream, String> {
    // Rust expands the provider macro before invoking our continuation, giving
    // us literal data without locating the dependency's source directory.
    for (index, import) in args.imports.iter().enumerate() {
        if let ImportSource::Dependency(provider) = &import.source {
            let source = literal(&args.source);
            let export = args.export.iter().map(|name| quote!(export: #name,));
            let imports = &args.imports;
            return Ok(quote! {
                #provider! {
                    ::bmux_plugin_schema_macros::__schema_import,
                    { source: #source, #(#export)* imports: { #(#imports,)* } },
                    #index
                }
            });
        }
    }

    let mut imports = ImportMap::new();
    let mut schemas = BTreeMap::new();
    for import in &args.imports {
        let ImportSource::Inline(source) = &import.source else {
            return Err("unresolved file import in schema continuation".into());
        };
        let schema = bmux_plugin_schema::compile(source)
            .map_err(|err| format!("imported schema `{}` failed: {err}", import.alias))?;
        let alias = import.alias.to_string();
        schemas.insert(alias.clone(), schema.clone());
        imports.insert(
            alias,
            ImportInfo {
                crate_path: import
                    .crate_path
                    .to_token_stream()
                    .to_string()
                    .replace(' ', ""),
                schema,
            },
        );
    }
    let schema = bmux_plugin_schema::compile_with_imports(&args.source, &schemas)
        .map_err(|err| err.to_string())?;
    let generated: proc_macro2::TokenStream =
        bmux_plugin_schema::codegen_rust::emit_with_imports(&schema, &imports)
            .parse()
            .map_err(|err| format!("bmux_plugin_schema codegen produced invalid Rust: {err}"))?;
    let export = args.export.iter().map(|name| {
        let source = literal(&args.source);
        quote! {
            /// Supply this crate's BPDL source to a schema expansion callback.
            #[macro_export]
            macro_rules! #name {
                ($callback:path, { $($context:tt)* }, $index:literal) => {
                    $callback! { { $($context)* } $index, #source }
                };
            }
        }
    });
    Ok(quote!(#(#export)* #generated))
}

struct ImportContinuation {
    args: SchemaArgs,
    index: usize,
    source: String,
}

impl Parse for ImportContinuation {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let content;
        braced!(content in input);
        let args: SchemaArgs = content.parse()?;
        let index: syn::LitInt = input.parse()?;
        let value = index.base10_parse::<usize>()?;
        if !matches!(
            args.imports.get(value).map(|entry| &entry.source),
            Some(ImportSource::Dependency(_))
        ) {
            return Err(syn::Error::new(
                index.span(),
                "invalid schema import continuation index",
            ));
        }
        input.parse::<Token![,]>()?;
        Ok(Self {
            args,
            index: value,
            source: input.parse::<LitStr>()?.value(),
        })
    }
}

struct SchemaArgs {
    source: String,
    export: Option<Ident>,
    imports: Vec<ImportEntry>,
}

enum ImportSource {
    File(String),
    Dependency(Path),
    Inline(String),
}

struct ImportEntry {
    alias: Ident,
    source: ImportSource,
    crate_path: Path,
}

impl ToTokens for ImportEntry {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let alias = &self.alias;
        let crate_path = &self.crate_path;
        let source = match &self.source {
            ImportSource::File(path) => {
                let path = literal(path);
                quote!(source: #path)
            }
            ImportSource::Dependency(path) => quote!(schema: #path),
            ImportSource::Inline(text) => {
                let text = literal(text);
                quote!(inline: #text)
            }
        };
        tokens.extend(quote!(#alias: { #source, crate_path: #crate_path }));
    }
}

impl Parse for ImportEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let alias: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let body;
        braced!(body in input);
        let mut source = None;
        let mut crate_path = None;
        while !body.is_empty() {
            let field: Ident = body.parse()?;
            body.parse::<Token![:]>()?;
            match field.to_string().as_str() {
                "source" | "schema" | "inline" => {
                    if source.is_some() {
                        return Err(syn::Error::new(
                            field.span(),
                            "specify exactly one of `source`, `schema`, or `inline`",
                        ));
                    }
                    source = Some(match field.to_string().as_str() {
                        "source" => ImportSource::File(body.parse::<LitStr>()?.value()),
                        "schema" => ImportSource::Dependency(body.parse()?),
                        _ => ImportSource::Inline(body.parse::<LitStr>()?.value()),
                    });
                }
                "crate_path" => {
                    if crate_path.is_some() {
                        return Err(syn::Error::new(field.span(), "duplicate `crate_path`"));
                    }
                    crate_path = Some(body.parse()?);
                }
                _ => {
                    return Err(syn::Error::new(
                        field.span(),
                        "expected `source`, `schema`, or `crate_path`",
                    ));
                }
            }
            if !body.is_empty() {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            source: source.ok_or_else(|| {
                syn::Error::new(alias.span(), "import requires `source` or `schema`")
            })?,
            crate_path: crate_path.ok_or_else(|| {
                syn::Error::new(alias.span(), "import missing `crate_path` field")
            })?,
            alias,
        })
    }
}

impl Parse for SchemaArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut source = None;
        let mut export = None;
        let mut imports = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "source" if source.is_none() => source = Some(input.parse::<LitStr>()?.value()),
                "export" if export.is_none() => export = Some(input.parse()?),
                "imports" if imports.is_none() => {
                    let content;
                    braced!(content in input);
                    let mut entries: Vec<ImportEntry> = Vec::new();
                    while !content.is_empty() {
                        let entry: ImportEntry = content.parse()?;
                        if entries.iter().any(|previous| previous.alias == entry.alias) {
                            return Err(syn::Error::new(
                                entry.alias.span(),
                                "duplicate import alias",
                            ));
                        }
                        entries.push(entry);
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                    imports = Some(entries);
                }
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown or duplicate schema argument; expected `source`, `export`, or `imports`",
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            source: source.ok_or_else(|| {
                syn::Error::new(input.span(), "schema! requires a `source:` field")
            })?,
            export,
            imports: imports.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dependency_exports_and_multiple_imports() {
        let args: SchemaArgs = syn::parse_str(
            r#"
            source: "schema.bpdl", export: export_schema,
            imports: {
                first: { schema: ::first::export_schema, crate_path: ::first },
                second: { source: "local.bpdl", crate_path: ::second },
            }
        "#,
        )
        .unwrap();
        assert_eq!(args.export.unwrap().to_string(), "export_schema");
        assert_eq!(args.imports.len(), 2);
        assert!(matches!(
            args.imports[0].source,
            ImportSource::Dependency(_)
        ));
        assert!(matches!(args.imports[1].source, ImportSource::File(_)));
    }

    #[test]
    fn rejects_ambiguous_or_missing_sources() {
        for entry in [
            r#"source: "a.bpdl", schema: ::a::export_schema, crate_path: ::a"#,
            r#"schema: ::a::export_schema, source: "a.bpdl", crate_path: ::a"#,
            "crate_path: ::a",
            "schema: ::a::export_schema",
        ] {
            assert!(
                syn::parse_str::<SchemaArgs>(&format!(
                    "source: \"s.bpdl\", imports: {{ a: {{ {entry} }} }}"
                ))
                .is_err(),
                "accepted invalid import: {entry}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_aliases_and_invalid_continuations() {
        assert!(
            syn::parse_str::<SchemaArgs>(
                r#"
            source: "s.bpdl", imports: {
                a: { schema: ::a::export_schema, crate_path: ::a },
                a: { schema: ::a::export_schema, crate_path: ::a },
            }
        "#
            )
            .is_err()
        );
        assert!(
            syn::parse_str::<ImportContinuation>(
                r#"{ source: "plugin p version 1;" } 0, "plugin q version 1;""#
            )
            .is_err()
        );
    }
}
