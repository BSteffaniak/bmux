//! Rust codegen for a BPDL schema.
//!
//! Given a validated [`crate::ast::Schema`], produces a string of Rust
//! source that defines:
//!
//! - Structs for each `record` (with `Clone`, `Debug`, `PartialEq`,
//!   and serde derives).
//! - Enums for each `variant` (tagged union) and `enum` (pure tag).
//! - A `Consumer` async trait bundling every `query` and `command`.
//!
//! The codegen is deliberately minimal: it produces source meant to be
//! emitted into a plugin-api crate via a proc macro
//! (`bmux_plugin_schema_macros`) or written to disk by a build script.
//! It does not own async runtime choices, transport, or dispatch — those
//! are wired up separately by the plugin SDK.

use std::fmt::Write as _;

use crate::ast::{
    EnumDef, Field, Interface, InterfaceItem, Operation, Primitive, RecordDef, Schema, TypeRef,
    VariantCase, VariantDef,
};

/// Emit a Rust module for the entire schema. The output is a single
/// module body — the caller is responsible for wrapping it in the
/// appropriate `mod { ... }` if desired.
#[must_use]
pub fn emit(schema: &Schema) -> String {
    let mut out = String::new();
    out.push_str("// AUTO-GENERATED FROM BPDL. DO NOT EDIT BY HAND.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");
    for iface in &schema.interfaces {
        emit_interface(iface, &mut out);
    }
    out
}

fn emit_interface(iface: &Interface, out: &mut String) {
    let module_name = snake_case(&iface.name);
    let _ = writeln!(out, "pub mod {module_name} {{");
    out.push_str("    use super::*;\n\n");

    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => emit_record(r, out),
            InterfaceItem::Variant(v) => emit_variant(v, out),
            InterfaceItem::Enum(e) => emit_enum(e, out),
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_) => {}
        }
    }

    // Consumer trait contains queries + commands. Events are exposed via a
    // typed event subscription the SDK generates separately from the trait
    // surface, so they're only informational in the codegen.
    emit_consumer_trait(iface, out);

    out.push_str("}\n\n");
}

fn emit_record(r: &RecordDef, out: &mut String) {
    let name = pascal_case(&r.name);
    out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
    let _ = writeln!(out, "    pub struct {name} {{");
    for f in &r.fields {
        let field_name = snake_case(&f.name);
        let ty = rust_type(&f.ty);
        let _ = writeln!(out, "        pub {field_name}: {ty},");
    }
    out.push_str("    }\n\n");
}

fn emit_variant(v: &VariantDef, out: &mut String) {
    let name = pascal_case(&v.name);
    out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
    out.push_str("    #[serde(tag = \"kind\", rename_all = \"snake_case\")]\n");
    let _ = writeln!(out, "    pub enum {name} {{");
    for c in &v.cases {
        emit_variant_case(c, out);
    }
    out.push_str("    }\n\n");
}

fn emit_variant_case(case: &VariantCase, out: &mut String) {
    let case_name = pascal_case(&case.name);
    if case.payload.is_empty() {
        let _ = writeln!(out, "        {case_name},");
    } else {
        let _ = writeln!(out, "        {case_name} {{");
        for f in &case.payload {
            let field_name = snake_case(&f.name);
            let ty = rust_type(&f.ty);
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
}

fn emit_consumer_trait(iface: &Interface, out: &mut String) {
    let trait_name = pascal_case(&iface.name);
    out.push_str("    /// Consumer-facing trait for this interface.\n");
    out.push_str("    ///\n");
    out.push_str("    /// Other plugins call these methods to query or command the\n");
    out.push_str("    /// providing plugin. Returned futures are async.\n");
    let _ = writeln!(out, "    pub trait {trait_name}: Send + Sync {{");
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            emit_operation_signature(op, out);
        }
    }
    out.push_str("    }\n\n");
}

fn emit_operation_signature(op: &Operation, out: &mut String) {
    let name = snake_case(&op.name);
    let params = op
        .params
        .iter()
        .map(|f: &Field| format!("{}: {}", snake_case(&f.name), rust_type(&f.ty)))
        .collect::<Vec<_>>()
        .join(", ");
    let returns = rust_type(&op.returns);
    let sep = if op.params.is_empty() { "" } else { ", " };
    let _ = writeln!(
        out,
        "        fn {name}<'a>(&'a self{sep}{params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>>;"
    );
}

fn rust_type(ty: &TypeRef) -> String {
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
        TypeRef::Option(inner) => format!("Option<{}>", rust_type(inner)),
        TypeRef::List(inner) => format!("Vec<{}>", rust_type(inner)),
        TypeRef::Result(ok, err) => {
            format!(
                "::std::result::Result<{}, {}>",
                rust_type(ok),
                rust_type(err)
            )
        }
        TypeRef::Unit => "()".to_string(),
    }
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
    use super::emit;
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
    fn emits_consumer_trait_with_queries_and_commands() {
        let src = "plugin p version 1;\n\
                   interface windows-state {\n\
                     record pane-state { id: uuid }\n\
                     query pane-state(id: uuid) -> pane-state?;\n\
                     command focus-pane(id: uuid) -> result<unit, string>;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub trait WindowsState"));
        assert!(rust.contains("fn pane_state"));
        assert!(rust.contains("fn focus_pane"));
        assert!(rust.contains("Option<PaneState>"));
        assert!(rust.contains("::std::result::Result<(), String>"));
    }
}
