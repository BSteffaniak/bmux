//! Exact, versioned BPDL wire-contract bytes, independent of spans and source formatting.
//! This conservative contract includes all local and imported definitions. Declaration
//! order is retained because enum/variant order can affect wire encoding. It does not
//! claim structural or additive compatibility. No Rust paths or debug formatting enter it.

use crate::ast::{DeliveryMode, Field, InterfaceItem, Operation, Schema, TypeRef};
use crate::codegen_rust::ImportMap;

fn text(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn ty(out: &mut Vec<u8>, value: &TypeRef) {
    match value {
        TypeRef::Primitive(primitive) => {
            out.push(0);
            text(out, primitive.keyword());
        }
        TypeRef::Named(name) => {
            out.push(1);
            text(out, name);
        }
        TypeRef::Qualified { alias, name } => {
            out.push(2);
            text(out, alias);
            text(out, name);
        }
        TypeRef::List(inner) => {
            out.push(3);
            ty(out, inner);
        }
        TypeRef::Map(key, value) => {
            out.push(4);
            ty(out, key);
            ty(out, value);
        }
        TypeRef::Option(inner) => {
            out.push(5);
            ty(out, inner);
        }
        TypeRef::Result(ok, error) => {
            out.push(6);
            ty(out, ok);
            ty(out, error);
        }
        TypeRef::Unit => out.push(7),
    }
}

fn fields(out: &mut Vec<u8>, values: &[Field]) {
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    for field in values {
        text(out, &field.name);
        ty(out, &field.ty);
    }
}

fn operation(out: &mut Vec<u8>, value: &Operation) {
    text(out, &value.name);
    fields(out, &value.params);
    ty(out, &value.returns);
    out.push(u8::from(value.emits.is_some()));
    if let Some(events) = &value.emits {
        ty(out, events);
    }
}

fn schema_bytes(out: &mut Vec<u8>, schema: &Schema) {
    text(out, &schema.plugin.plugin_id);
    out.extend_from_slice(&schema.plugin.version.to_le_bytes());
    out.extend_from_slice(&(schema.imports.len() as u64).to_le_bytes());
    for import in &schema.imports {
        text(out, &import.alias);
        text(out, &import.plugin_id);
    }
    out.extend_from_slice(&(schema.capabilities.len() as u64).to_le_bytes());
    for capability in &schema.capabilities {
        text(out, &capability.name);
        text(out, &capability.id);
    }
    out.extend_from_slice(&(schema.interfaces.len() as u64).to_le_bytes());
    for interface in &schema.interfaces {
        text(out, &interface.name);
        out.push(u8::from(interface.interface_version.is_some()));
        if let Some(version) = interface.interface_version {
            out.extend_from_slice(&version.to_le_bytes());
        }
        out.push(u8::from(interface.capability.is_some()));
        if let Some(capability) = &interface.capability {
            text(out, capability);
        }
        out.extend_from_slice(&(interface.items.len() as u64).to_le_bytes());
        for item in &interface.items {
            match item {
                InterfaceItem::Record(record) => {
                    out.push(0);
                    text(out, &record.name);
                    fields(out, &record.fields);
                }
                InterfaceItem::Variant(variant) => {
                    out.push(1);
                    text(out, &variant.name);
                    out.extend_from_slice(&(variant.cases.len() as u64).to_le_bytes());
                    for case in &variant.cases {
                        text(out, &case.name);
                        out.push(u8::from(case.is_default));
                        fields(out, &case.payload);
                    }
                }
                InterfaceItem::Enum(enumeration) => {
                    out.push(2);
                    text(out, &enumeration.name);
                    out.extend_from_slice(&(enumeration.cases.len() as u64).to_le_bytes());
                    for case in &enumeration.cases {
                        text(out, &case.name);
                        out.push(u8::from(case.is_default));
                    }
                }
                InterfaceItem::Query(query) => {
                    out.push(3);
                    operation(out, query);
                }
                InterfaceItem::Command(command) => {
                    out.push(4);
                    operation(out, command);
                }
                InterfaceItem::Events(events) => {
                    out.push(5);
                    ty(out, &events.ty);
                    out.push(match events.delivery {
                        DeliveryMode::Broadcast => 0,
                        DeliveryMode::State => 1,
                    });
                }
            }
        }
    }
}

/// Produce exact-contract v1 evidence, or no evidence if an imported schema's
/// transitive dependency is unavailable. Unknown imports must never look compatible.
#[must_use]
pub fn service_contract(schema: &Schema, imports: &ImportMap) -> Vec<u8> {
    let mut by_plugin = std::collections::BTreeMap::new();
    for imported in imports.values() {
        if let Some(previous) =
            by_plugin.insert(imported.schema.plugin.plugin_id.as_str(), &imported.schema)
            && previous != &imported.schema
        {
            return Vec::new();
        }
    }
    let mut closure = std::collections::BTreeMap::new();
    let mut pending = schema
        .imports
        .iter()
        .map(|import| import.plugin_id.as_str())
        .collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if closure.contains_key(id) {
            continue;
        }
        let Some(imported) = imports
            .values()
            .find(|entry| entry.schema.plugin.plugin_id == id)
        else {
            return Vec::new();
        };
        closure.insert(id, &imported.schema);
        pending.extend(
            imported
                .schema
                .imports
                .iter()
                .map(|import| import.plugin_id.as_str()),
        );
    }
    // Resolve every named/qualified nested reference using exactly the same
    // validator as code generation. Never emit evidence for an unresolved AST.
    for current in std::iter::once(schema).chain(closure.values().copied()) {
        let mut resolved = std::collections::BTreeMap::new();
        for import in &current.imports {
            let Some(imported) = by_plugin.get(import.plugin_id.as_str()) else {
                return Vec::new();
            };
            resolved.insert(import.alias.clone(), (**imported).clone());
        }
        if crate::validator::validate_with_imports(current, &resolved).is_err() {
            return Vec::new();
        }
    }
    let mut out = b"bmux.bpdl.exact.v1\0".to_vec();
    schema_bytes(&mut out, schema);
    out.extend_from_slice(&(closure.len() as u64).to_le_bytes());
    for imported in closure.values() {
        schema_bytes(&mut out, imported);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_types_participate_and_missing_transitive_evidence_is_empty() {
        let mut schema = crate::compile(
            "plugin example.api version 1; capability READ = example.read; @capability(READ) interface state { query get() -> unit; }",
        )
        .unwrap();
        schema.imports.push(crate::ast::Import {
            alias: "types".into(),
            plugin_id: "example.types".into(),
            span: crate::Span::new(1, 1),
        });
        assert!(service_contract(&schema, &ImportMap::new()).is_empty());
        let mut imports = ImportMap::new();
        imports.insert("types".into(), crate::codegen_rust::ImportInfo {
            crate_path: "::ignored_rust_path".into(),
            schema: crate::compile("plugin example.types version 1; interface types { record item { value: u32 } }").unwrap(),
        });
        let first = service_contract(&schema, &imports);
        assert!(!first.is_empty());
        imports.get_mut("types").unwrap().crate_path = "::different_rust_path".into();
        assert_eq!(first, service_contract(&schema, &imports));
        imports.get_mut("types").unwrap().schema = crate::compile(
            "plugin example.types version 1; interface types { record item { value: string } }",
        )
        .unwrap();
        assert_ne!(first, service_contract(&schema, &imports));
    }

    #[test]
    fn nested_named_fields_and_imported_variant_payloads_are_exact() {
        let source = "plugin example.api version 1; import foreign = example.types; capability READ = example.read; @capability(READ) interface state { record nested { values: list<map<string, foreign.item?>> } record row { child: nested } query get() -> row; }";
        let provider = crate::compile("plugin example.types version 1; interface types { variant item { value { nested: list<map<string, u32?>> } } }").unwrap();
        let mut imports = ImportMap::new();
        imports.insert(
            "foreign".into(),
            crate::codegen_rust::ImportInfo {
                crate_path: "::types".into(),
                schema: provider.clone(),
            },
        );
        let resolved = std::collections::BTreeMap::from([("foreign".into(), provider)]);
        let schema = crate::compile_with_imports(source, &resolved).unwrap();
        let first = service_contract(&schema, &imports);
        assert!(!first.is_empty());
        let changed_source = source.replace("child: nested", "child: list<nested>");
        let changed = crate::compile_with_imports(&changed_source, &resolved).unwrap();
        assert_ne!(first, service_contract(&changed, &imports));
        imports.get_mut("foreign").unwrap().schema = crate::compile("plugin example.types version 1; interface types { variant item { value { nested: list<map<string, string?>> } } }").unwrap();
        assert_ne!(first, service_contract(&schema, &imports));
        let mut unresolved = schema.clone();
        if let InterfaceItem::Record(record) = &mut unresolved.interfaces[0].items[1] {
            record.fields[0].ty = TypeRef::Named("missing".into());
        } else {
            panic!("expected row record");
        }
        assert!(service_contract(&unresolved, &imports).is_empty());
        imports
            .get_mut("foreign")
            .unwrap()
            .schema
            .imports
            .push(crate::ast::Import {
                alias: "deeper".into(),
                plugin_id: "example.unavailable".into(),
                span: crate::Span::new(1, 1),
            });
        assert!(service_contract(&schema, &imports).is_empty());
    }

    #[test]
    fn formatting_is_not_identity_but_field_types_are() {
        let first = crate::compile("plugin example.api version 1; capability READ = example.read; @capability(READ) interface state { record row { value: u32, } query get() -> row; }").unwrap();
        let reformatted = crate::compile("// comment\nplugin example.api version 1; capability READ = example.read;\n@capability(READ) interface state { record row { value : u32, }\n query get ( ) -> row; }").unwrap();
        let changed = crate::compile("plugin example.api version 1; capability READ = example.read; @capability(READ) interface state { record row { value: string, } query get() -> row; }").unwrap();
        assert_eq!(
            service_contract(&first, &ImportMap::new()),
            service_contract(&reformatted, &ImportMap::new())
        );
        assert_ne!(
            service_contract(&first, &ImportMap::new()),
            service_contract(&changed, &ImportMap::new())
        );
    }
}
