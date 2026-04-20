//! Rust codegen for a BPDL schema.
//!
//! Given a validated [`crate::ast::Schema`], produces a string of Rust
//! source that defines:
//!
//! - Structs for each `record` (with `Clone`, `Debug`, `PartialEq`,
//!   and serde derives).
//! - Enums for each `variant` (tagged union) and `enum` (pure tag).
//! - `impl Default` for any `enum`/`variant` with a `@default` case.
//! - A `<Iface>Service` async trait bundling every `query` and
//!   `command`.
//! - A `pub const INTERFACE_ID: &str` with the canonical name.
//!
//! Qualified type references (`<alias>.<type>`) are resolved against a
//! caller-provided [`ImportMap`], which maps each alias to the Rust
//! crate path where the imported bindings live.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::ast::{
    EnumDef, Field, Interface, InterfaceItem, Operation, Primitive, RecordDef, Schema, TypeRef,
    VariantCase, VariantDef,
};

/// Resolution table used by codegen to turn qualified BPDL type
/// references (`windows.pane-state`) into Rust paths
/// (`::bmux_windows_plugin_api::windows_state::PaneState`).
///
/// Keys are the import aliases declared in the schema's `import`
/// directives; values are the [`ImportInfo`] describing the target crate.
pub type ImportMap = BTreeMap<String, ImportInfo>;

/// Resolution target for a single import alias.
#[derive(Debug, Clone)]
pub struct ImportInfo {
    /// Rust crate path the generated code should prefix onto imported
    /// type references, e.g. `::bmux_windows_plugin_api`.
    pub crate_path: String,
    /// The imported plugin's parsed schema. Used to find which
    /// interface a qualified type belongs to (so the emitted path
    /// includes the right submodule).
    pub schema: Schema,
}

/// Emit a Rust module for the entire schema with no imports resolved.
///
/// Suitable for schemas that do not use qualified type references.
#[must_use]
pub fn emit(schema: &Schema) -> String {
    emit_with_imports(schema, &ImportMap::new())
}

/// Emit a Rust module for the entire schema, resolving qualified type
/// references through `imports`.
#[must_use]
pub fn emit_with_imports(schema: &Schema, imports: &ImportMap) -> String {
    let mut out = String::new();
    out.push_str("// AUTO-GENERATED FROM BPDL. DO NOT EDIT BY HAND.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");
    for iface in &schema.interfaces {
        emit_interface(&schema.plugin.plugin_id, iface, imports, &mut out);
    }
    out
}

fn emit_interface(plugin_id: &str, iface: &Interface, imports: &ImportMap, out: &mut String) {
    let module_name = snake_case(&iface.name);
    let _ = writeln!(out, "pub mod {module_name} {{");
    out.push_str("    use super::*;\n\n");

    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => emit_record(r, imports, out),
            InterfaceItem::Variant(v) => emit_variant(v, imports, out),
            InterfaceItem::Enum(e) => emit_enum(e, out),
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_) => {}
        }
    }

    // Service trait contains queries + commands. Events are exposed
    // separately as a typed `EVENT_KIND` constant + payload type
    // alias below.
    emit_service_trait(iface, imports, out);

    // If this interface declares `events <type>`, emit a canonical
    // `PluginEventKind` constant plus a `EventPayload` type alias so
    // both producers and subscribers import from the same place.
    emit_event_bindings(plugin_id, iface, imports, out);

    out.push_str("}\n\n");
}

fn emit_record(r: &RecordDef, imports: &ImportMap, out: &mut String) {
    let name = pascal_case(&r.name);
    out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
    let _ = writeln!(out, "    pub struct {name} {{");
    for f in &r.fields {
        let field_name = snake_case(&f.name);
        let ty = rust_type(&f.ty, imports);
        let _ = writeln!(out, "        pub {field_name}: {ty},");
    }
    out.push_str("    }\n\n");
}

fn emit_variant(v: &VariantDef, imports: &ImportMap, out: &mut String) {
    let name = pascal_case(&v.name);
    out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
    // External (default) tagging. Internally-tagged variants
    // (`#[serde(tag = ...)]`) require `deserialize_any`, which the
    // non-self-describing `bmux_codec` cannot implement. External
    // tagging serializes the variant discriminant as a length-
    // prefixed key for struct/tuple variants and works uniformly
    // across codec and JSON encodings.
    out.push_str("    #[serde(rename_all = \"snake_case\")]\n");
    let _ = writeln!(out, "    pub enum {name} {{");
    for c in &v.cases {
        emit_variant_case(c, imports, out);
    }
    out.push_str("    }\n\n");

    if let Some(default_case) = v.cases.iter().find(|c| c.is_default) {
        let case_name = pascal_case(&default_case.name);
        let _ = writeln!(
            out,
            "    impl Default for {name} {{\n        fn default() -> Self {{ Self::{case_name} }}\n    }}\n",
        );
    }
}

fn emit_variant_case(case: &VariantCase, imports: &ImportMap, out: &mut String) {
    let case_name = pascal_case(&case.name);
    if case.payload.is_empty() {
        let _ = writeln!(out, "        {case_name},");
    } else {
        let _ = writeln!(out, "        {case_name} {{");
        for f in &case.payload {
            let field_name = snake_case(&f.name);
            let ty = rust_type(&f.ty, imports);
            let _ = writeln!(out, "            {field_name}: {ty},");
        }
        out.push_str("        },\n");
    }
}

fn emit_enum(e: &EnumDef, out: &mut String) {
    let name = pascal_case(&e.name);
    out.push_str("    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n");
    out.push_str("    #[serde(rename_all = \"snake_case\")]\n");
    let _ = writeln!(out, "    pub enum {name} {{");
    for c in &e.cases {
        let _ = writeln!(out, "        {},", pascal_case(&c.name));
    }
    out.push_str("    }\n\n");

    if let Some(default_case) = e.cases.iter().find(|c| c.is_default) {
        let case_name = pascal_case(&default_case.name);
        let _ = writeln!(
            out,
            "    impl Default for {name} {{\n        fn default() -> Self {{ Self::{case_name} }}\n    }}\n",
        );
    }
}

fn emit_service_trait(iface: &Interface, imports: &ImportMap, out: &mut String) {
    let trait_name = format!("{}Service", pascal_case(&iface.name));
    // Canonical interface identifier used to look up a typed service via
    // the plugin host registry. Matches the BPDL `interface <name>` name.
    let _ = writeln!(
        out,
        "    /// Canonical identifier for this interface. Matches the `interface`\n    /// name in the BPDL source exactly; used to look up a provider via\n    /// the plugin host registry.\n    pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"{}\");\n",
        iface.name
    );
    out.push_str("    /// Service trait for this interface.\n");
    out.push_str("    ///\n");
    out.push_str("    /// Consumers call through a `&dyn` reference; providers `impl`\n");
    out.push_str("    /// this trait on their plugin type. Returned futures are\n");
    out.push_str("    /// `Pin<Box<dyn Future + Send>>` to keep the trait object-safe.\n");
    let _ = writeln!(out, "    pub trait {trait_name}: Send + Sync {{");
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            emit_operation_signature(op, imports, out);
        }
    }
    out.push_str("    }\n\n");

    emit_service_client(iface, imports, out, &trait_name);
}

/// Emit event-stream bindings for an interface that declares
/// `events <type>`. Generates:
///
/// - `pub const EVENT_KIND: PluginEventKind` — the namespaced kind
///   (`<plugin.id>/<interface-name>`) used when publishing and
///   subscribing.
/// - `pub type EventPayload = <type>` — a convenient alias for the
///   event payload type so both producer and subscriber can refer to
///   it without re-stating the BPDL type name.
///
/// Interfaces without an `events` declaration emit nothing here.
fn emit_event_bindings(plugin_id: &str, iface: &Interface, imports: &ImportMap, out: &mut String) {
    let Some(event_ty) = iface.items.iter().find_map(|item| match item {
        InterfaceItem::Events(ty) => Some(ty),
        _ => None,
    }) else {
        return;
    };
    let kind_literal = format!("{plugin_id}/{}", iface.name);
    let _ = writeln!(
        out,
        "    /// Canonical [`bmux_plugin_sdk::PluginEventKind`] for this\n    /// interface's event stream. Publishers and subscribers both\n    /// reference this constant; the underlying wire value is\n    /// `\"{kind_literal}\"`.\n    pub const EVENT_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"{kind_literal}\");\n"
    );
    let ty = rust_type(event_ty, imports);
    let _ = writeln!(
        out,
        "    /// Payload type published on this interface's event stream.\n    pub type EventPayload = {ty};\n"
    );
}

fn emit_service_client(iface: &Interface, imports: &ImportMap, out: &mut String, trait_name: &str) {
    let client_name = format!("{}Client", pascal_case(&iface.name));
    out.push_str("    /// Typed client for this interface.\n");
    out.push_str("    ///\n");
    out.push_str("    /// Holds an `Arc<dyn ...Service + Send + Sync>` and forwards every\n");
    out.push_str("    /// method to the underlying provider. Construct via\n");
    out.push_str("    /// [`Client::from_handle`] against a resolved typed service handle.\n");
    out.push_str("    #[derive(Clone)]\n");
    let _ = writeln!(out, "    pub struct {client_name} {{");
    let _ = writeln!(
        out,
        "        inner: ::std::sync::Arc<dyn {trait_name} + Send + Sync>,",
    );
    out.push_str("    }\n\n");

    let _ = writeln!(out, "    impl {client_name} {{");
    out.push_str("        /// Construct directly from a concrete `Arc` to a provider.\n");
    out.push_str("        #[must_use]\n");
    let _ = writeln!(
        out,
        "        pub fn new(provider: ::std::sync::Arc<dyn {trait_name} + Send + Sync>) -> Self {{",
    );
    out.push_str("            Self { inner: provider }\n");
    out.push_str("        }\n\n");

    out.push_str("        /// Borrow the inner provider as a trait reference.\n");
    out.push_str("        #[must_use]\n");
    let _ = writeln!(
        out,
        "        pub fn as_service(&self) -> &(dyn {trait_name} + Send + Sync) {{",
    );
    out.push_str("            &*self.inner\n");
    out.push_str("        }\n");

    // Forward every query/command through the trait.
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            emit_client_forwarder(op, imports, out);
        }
    }

    out.push_str("    }\n\n");
}

fn emit_client_forwarder(op: &Operation, imports: &ImportMap, out: &mut String) {
    let name = snake_case(&op.name);
    let params = op
        .params
        .iter()
        .map(|f: &Field| format!("{}: {}", snake_case(&f.name), rust_type(&f.ty, imports)))
        .collect::<Vec<_>>()
        .join(", ");
    let returns = rust_type(&op.returns, imports);
    let sep = if op.params.is_empty() { "" } else { ", " };
    let arg_names = op
        .params
        .iter()
        .map(|f| snake_case(&f.name))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str("\n        /// Forward to the provider's trait method.\n");
    let _ = writeln!(
        out,
        "        pub fn {name}<'a>(&'a self{sep}{params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>> {{",
    );
    let _ = writeln!(out, "            self.inner.{name}({arg_names})");
    out.push_str("        }\n");
}

fn emit_operation_signature(op: &Operation, imports: &ImportMap, out: &mut String) {
    let name = snake_case(&op.name);
    let params = op
        .params
        .iter()
        .map(|f: &Field| format!("{}: {}", snake_case(&f.name), rust_type(&f.ty, imports)))
        .collect::<Vec<_>>()
        .join(", ");
    let returns = rust_type(&op.returns, imports);
    let sep = if op.params.is_empty() { "" } else { ", " };
    let _ = writeln!(
        out,
        "        fn {name}<'a>(&'a self{sep}{params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>>;"
    );
}

fn rust_type(ty: &TypeRef, imports: &ImportMap) -> String {
    match ty {
        TypeRef::Primitive(p) => match p {
            Primitive::Bool => "bool".to_string(),
            Primitive::U8 => "u8".to_string(),
            Primitive::U16 => "u16".to_string(),
            Primitive::U32 => "u32".to_string(),
            Primitive::U64 => "u64".to_string(),
            Primitive::I8 => "i8".to_string(),
            Primitive::I16 => "i16".to_string(),
            Primitive::I32 => "i32".to_string(),
            Primitive::I64 => "i64".to_string(),
            Primitive::F32 => "f32".to_string(),
            Primitive::F64 => "f64".to_string(),
            Primitive::String => "String".to_string(),
            Primitive::Bytes => "Vec<u8>".to_string(),
            Primitive::Uuid => "::uuid::Uuid".to_string(),
        },
        TypeRef::Named(name) => pascal_case(name),
        TypeRef::Qualified { alias, name } => resolve_qualified(alias, name, imports),
        TypeRef::Option(inner) => format!("Option<{}>", rust_type(inner, imports)),
        TypeRef::List(inner) => format!("Vec<{}>", rust_type(inner, imports)),
        TypeRef::Map(key, value) => {
            format!(
                "::std::collections::BTreeMap<{}, {}>",
                rust_type(key, imports),
                rust_type(value, imports)
            )
        }
        TypeRef::Result(ok, err) => {
            format!(
                "::std::result::Result<{}, {}>",
                rust_type(ok, imports),
                rust_type(err, imports)
            )
        }
        TypeRef::Unit => "()".to_string(),
    }
}

/// Resolve `alias.type-name` to a concrete Rust path by consulting the
/// imports table. If the alias is unknown at codegen time we emit a
/// `::bmux_plugin_schema_unresolved::<alias>::<type>` path that will
/// trigger an obvious compile error; normal validated schemas never hit
/// this branch because the validator requires declared aliases.
fn resolve_qualified(alias: &str, name: &str, imports: &ImportMap) -> String {
    let Some(info) = imports.get(alias) else {
        return format!(
            "::bmux_plugin_schema_unresolved::{}::{}",
            snake_case(alias),
            pascal_case(name)
        );
    };
    // Locate the interface in the imported schema that defines `name`.
    for iface in &info.schema.interfaces {
        for item in &iface.items {
            let defined = match item {
                InterfaceItem::Record(r) => &r.name,
                InterfaceItem::Variant(v) => &v.name,
                InterfaceItem::Enum(e) => &e.name,
                _ => continue,
            };
            if defined == name {
                return format!(
                    "{}::{}::{}",
                    info.crate_path.trim_end_matches("::"),
                    snake_case(&iface.name),
                    pascal_case(name)
                );
            }
        }
    }
    // Validated-but-unresolvable: fallback to same shape so compile
    // errors surface in the emitted code.
    format!(
        "{}::unresolved::{}",
        info.crate_path.trim_end_matches("::"),
        pascal_case(name)
    )
}

fn snake_case(s: &str) -> String {
    // BPDL identifiers use `kebab-case` or `snake_case`. Normalize to
    // snake_case for Rust field/module names.
    s.replace(['-', '.'], "_")
}

fn pascal_case(s: &str) -> String {
    // Convert `kebab-case` or `snake_case` to `PascalCase` for Rust
    // type/trait names.
    let mut out = String::new();
    let mut capitalize = true;
    for c in s.chars() {
        if c == '-' || c == '_' || c == '.' {
            capitalize = true;
            continue;
        }
        if capitalize {
            for up in c.to_uppercase() {
                out.push(up);
            }
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{ImportInfo, ImportMap, emit, emit_with_imports};
    use crate::compile;

    #[test]
    fn emits_record_struct_with_fields() {
        let src = "plugin p version 1;\n\
                   interface my-iface {\n\
                     record pane-state { id: uuid, name: string?, count: u32 }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub mod my_iface"));
        assert!(rust.contains("pub struct PaneState"));
        assert!(rust.contains("pub id: ::uuid::Uuid"));
        assert!(rust.contains("pub name: Option<String>"));
        assert!(rust.contains("pub count: u32"));
    }

    #[test]
    fn emits_variant_with_payload() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     variant status { running, exited { code: i32 } }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub enum Status"));
        assert!(rust.contains("Running,"));
        assert!(rust.contains("Exited {"));
        assert!(rust.contains("code: i32"));
    }

    #[test]
    fn emits_service_trait_with_queries_and_commands() {
        let src = "plugin p version 1;\n\
                   interface windows-state {\n\
                     record pane-state { id: uuid }\n\
                     query pane-state(id: uuid) -> pane-state?;\n\
                     command focus-pane(id: uuid) -> result<unit, string>;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub trait WindowsStateService"));
        assert!(rust.contains("fn pane_state"));
        assert!(rust.contains("fn focus_pane"));
        assert!(rust.contains("Option<PaneState>"));
        assert!(rust.contains("::std::result::Result<(), String>"));
    }

    #[test]
    fn emits_service_client_with_forwarders() {
        let src = "plugin p version 1;\n\
                   interface windows-state {\n\
                     record pane-state { id: uuid }\n\
                     query pane-state(id: uuid) -> pane-state?;\n\
                     command focus-pane(id: uuid) -> result<unit, string>;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub struct WindowsStateClient"),
            "client wrapper not emitted; got: {rust}"
        );
        assert!(
            rust.contains("inner: ::std::sync::Arc<dyn WindowsStateService + Send + Sync>"),
            "client wrapper should hold Arc<dyn Service + Send + Sync>; got: {rust}"
        );
        assert!(
            rust.contains("pub fn new("),
            "client should have new ctor; got: {rust}"
        );
        assert!(
            rust.contains("pub fn as_service"),
            "client should expose as_service borrow; got: {rust}"
        );
        assert!(
            rust.contains("self.inner.pane_state("),
            "client should forward pane_state through inner; got: {rust}"
        );
        assert!(
            rust.contains("self.inner.focus_pane("),
            "client should forward focus_pane through inner; got: {rust}"
        );
    }

    #[test]
    fn emits_interface_id_const() {
        let src = "plugin p version 1;\n\
                   interface windows-state {\n\
                     query ping() -> bool;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"windows-state\");"
            ),
            "codegen must emit the canonical interface id as a typed const; got: {rust}"
        );
    }

    #[test]
    fn emits_event_bindings_for_events_declaration() {
        let src = "plugin bmux.windows version 1;\n\
                   interface windows-events {\n\
                     variant pane-event { focused { pane_id: uuid }, closed { pane_id: uuid } }\n\
                     events pane-event;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const EVENT_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"bmux.windows/windows-events\");"
            ),
            "codegen must emit typed EVENT_KIND for interface with events; got: {rust}"
        );
        assert!(
            rust.contains("pub type EventPayload = PaneEvent;"),
            "codegen must emit EventPayload alias; got: {rust}"
        );
    }

    #[test]
    fn emits_no_event_bindings_without_events_declaration() {
        let src = "plugin p version 1;\n\
                   interface windows-state {\n\
                     query ping() -> bool;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            !rust.contains("EVENT_KIND"),
            "interfaces without events must not emit EVENT_KIND; got: {rust}"
        );
        assert!(
            !rust.contains("EventPayload"),
            "interfaces without events must not emit EventPayload; got: {rust}"
        );
    }

    #[test]
    fn emits_btreemap_for_map_type() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { labels: map<string, u32> }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("::std::collections::BTreeMap<String, u32>"),
            "map lowers to BTreeMap; got: {rust}"
        );
    }

    #[test]
    fn emits_default_impl_for_enum_with_default_case() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     enum e { a, @default b, c }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("impl Default for E"),
            "expected Default impl for enum E; got: {rust}"
        );
        assert!(
            rust.contains("Self::B"),
            "Default impl must use the designated case; got: {rust}"
        );
    }

    #[test]
    fn emits_default_impl_for_variant_unit_case() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     variant v { @default a, b }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("impl Default for V"),
            "expected Default impl for variant V; got: {rust}"
        );
    }

    #[test]
    fn no_default_impl_when_unannotated() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     enum e { a, b }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            !rust.contains("impl Default for E"),
            "no Default impl should be emitted without @default; got: {rust}"
        );
    }

    #[test]
    fn emits_qualified_type_via_import_crate_path() {
        let importer = "plugin importer version 1;\n\
                        import windows = bmux.windows;\n\
                        interface my-iface {\n\
                          query pane-ref(id: uuid) -> windows.pane-state;\n\
                        }";
        let imported_src = "plugin bmux.windows version 1;\n\
                            interface windows-state {\n\
                              record pane-state { id: uuid }\n\
                            }";
        let schema = compile(importer).expect("valid");
        let imported_schema = compile(imported_src).expect("valid");
        let mut imports = ImportMap::new();
        imports.insert(
            "windows".to_string(),
            ImportInfo {
                crate_path: "::bmux_windows_plugin_api".to_string(),
                schema: imported_schema,
            },
        );
        let rust = emit_with_imports(&schema, &imports);
        assert!(
            rust.contains("::bmux_windows_plugin_api::windows_state::PaneState"),
            "qualified type should resolve to imported crate path; got: {rust}"
        );
    }
}
