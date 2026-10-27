//! Exact, versioned BPDL wire-contract bytes, independent of spans and source formatting.
//! Interface identities include only reachable wire definitions. Declaration
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
        // `secret` affects Rust diagnostics only, never wire compatibility.
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

/// SHA-256 identity of one interface and only its reachable wire definitions.
///
/// Canonical v2 fixes UTF-8 length-prefixed names, little-endian integers,
/// declaration-ordered fields/variant cases/operations, and the SDK's named
/// `bmux_codec` `TypedStable` service encoding (including envelope v1). These rules are part
/// of the hash domain: changing encoding requires a new canonical version.
/// Missing or ambiguous references produce no compatibility evidence.
#[must_use]
pub fn interface_contract(
    schema: &Schema,
    interface: &crate::ast::Interface,
    imports: &ImportMap,
) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut out =
        b"bmux.bpdl.interface.sha256.v2\0bmux_codec.typed_stable.v1.envelope.v1\0".to_vec();
    if interface
        .capability
        .as_ref()
        .is_some_and(|name| !schema.capabilities.iter().any(|cap| cap.name == *name))
    {
        return Vec::new();
    }
    let mut root = schema.clone();
    root.plugin.version = 0; // Plugin releases do not version individual interfaces.
    root.imports.clear();
    root.capabilities
        .retain(|cap| interface.capability.as_ref() == Some(&cap.name));
    let mut selected = interface.clone();
    selected.items.retain(|item| {
        matches!(
            item,
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_)
        )
    });
    root.interfaces = vec![selected.clone()];
    schema_bytes(&mut out, &root);
    let mut closure = WireClosure {
        imports,
        seen: std::collections::BTreeSet::new(),
        definitions: std::collections::BTreeMap::new(),
    };
    for item in &selected.items {
        if closure.item(schema, interface, item).is_none() {
            return Vec::new();
        }
    }
    for (key, bytes) in closure.definitions {
        text(&mut out, &key.0);
        text(&mut out, &key.1);
        text(&mut out, &key.2);
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    Sha256::digest(&out).to_vec()
}

type WireKey = (String, String, String);

struct WireClosure<'a> {
    imports: &'a ImportMap,
    seen: std::collections::BTreeSet<WireKey>,
    definitions: std::collections::BTreeMap<WireKey, Vec<u8>>,
}

fn item_name(item: &InterfaceItem) -> Option<&str> {
    match item {
        InterfaceItem::Record(value) => Some(&value.name),
        InterfaceItem::Variant(value) => Some(&value.name),
        InterfaceItem::Enum(value) => Some(&value.name),
        _ => None,
    }
}

impl WireClosure<'_> {
    fn item(
        &mut self,
        schema: &Schema,
        iface: &crate::ast::Interface,
        item: &InterfaceItem,
    ) -> Option<()> {
        let types: Vec<&TypeRef> = match item {
            InterfaceItem::Record(value) => value.fields.iter().map(|field| &field.ty).collect(),
            InterfaceItem::Variant(value) => value
                .cases
                .iter()
                .flat_map(|case| case.payload.iter().map(|field| &field.ty))
                .collect(),
            InterfaceItem::Enum(_) => vec![],
            InterfaceItem::Query(value) | InterfaceItem::Command(value) => value
                .params
                .iter()
                .map(|field| &field.ty)
                .chain(std::iter::once(&value.returns))
                .chain(value.emits.iter())
                .collect(),
            InterfaceItem::Events(value) => vec![&value.ty],
        };
        for value in types {
            self.ty(schema, iface, value)?;
        }
        Some(())
    }

    fn ty(
        &mut self,
        schema: &Schema,
        iface: &crate::ast::Interface,
        value: &TypeRef,
    ) -> Option<()> {
        match value {
            TypeRef::Named(name) => self.named(schema, iface, name),
            TypeRef::Qualified { alias, name } => {
                if let Some(import) = schema.imports.iter().find(|import| import.alias == *alias) {
                    let mut providers = self
                        .imports
                        .values()
                        .filter(|entry| entry.schema.plugin.plugin_id == import.plugin_id);
                    let imported = &providers.next()?.schema;
                    if providers.any(|entry| entry.schema != *imported) {
                        return None;
                    }
                    let mut candidates = imported.interfaces.iter().filter(|iface| {
                        iface.items.iter().any(|item| item_name(item) == Some(name))
                    });
                    let target = candidates.next()?;
                    if candidates.next().is_some() {
                        return None;
                    }
                    // Bind the alias to its actual plugin, not the Rust import path.
                    let key = (
                        schema.plugin.plugin_id.clone(),
                        format!("import:{alias}"),
                        name.clone(),
                    );
                    self.definitions
                        .insert(key, import.plugin_id.as_bytes().to_vec());
                    self.named(imported, target, name)
                } else {
                    let mut candidates = schema.interfaces.iter().filter(|iface| {
                        iface.name == *alias
                            && iface.items.iter().any(|item| item_name(item) == Some(name))
                    });
                    let target = candidates.next()?;
                    if candidates.next().is_some() {
                        return None;
                    }
                    self.named(schema, target, name)
                }
            }
            TypeRef::List(inner) | TypeRef::Option(inner) => self.ty(schema, iface, inner),
            TypeRef::Map(first, second) | TypeRef::Result(first, second) => {
                self.ty(schema, iface, first)?;
                self.ty(schema, iface, second)
            }
            TypeRef::Primitive(_) | TypeRef::Unit => Some(()),
        }
    }

    fn named(&mut self, schema: &Schema, iface: &crate::ast::Interface, name: &str) -> Option<()> {
        let key = (
            schema.plugin.plugin_id.clone(),
            format!(
                "{}:{}:{}",
                iface.name,
                u8::from(iface.interface_version.is_some()),
                iface.interface_version.unwrap_or(0)
            ),
            name.to_owned(),
        );
        if !self.seen.insert(key.clone()) {
            return Some(());
        }
        let item = iface
            .items
            .iter()
            .find(|item| item_name(item) == Some(name))?;
        let mut selected = schema.clone();
        selected.plugin.version = 0;
        selected.imports.clear();
        selected.capabilities.clear();
        let mut target = iface.clone();
        target.capability = None;
        target.items = vec![item.clone()];
        selected.interfaces = vec![target];
        let mut bytes = Vec::new();
        schema_bytes(&mut bytes, &selected);
        self.definitions.insert(key, bytes);
        self.item(schema, iface, item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(source: &str) -> Vec<u8> {
        let schema = crate::compile(source).unwrap();
        interface_contract(&schema, &schema.interfaces[0], &ImportMap::new())
    }

    #[test]
    fn scoped_identity_ignores_unreachable_types_interfaces_and_plugin_releases() {
        let first = identity(
            "plugin example.api version 1; capability READ = example.read; @capability(READ) interface read { record used { n: u32 } record unused { n: u32 } query get() -> used; } @capability(READ) interface other { query get() -> u32; }",
        );
        let changed = identity(
            "plugin example.api version 2; capability READ = example.read; @capability(READ) interface read { record unused { n: string } record used { n: u32 } query get() -> used; } @capability(READ) interface other { query different() -> string; }",
        );
        assert_eq!(first.len(), 32);
        assert_eq!(first, changed);
        assert_eq!(
            first,
            identity(
                "plugin example.api version 1; capability READ = example.read; @capability(READ) interface read { record used { n: u32 } query get() -> used; }"
            )
        );
        assert_ne!(
            first,
            identity(
                "plugin example.api version 1; capability READ = example.read; @capability(READ) interface read { record used { n: u64 } query get() -> used; }"
            )
        );
    }

    #[test]
    fn scoped_local_cross_interface_types_are_reachable() {
        let base = "plugin example.api version 1; capability READ = example.read; @capability(READ) interface read { query get() -> types.row; } interface types { record row { nested: value } record value { n: u32 } record ignored { n: string } }";
        let first = identity(base);
        assert_eq!(first.len(), 32);
        assert_eq!(first, identity(&base.replace("n: string", "n: bool")));
        assert_ne!(first, identity(&base.replace("n: u32", "n: u64")));
        let mut schema = crate::compile(base).unwrap();
        if let InterfaceItem::Record(record) = &mut schema.interfaces[1].items[0] {
            record.fields[0].ty = TypeRef::Named("missing".into());
        } else {
            panic!("expected reachable row record");
        }
        assert_eq!(
            interface_contract(&schema, &schema.interfaces[0], &ImportMap::new()),
            [] as [u8; 0]
        );
    }

    #[test]
    fn scoped_identity_binds_version_capability_and_wire_case_order() {
        let base = "plugin example.api version 1; capability READ = example.read; @capability(READ) @interface-version(1) interface read { enum value { a, b } query get() -> value; }";
        let first = identity(base);
        assert_ne!(first, identity(&base.replace("version(1)", "version(2)")));
        assert_ne!(
            first,
            identity(&base.replace("example.read", "example.changed"))
        );
        assert_ne!(first, identity(&base.replace("a, b", "b, a")));
        assert_ne!(first, identity(&base.replace("get()", "get(n: u32)")));
    }

    #[test]
    fn scoped_imports_follow_only_reachable_transitive_types() {
        let source = "plugin example.api version 1; import data = example.data; import unrelated = example.absent; capability READ = example.read; @capability(READ) interface read { query get() -> data.value; }";
        let schema = crate::compile(source).unwrap();
        let imported = crate::compile("plugin example.data version 1; interface types { record value { n: u32 } record ignored { n: string } }").unwrap();
        let mut imports = ImportMap::new();
        imports.insert(
            "data".into(),
            crate::codegen_rust::ImportInfo {
                crate_path: "::some_rust_path".into(),
                schema: imported,
            },
        );
        let first = interface_contract(&schema, &schema.interfaces[0], &imports);
        assert_eq!(first.len(), 32);
        imports.get_mut("data").unwrap().crate_path = "::changed_rust_path".into();
        assert_eq!(
            first,
            interface_contract(&schema, &schema.interfaces[0], &imports)
        );
        imports.get_mut("data").unwrap().schema = crate::compile("plugin example.data version 2; interface types { record ignored { n: bool } record value { n: u32 } }").unwrap();
        assert_eq!(
            first,
            interface_contract(&schema, &schema.interfaces[0], &imports)
        );
        imports.get_mut("data").unwrap().schema = crate::compile(
            "plugin example.data version 2; interface types { record value { n: u64 } }",
        )
        .unwrap();
        assert_ne!(
            first,
            interface_contract(&schema, &schema.interfaces[0], &imports)
        );
        imports.get_mut("data").unwrap().schema = crate::compile("plugin example.data version 2; import nested = example.nested; interface types { record value { n: nested.number } }").unwrap();
        assert_eq!(
            interface_contract(&schema, &schema.interfaces[0], &imports),
            [] as [u8; 0]
        );
        imports.insert(
            "nested".into(),
            crate::codegen_rust::ImportInfo {
                crate_path: "::nested".into(),
                schema: crate::compile(
                    "plugin example.nested version 1; interface types { record number { n: u32 } }",
                )
                .unwrap(),
            },
        );
        let transitive = interface_contract(&schema, &schema.interfaces[0], &imports);
        assert_eq!(transitive.len(), 32);
        imports.get_mut("nested").unwrap().schema = crate::compile(
            "plugin example.nested version 1; interface types { record number { n: u64 } }",
        )
        .unwrap();
        assert_ne!(
            transitive,
            interface_contract(&schema, &schema.interfaces[0], &imports)
        );
        assert_eq!(
            interface_contract(&schema, &schema.interfaces[0], &ImportMap::new()),
            [] as [u8; 0]
        );
    }

    #[test]
    fn formatting_is_not_identity_but_field_types_are() {
        let first = crate::compile("plugin example.api version 1; capability READ = example.read; @capability(READ) interface state { record row { value: u32, } query get() -> row; }").unwrap();
        let reformatted = crate::compile("// comment\nplugin example.api version 1; capability READ = example.read;\n@capability(READ) interface state { record row { value : u32, }\n query get ( ) -> row; }").unwrap();
        let changed = crate::compile("plugin example.api version 1; capability READ = example.read; @capability(READ) interface state { record row { value: string, } query get() -> row; }").unwrap();
        assert_eq!(
            interface_contract(&first, &first.interfaces[0], &ImportMap::new()),
            interface_contract(&reformatted, &reformatted.interfaces[0], &ImportMap::new())
        );
        assert_ne!(
            interface_contract(&first, &first.interfaces[0], &ImportMap::new()),
            interface_contract(&changed, &changed.interfaces[0], &ImportMap::new())
        );
    }
}
