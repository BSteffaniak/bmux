//! Exact, versioned BPDL wire-contract identity, computed from the
//! canonical IR ([`crate::ir`]) independent of spans, documentation,
//! stability metadata and source formatting.
//!
//! Interface identities include only reachable wire definitions. Declaration
//! order is retained because enum/variant order can affect wire encoding. It
//! does not claim structural or additive compatibility. No Rust paths or
//! debug formatting enter it.
//!
//! # Canonical v2 byte scheme
//!
//! The digest is SHA-256 over the domain string
//! `bmux.bpdl.interface.sha256.v2\0bmux_codec.typed_stable.v1.envelope.v1\0`
//! followed by a root package record and the sorted reachable definitions.
//! Strings are `u64` little-endian length-prefixed UTF-8; counts are `u64`
//! LE; versions are `u32` LE; flags are one byte.
//!
//! - package record: `id`, `version` (always 0: plugin releases do not
//!   version interfaces), imports (always empty), capabilities (the guarding
//!   capability only for the root), interfaces (exactly one).
//! - interface: `name`, has-version + version, has-capability + constant
//!   name, items. The root keeps only operations and events; each
//!   reachable definition is hashed as its own one-item package record.
//! - item tags: record 0, variant 1, enum 2, query 3 (`@reentrant` query
//!   6), command 4, events 5, flags 7, resource 8.
//! - type tags: primitive 0 + keyword, local reference 1 + name, scoped
//!   reference 2 + scope alias + name, list 3, map 4, option 5, result 6,
//!   unit 7, tuple 8 + count, stream 9, future 10, `own` 11 + reference,
//!   `borrow` 12 + reference.
//! - reachable definitions are keyed by `(defining package id,
//!   "interface:has-version:version", type name)`; an import alias used by
//!   a package binds `(package id, "import:alias", type)` to the imported
//!   package id.
//!
//! The scheme for every construct that existed before the IR (tags 0–7 of
//! both tables) is byte-for-byte the pre-IR v2 scheme, so every existing
//! contract keeps its identity. Constructs added with the IR use tags that
//! no earlier schema could produce, which is why they extend v2 instead of
//! requiring v3: no previously hashed contract can collide with them.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Interface as AstInterface, Schema};
use crate::codegen_rust::ImportMap;
use crate::ir::{
    self, Delivery, ExtensionPoint, Field, Function, FunctionKind, Interface, Item, MethodKind,
    Package, PackageSet, RefScope, Type, TypeDef, TypeDefKind, TypeRef,
};

const INTERFACE_DOMAIN: &[u8] =
    b"bmux.bpdl.interface.sha256.v2\0bmux_codec.typed_stable.v1.envelope.v1\0";
const EXTENSION_POINT_DOMAIN: &[u8] =
    b"bmux.bpdl.extension-point.sha256.v1\0bmux_codec.typed_stable.v1.envelope.v1\0";

fn text(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn count(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u64).to_le_bytes());
}

fn reference(out: &mut Vec<u8>, value: &TypeRef) {
    match &value.scope {
        RefScope::Local => {
            out.push(1);
            text(out, &value.name);
        }
        RefScope::Interface(alias) | RefScope::Import(alias) => {
            out.push(2);
            text(out, alias);
            text(out, &value.name);
        }
    }
}

fn ty(out: &mut Vec<u8>, value: &Type) {
    match value {
        Type::Primitive(primitive) => {
            out.push(0);
            text(out, primitive.keyword());
        }
        Type::Ref(target) => reference(out, target),
        Type::List(inner) => {
            out.push(3);
            ty(out, inner);
        }
        Type::Map(key, value) => {
            out.push(4);
            ty(out, key);
            ty(out, value);
        }
        Type::Option(inner) => {
            out.push(5);
            ty(out, inner);
        }
        Type::Result(ok, error) => {
            out.push(6);
            ty(out, ok);
            ty(out, error);
        }
        Type::Unit => out.push(7),
        Type::Tuple(items) => {
            out.push(8);
            count(out, items.len());
            for item in items {
                ty(out, item);
            }
        }
        Type::Stream(inner) => {
            out.push(9);
            ty(out, inner);
        }
        Type::Future(inner) => {
            out.push(10);
            ty(out, inner);
        }
        Type::Own(target) => {
            out.push(11);
            reference(out, target);
        }
        Type::Borrow(target) => {
            out.push(12);
            reference(out, target);
        }
    }
}

fn fields(out: &mut Vec<u8>, values: &[Field]) {
    count(out, values.len());
    for field in values {
        // `secret` affects diagnostics only, never wire compatibility.
        text(out, &field.name);
        ty(out, &field.ty);
    }
}

fn function(out: &mut Vec<u8>, value: &Function) {
    text(out, &value.name);
    fields(out, &value.params);
    ty(out, &value.result);
    out.push(u8::from(value.emits.is_some()));
    if let Some(events) = &value.emits {
        ty(out, events);
    }
}

fn item(out: &mut Vec<u8>, value: &Item) {
    match value {
        Item::Type(def) => type_def(out, def),
        Item::Function(op) => {
            // `@reentrant` changes concurrency semantics, so it is
            // identity-bearing when set. It uses a distinct item tag so
            // unmarked queries keep their exact pre-existing bytes.
            out.push(match (op.kind, op.reentrant) {
                (FunctionKind::Query, false) => 3,
                (FunctionKind::Query, true) => 6,
                (FunctionKind::Command, _) => 4,
            });
            function(out, op);
        }
        Item::Events(events) => {
            out.push(5);
            ty(out, &events.ty);
            out.push(match events.delivery {
                Delivery::Broadcast => 0,
                Delivery::State => 1,
            });
        }
    }
}

fn type_def(out: &mut Vec<u8>, def: &TypeDef) {
    match &def.kind {
        TypeDefKind::Record { fields: values } => {
            out.push(0);
            text(out, &def.name);
            fields(out, values);
        }
        TypeDefKind::Variant { cases } => {
            out.push(1);
            text(out, &def.name);
            count(out, cases.len());
            for case in cases {
                text(out, &case.name);
                out.push(u8::from(case.default));
                fields(out, &case.fields);
            }
        }
        TypeDefKind::Enum { cases } => {
            out.push(2);
            text(out, &def.name);
            count(out, cases.len());
            for case in cases {
                text(out, &case.name);
                out.push(u8::from(case.default));
            }
        }
        TypeDefKind::Flags { flags } => {
            out.push(7);
            text(out, &def.name);
            count(out, flags.len());
            for flag in flags {
                text(out, &flag.name);
            }
        }
        TypeDefKind::Resource { methods } => {
            out.push(8);
            text(out, &def.name);
            count(out, methods.len());
            for method in methods {
                out.push(match method.kind {
                    MethodKind::Constructor => 0,
                    MethodKind::Query => 1,
                    MethodKind::Command => 2,
                    MethodKind::StaticQuery => 3,
                    MethodKind::StaticCommand => 4,
                });
                text(out, &method.name);
                fields(out, &method.params);
                out.push(u8::from(method.result.is_some()));
                if let Some(result) = &method.result {
                    ty(out, result);
                }
            }
        }
    }
}

/// One package record holding exactly one interface with `items`.
fn package_record(
    out: &mut Vec<u8>,
    package: &str,
    capability: Option<(&str, &str)>,
    iface: &Interface,
    keep_capability: bool,
    items: &[&Item],
) {
    text(out, package);
    out.extend_from_slice(&0_u32.to_le_bytes()); // plugin releases do not version interfaces
    count(out, 0); // imports are bound per reachable reference instead
    match capability {
        Some((name, id)) => {
            count(out, 1);
            text(out, name);
            text(out, id);
        }
        None => count(out, 0),
    }
    count(out, 1);
    text(out, &iface.name);
    out.push(u8::from(iface.version.is_some()));
    if let Some(version) = iface.version {
        out.extend_from_slice(&version.to_le_bytes());
    }
    let capability = iface.capability.as_ref().filter(|_| keep_capability);
    out.push(u8::from(capability.is_some()));
    if let Some(capability) = capability {
        text(out, capability);
    }
    count(out, items.len());
    for value in items {
        item(out, value);
    }
}

type WireKey = (String, String, String);

struct WireClosure<'a> {
    set: &'a PackageSet,
    seen: BTreeSet<WireKey>,
    definitions: BTreeMap<WireKey, Vec<u8>>,
}

impl<'a> WireClosure<'a> {
    fn new(set: &'a PackageSet) -> Self {
        Self {
            set,
            seen: BTreeSet::new(),
            definitions: BTreeMap::new(),
        }
    }

    fn types(&mut self, package: &'a Package, iface: &'a Interface, value: &Type) -> Option<()> {
        match value {
            Type::Ref(target) | Type::Own(target) | Type::Borrow(target) => {
                self.reference(package, iface, target)
            }
            Type::List(inner) | Type::Option(inner) | Type::Stream(inner) | Type::Future(inner) => {
                self.types(package, iface, inner)
            }
            Type::Map(first, second) | Type::Result(first, second) => {
                self.types(package, iface, first)?;
                self.types(package, iface, second)
            }
            Type::Tuple(items) => {
                for value in items {
                    self.types(package, iface, value)?;
                }
                Some(())
            }
            Type::Primitive(_) | Type::Unit => Some(()),
        }
    }

    fn fields(
        &mut self,
        package: &'a Package,
        iface: &'a Interface,
        values: &[Field],
    ) -> Option<()> {
        for field in values {
            self.types(package, iface, &field.ty)?;
        }
        Some(())
    }

    fn function(
        &mut self,
        package: &'a Package,
        iface: &'a Interface,
        op: &Function,
    ) -> Option<()> {
        self.fields(package, iface, &op.params)?;
        self.types(package, iface, &op.result)?;
        if let Some(emits) = &op.emits {
            self.types(package, iface, emits)?;
        }
        Some(())
    }

    fn item(&mut self, package: &'a Package, iface: &'a Interface, value: &Item) -> Option<()> {
        match value {
            Item::Function(op) => self.function(package, iface, op),
            Item::Events(events) => self.types(package, iface, &events.ty),
            Item::Type(def) => match &def.kind {
                TypeDefKind::Record { fields } => self.fields(package, iface, fields),
                TypeDefKind::Variant { cases } => {
                    for case in cases {
                        self.fields(package, iface, &case.fields)?;
                    }
                    Some(())
                }
                TypeDefKind::Enum { .. } | TypeDefKind::Flags { .. } => Some(()),
                TypeDefKind::Resource { methods } => {
                    for method in methods {
                        self.fields(package, iface, &method.params)?;
                        if let Some(result) = &method.result {
                            self.types(package, iface, result)?;
                        }
                    }
                    Some(())
                }
            },
        }
    }

    fn reference(
        &mut self,
        package: &'a Package,
        iface: &'a Interface,
        target: &TypeRef,
    ) -> Option<()> {
        if let RefScope::Import(alias) = &target.scope {
            // Bind the alias to its actual package, not a language import path.
            let imported = package.import(alias)?;
            self.definitions.insert(
                (
                    package.id.clone(),
                    format!("import:{alias}"),
                    target.name.clone(),
                ),
                imported.as_bytes().to_vec(),
            );
        }
        let (defining, owner, def) = self.set.resolve(package, iface, target)?;
        self.named(defining, owner, def)
    }

    fn named(
        &mut self,
        package: &'a Package,
        iface: &'a Interface,
        def: &'a TypeDef,
    ) -> Option<()> {
        let key = (
            package.id.clone(),
            format!(
                "{}:{}:{}",
                iface.name,
                u8::from(iface.version.is_some()),
                iface.version.unwrap_or(0)
            ),
            def.name.clone(),
        );
        if !self.seen.insert(key.clone()) {
            return Some(());
        }
        let value = Item::Type(def.clone());
        let mut bytes = Vec::new();
        package_record(&mut bytes, &package.id, None, iface, false, &[&value]);
        self.definitions.insert(key, bytes);
        self.item(package, iface, &value)
    }

    fn finish(self, mut out: Vec<u8>) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        for (key, bytes) in self.definitions {
            text(&mut out, &key.0);
            text(&mut out, &key.1);
            text(&mut out, &key.2);
            count(&mut out, bytes.len());
            out.extend_from_slice(&bytes);
        }
        Sha256::digest(&out).to_vec()
    }
}

/// SHA-256 identity of one interface of `package` and only its reachable
/// wire definitions, resolving imports through `set`.
///
/// Missing or ambiguous references produce no compatibility evidence: the
/// result is empty rather than a guess.
#[must_use]
pub fn interface_identity(set: &PackageSet, package: &Package, iface: &Interface) -> Vec<u8> {
    let capability = match &iface.capability {
        Some(name) => match package.capability_id(name) {
            Some(id) => Some((name.as_str(), id)),
            None => return Vec::new(),
        },
        None => None,
    };
    let mut out = INTERFACE_DOMAIN.to_vec();
    let operations: Vec<&Item> = iface
        .items
        .iter()
        .filter(|value| matches!(value, Item::Function(_) | Item::Events(_)))
        .collect();
    package_record(&mut out, &package.id, capability, iface, true, &operations);
    let mut closure = WireClosure::new(set);
    for value in operations {
        if closure.item(package, iface, value).is_none() {
            return Vec::new();
        }
    }
    closure.finish(out)
}

/// SHA-256 identity of an extension point: its data record, its interface
/// operations, and their reachable wire definitions.
#[must_use]
pub fn extension_point_identity(
    set: &PackageSet,
    package: &Package,
    point: &ExtensionPoint,
) -> Vec<u8> {
    let mut out = EXTENSION_POINT_DOMAIN.to_vec();
    text(&mut out, &package.id);
    text(&mut out, &point.name);
    out.extend_from_slice(&point.version.to_le_bytes());
    fields(&mut out, &point.data);
    count(&mut out, point.functions.len());
    for op in &point.functions {
        out.push(match op.kind {
            FunctionKind::Query => 3,
            FunctionKind::Command => 4,
        });
        function(&mut out, op);
    }
    // Extension points define no types; references resolve as if made from
    // an empty interface of the declaring package.
    let scope = Interface {
        name: String::new(),
        version: None,
        contract_major: None,
        capability: None,
        access: None,
        docs: Vec::new(),
        stability: ir::Stability::default(),
        items: Vec::new(),
    };
    let mut closure = WireClosure::new(set);
    if closure.fields(package, &scope, &point.data).is_none() {
        return Vec::new();
    }
    for op in &point.functions {
        if closure.function(package, &scope, op).is_none() {
            return Vec::new();
        }
    }
    closure.finish(out)
}

/// Lower `schema` and every schema in `imports` into one [`PackageSet`].
#[must_use]
pub fn package_set(schema: &Schema, imports: &ImportMap) -> (Package, PackageSet) {
    let mut set = PackageSet::new();
    for info in imports.values() {
        set.insert(ir::lower(&info.schema));
    }
    (ir::lower(schema), set)
}

/// SHA-256 identity of one interface of a parsed schema.
///
/// Compatibility entry point for AST-based callers (the Rust generator):
/// lowers `schema` and `imports` into the IR and delegates to
/// [`interface_identity`].
#[must_use]
pub fn interface_contract(schema: &Schema, iface: &AstInterface, imports: &ImportMap) -> Vec<u8> {
    let Some(index) = schema
        .interfaces
        .iter()
        .position(|candidate| std::ptr::eq(candidate, iface))
        .or_else(|| {
            schema
                .interfaces
                .iter()
                .position(|candidate| candidate == iface)
        })
    else {
        return Vec::new();
    };
    let (package, set) = package_set(schema, imports);
    interface_identity(&set, &package, &package.interfaces[index])
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
        if let crate::ast::InterfaceItem::Record(record) = &mut schema.interfaces[1].items[0] {
            record.fields[0].ty = crate::ast::TypeRef::Named("missing".into());
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
    fn reentrant_marker_is_identity_bearing_only_when_set() {
        let unmarked = "plugin example.api version 1; capability READ = example.read; @capability(READ) @interface-version(1) interface read { enum value { a, b } query get() -> value; query other(n: u32) -> u32; }";
        let first = identity(unmarked);
        // Pinned digest of the unmarked interface: adding the `@reentrant`
        // grammar must not perturb any existing contract identity.
        assert_eq!(
            hex(&first),
            "685926e8a746fabdf1a81b7a36993435f307395b012fc840a040b2cf10cb2d52"
        );
        let marked = identity(&unmarked.replace("query get()", "@reentrant query get()"));
        assert_eq!(marked.len(), 32);
        assert_ne!(first, marked);
        assert_ne!(
            marked,
            identity(&unmarked.replace("query other(", "@reentrant query other("))
        );
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
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
