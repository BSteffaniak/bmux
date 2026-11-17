//! Compile-time BPDL bindings, dependency-owned schema exports, and plugin
//! manifests generated from BPDL worlds.
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
//!
//! A plugin whose package (and `world`) lives in its own crate adds
//! `manifest: "plugin.toml"` to `schema!`. A plugin whose package lives in
//! a contract crate generates its manifest with [`plugin_manifest!`]. Both
//! emit `PLUGIN_ID`, `PLUGIN_MANIFEST` (the complete manifest TOML the host
//! reads without running plugin code) and a `provides` module re-exporting
//! exactly the provided interfaces, so typed route tables can only name
//! declared routes.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use bmux_plugin_schema::codegen_rust::{ImportInfo, ImportMap};
use proc_macro::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Path, Token, braced, parse_macro_input};

/// Embed a BPDL file, optionally exporting its source for dependency consumers.
#[proc_macro]
pub fn schema(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as SchemaArgs);
    result_tokens(expand(args))
}

/// Generate a plugin manifest from the `world` of a package that lives in a
/// dependency (a plugin API crate) plus the plugin's own settings file:
///
/// ```ignore
/// pub mod declaration {
///     bmux_plugin_schema_macros::plugin_manifest! {
///         package: {
///             schema: ::bmux_tabs_plugin_api::export_schema,
///             crate_path: ::bmux_tabs_plugin_api,
///         },
///         settings: "plugin.toml",
///     }
/// }
/// ```
#[proc_macro]
pub fn plugin_manifest(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as ManifestArgs);
    result_tokens(expand(args.0))
}

/// Continue expansion after a dependency supplies its literal schema text.
/// This is an implementation detail of `schema!`, not a stable entry point.
#[doc(hidden)]
#[proc_macro]
pub fn __schema_import(input: TokenStream) -> TokenStream {
    let continuation = parse_macro_input!(input as Continuation);
    let mut args = continuation.args;
    let Some(entry) = args.imports.get_mut(continuation.index) else {
        return result_tokens(Err("invalid schema import continuation index".into()));
    };
    entry.source = ImportSource::Inline(continuation.source);
    result_tokens(expand_resolved(&args))
}

/// Continue expansion after a dependency supplies the root package text.
/// This is an implementation detail of `plugin_manifest!`.
#[doc(hidden)]
#[proc_macro]
pub fn __schema_package(input: TokenStream) -> TokenStream {
    let continuation = parse_macro_input!(input as Continuation);
    let mut args = continuation.args;
    let Some(package) = args.package.as_mut() else {
        return result_tokens(Err("package continuation without a package".into()));
    };
    package.provider = None;
    args.source = continuation.source;
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
            .map_err(|err| format!("failed to read `{}`: {err}", path.display()))?;
        let path = literal(&path.to_string_lossy());
        // File reads performed by a proc macro alone are not tracked by rustc.
        tracking.extend(quote!(
            const _: &[u8] = include_bytes!(#path);
        ));
        Ok::<_, String>(text)
    };
    if args.package.is_none() {
        args.source = read(&args.source)?;
    }
    if let Some(manifest) = &args.manifest {
        args.manifest = Some(read(manifest)?);
    }
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
    if let Some(PackageRoot {
        provider: Some(provider),
        ..
    }) = &args.package
    {
        return Ok(quote! {
            #provider! { ::bmux_plugin_schema_macros::__schema_package, { #args }, 0 }
        });
    }
    for (index, import) in args.imports.iter().enumerate() {
        if let ImportSource::Dependency(provider) = &import.source {
            return Ok(quote! {
                #provider! { ::bmux_plugin_schema_macros::__schema_import, { #args }, #index }
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
                crate_path: path_string(&import.crate_path),
                schema,
            },
        );
    }
    let schema = if args.package.is_some() {
        // A dependency's package was validated by its own crate; its own
        // imports are not needed to read the world's interface surface.
        bmux_plugin_schema::compile(&args.source)
    } else {
        bmux_plugin_schema::compile_with_imports(&args.source, &schemas)
    }
    .map_err(|err| err.to_string())?;
    let generated: proc_macro2::TokenStream = if args.package.is_some() {
        proc_macro2::TokenStream::new()
    } else {
        bmux_plugin_schema::codegen_rust::emit_with_imports(&schema, &imports)
            .parse()
            .map_err(|err| format!("bmux_plugin_schema codegen produced invalid Rust: {err}"))?
    };
    let manifest = match &args.manifest {
        Some(settings) => manifest_tokens(args, &schema, &imports, settings)?,
        None => proc_macro2::TokenStream::new(),
    };
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
    Ok(quote!(#(#export)* #generated #manifest))
}

fn path_string(path: &Path) -> String {
    path.to_token_stream().to_string().replace(' ', "")
}

/// `PLUGIN_ID`, `PLUGIN_MANIFEST` and the `provides` route module.
fn manifest_tokens(
    args: &SchemaArgs,
    schema: &bmux_plugin_schema::ast::Schema,
    imports: &ImportMap,
    settings: &str,
) -> Result<proc_macro2::TokenStream, String> {
    let (package, set) = bmux_plugin_schema::canonical::package_set(schema, imports);
    let manifest = bmux_plugin_schema::manifest::generate(&set, &package, settings)
        .map_err(|err| format!("plugin manifest generation failed: {err}"))?;
    let local_root = args.package.as_ref().map_or_else(
        || quote!(super),
        |package| package.crate_path.to_token_stream(),
    );
    let mut seen = BTreeMap::new();
    let mut reexports = Vec::new();
    for world in &package.worlds {
        for item in &world.items {
            let bmux_plugin_schema::ir::WorldItem::Provide { interface, .. } = item else {
                continue;
            };
            let (owner, root) = match &interface.import {
                None => (&package, local_root.clone()),
                Some(alias) => {
                    let owner = set
                        .imported(&package, alias)
                        .ok_or_else(|| format!("unresolved world import `{alias}`"))?;
                    let crate_path: Path = syn::parse_str(&imports[alias].crate_path)
                        .map_err(|err| format!("invalid crate path for `{alias}`: {err}"))?;
                    (owner, crate_path.to_token_stream())
                }
            };
            let iface = owner
                .find_interface(&interface.name, interface.version)
                .ok_or_else(|| format!("world names unknown interface `{}`", interface.name))?;
            let module = bmux_plugin_schema::codegen_rust::interface_module(owner, iface);
            if let Some(previous) = seen.insert(module.clone(), iface.wire_id()) {
                return Err(format!(
                    "provided interfaces `{previous}` and `{}` share the Rust module name `{module}`",
                    iface.wire_id()
                ));
            }
            let module = format_ident!("{module}");
            reexports.push(quote!(pub use #root::#module;));
        }
    }
    let id = literal(&package.id);
    let manifest = literal(&manifest);
    let doc = literal(&format!(
        " Complete manifest of `{}`, generated from its BPDL world and plugin settings.",
        package.id
    ));
    Ok(quote! {
        /// Plugin id declared by the BPDL package.
        pub const PLUGIN_ID: &str = #id;
        #[doc = #doc]
        pub const PLUGIN_MANIFEST: &str = #manifest;
        /// Interfaces this plugin provides. Typed route tables name endpoints
        /// through this module, so an undeclared route does not compile.
        pub mod provides {
            #(#reexports)*
        }
    })
}

/// Continuation payload: `{ args } index, "source"`.
struct Continuation {
    args: SchemaArgs,
    index: usize,
    source: String,
}

impl Parse for Continuation {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let content;
        braced!(content in input);
        let args: SchemaArgs = content.parse()?;
        let index: syn::LitInt = input.parse()?;
        let value = index.base10_parse::<usize>()?;
        let pending_package = matches!(
            &args.package,
            Some(PackageRoot {
                provider: Some(_),
                ..
            })
        );
        let pending_import = matches!(
            args.imports.get(value).map(|entry| &entry.source),
            Some(ImportSource::Dependency(_))
        );
        let valid = (pending_package && value == 0) || pending_import;
        if !valid {
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
    /// A path before files are read; schema text afterwards. Empty while a
    /// dependency package is pending.
    source: String,
    export: Option<Ident>,
    /// A settings path before files are read; settings text afterwards.
    manifest: Option<String>,
    /// Root package supplied by a dependency (`plugin_manifest!`). Present
    /// packages generate no bindings: the dependency already owns them.
    package: Option<PackageRoot>,
    imports: Vec<ImportEntry>,
}

struct PackageRoot {
    /// Export macro still to expand, or `None` once `source` holds the text.
    provider: Option<Path>,
    crate_path: Path,
}

impl ToTokens for SchemaArgs {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let source = literal(&self.source);
        tokens.extend(quote!(source: #source,));
        if let Some(export) = &self.export {
            tokens.extend(quote!(export: #export,));
        }
        if let Some(manifest) = &self.manifest {
            let manifest = literal(manifest);
            tokens.extend(quote!(manifest: #manifest,));
        }
        if let Some(package) = &self.package {
            let crate_path = &package.crate_path;
            match &package.provider {
                Some(provider) => {
                    tokens.extend(quote!(package: { schema: #provider, crate_path: #crate_path },))
                }
                None => tokens.extend(quote!(package: { crate_path: #crate_path },)),
            }
        }
        let imports = &self.imports;
        tokens.extend(quote!(imports: { #(#imports,)* }));
    }
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

impl Parse for PackageRoot {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let body;
        braced!(body in input);
        let mut provider = None;
        let mut crate_path = None;
        while !body.is_empty() {
            let field: Ident = body.parse()?;
            body.parse::<Token![:]>()?;
            match field.to_string().as_str() {
                "schema" if provider.is_none() => provider = Some(body.parse()?),
                "crate_path" if crate_path.is_none() => crate_path = Some(body.parse()?),
                _ => {
                    return Err(syn::Error::new(
                        field.span(),
                        "expected one `schema` and one `crate_path`",
                    ));
                }
            }
            if !body.is_empty() {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            provider,
            crate_path: crate_path
                .ok_or_else(|| syn::Error::new(body.span(), "package missing `crate_path`"))?,
        })
    }
}

fn parse_imports(input: ParseStream<'_>) -> syn::Result<Vec<ImportEntry>> {
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
    Ok(entries)
}

impl Parse for SchemaArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut source = None;
        let mut export = None;
        let mut manifest = None;
        let mut package = None;
        let mut imports = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "source" if source.is_none() => source = Some(input.parse::<LitStr>()?.value()),
                "export" if export.is_none() => export = Some(input.parse()?),
                "manifest" if manifest.is_none() => {
                    manifest = Some(input.parse::<LitStr>()?.value());
                }
                // Internal: continuation state of `plugin_manifest!`.
                "package" if package.is_none() => package = Some(input.parse()?),
                "imports" if imports.is_none() => imports = Some(parse_imports(input)?),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown or duplicate schema argument; expected `source`, `export`, `manifest`, or `imports`",
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
            manifest,
            package,
            imports: imports.unwrap_or_default(),
        })
    }
}

/// `plugin_manifest!` arguments, normalized into [`SchemaArgs`].
struct ManifestArgs(SchemaArgs);

impl Parse for ManifestArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut package: Option<PackageRoot> = None;
        let mut settings = None;
        let mut imports = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            match key.to_string().as_str() {
                "package" if package.is_none() => {
                    let root: PackageRoot = input.parse()?;
                    if root.provider.is_none() {
                        return Err(syn::Error::new(
                            key.span(),
                            "`package` requires `schema: <dependency export macro>`",
                        ));
                    }
                    package = Some(root);
                }
                "settings" if settings.is_none() => {
                    settings = Some(input.parse::<LitStr>()?.value());
                }
                "imports" if imports.is_none() => imports = Some(parse_imports(input)?),
                _ => {
                    return Err(syn::Error::new(
                        key.span(),
                        "unknown or duplicate argument; expected `package`, `settings`, or `imports`",
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self(SchemaArgs {
            source: String::new(),
            export: None,
            manifest: Some(settings.ok_or_else(|| {
                syn::Error::new(input.span(), "plugin_manifest! requires `settings:`")
            })?),
            package: Some(package.ok_or_else(|| {
                syn::Error::new(input.span(), "plugin_manifest! requires `package:`")
            })?),
            imports: imports.unwrap_or_default(),
        }))
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
            syn::parse_str::<Continuation>(
                r#"{ source: "plugin p version 1;" } 0, "plugin q version 1;""#
            )
            .is_err()
        );
    }

    #[test]
    fn plugin_manifest_args_round_trip_through_continuations() {
        let ManifestArgs(args) = syn::parse_str(
            r#"
            package: { schema: ::api::export_schema, crate_path: ::api },
            settings: "plugin.toml",
        "#,
        )
        .unwrap();
        assert!(args.package.as_ref().unwrap().provider.is_some());
        let tokens = args.to_token_stream().to_string();
        let reparsed: SchemaArgs = syn::parse_str(&tokens).unwrap();
        assert_eq!(reparsed.manifest.as_deref(), Some("plugin.toml"));
        assert!(reparsed.package.unwrap().provider.is_some());
        let continuation: Continuation =
            syn::parse_str(&format!("{{ {tokens} }} 0, \"plugin p version 1;\"")).unwrap();
        assert_eq!(continuation.source, "plugin p version 1;");
        assert!(syn::parse_str::<ManifestArgs>(r#"settings: "plugin.toml""#).is_err());
        assert!(
            syn::parse_str::<ManifestArgs>(
                r#"package: { crate_path: ::api }, settings: "plugin.toml""#
            )
            .is_err()
        );
    }
}
