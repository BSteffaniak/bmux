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
//! - `CapabilityId`, `InterfaceId`, `OperationId`, and event-kind
//!   constants for schema-declared surfaces.
//!
//! Qualified type references (`<alias>.<type>`) are resolved against a
//! caller-provided [`ImportMap`], which maps each alias to the Rust
//! crate path where the imported bindings live.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::ast::{
    CapabilityDecl, DeliveryMode, EnumDef, Field, Interface, InterfaceItem, Operation, Primitive,
    RecordDef, Schema, TypeRef, VariantCase, VariantDef,
};

/// Resolution table used by codegen to turn qualified BPDL type
/// references (`windows.pane-state`) into Rust paths
/// (`::bmux_tabs_plugin_api::tabs_state::PaneState`).
///
/// Keys are the import aliases declared in the schema's `import`
/// directives; values are the [`ImportInfo`] describing the target crate.
pub type ImportMap = BTreeMap<String, ImportInfo>;

type OwnTypeMap = BTreeMap<String, BTreeSet<String>>;
type NonEqTypeSet = BTreeSet<String>;

/// Resolution target for a single import alias.
#[derive(Debug, Clone)]
pub struct ImportInfo {
    /// Rust crate path the generated code should prefix onto imported
    /// type references, e.g. `::bmux_tabs_plugin_api`.
    pub crate_path: String,
    /// The imported plugin's parsed schema. Used to find which
    /// interface a qualified type belongs to (so the emitted path
    /// includes the right submodule).
    pub schema: Schema,
}

fn interface_module_counts(schema: &Schema) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for iface in &schema.interfaces {
        *counts.entry(iface.name.as_str()).or_default() += 1;
    }
    counts
}

fn interface_module_name(iface: &Interface, duplicate_name: bool) -> String {
    let base = snake_case(&iface.name);
    if duplicate_name {
        format!(
            "{base}_v{}",
            iface
                .interface_version
                .expect("validated duplicate interface names require explicit versions")
        )
    } else {
        base
    }
}

/// Rust module path segment generated for an IR interface: the snake-case
/// name, suffixed `_vN` when the package declares a versioned family under
/// that name. Matches the modules [`emit_with_imports`] produces.
#[must_use]
pub fn interface_module(package: &crate::ir::Package, iface: &crate::ir::Interface) -> String {
    let duplicate = package
        .interfaces
        .iter()
        .filter(|candidate| candidate.name == iface.name)
        .count()
        > 1;
    match (duplicate, iface.version) {
        (true, Some(version)) => format!("{}_v{version}", snake_case(&iface.name)),
        _ => snake_case(&iface.name),
    }
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
    let own_types = own_type_map(schema);
    let non_eq_types = non_eq_type_set(schema);
    let module_counts = interface_module_counts(schema);
    out.push_str("// AUTO-GENERATED FROM BPDL. DO NOT EDIT BY HAND.\n\n");
    emit_capabilities(&schema.capabilities, &mut out);
    for iface in &schema.interfaces {
        emit_interface(
            schema,
            iface,
            module_counts.get(iface.name.as_str()).copied().unwrap_or(0) > 1,
            imports,
            &own_types,
            &non_eq_types,
            &mut out,
        );
    }
    emit_versioned_interface_aliases(schema, &module_counts, &mut out);
    emit_schema_service_declarations(schema, imports, &mut out);
    out
}

fn emit_versioned_interface_aliases(
    schema: &Schema,
    module_counts: &BTreeMap<&str, usize>,
    out: &mut String,
) {
    for iface in &schema.interfaces {
        if module_counts.get(iface.name.as_str()).copied().unwrap_or(0) > 1
            && iface.interface_version == Some(1)
        {
            let base = snake_case(&iface.name);
            let versioned = interface_module_name(iface, true);
            let _ = writeln!(
                out,
                "/// Source-compatible alias for the immutable v1 interface.\npub use {versioned} as {base};\n"
            );
        }
    }
}

fn non_eq_type_set(schema: &Schema) -> NonEqTypeSet {
    let mut non_eq = BTreeSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for iface in &schema.interfaces {
            for item in &iface.items {
                let (name, has_non_eq) = match item {
                    InterfaceItem::Record(record) => (
                        &record.name,
                        record
                            .fields
                            .iter()
                            .any(|field| type_contains_non_eq(&field.ty, &non_eq)),
                    ),
                    InterfaceItem::Variant(variant) => (
                        &variant.name,
                        variant.cases.iter().any(|case| {
                            case.payload
                                .iter()
                                .any(|field| type_contains_non_eq(&field.ty, &non_eq))
                        }),
                    ),
                    InterfaceItem::Enum(_)
                    | InterfaceItem::Flags(_)
                    | InterfaceItem::Resource(_)
                    | InterfaceItem::Query(_)
                    | InterfaceItem::Command(_)
                    | InterfaceItem::Events(_) => continue,
                };
                if has_non_eq && non_eq.insert(name.clone()) {
                    changed = true;
                }
            }
        }
    }
    non_eq
}

fn type_contains_non_eq(ty: &TypeRef, non_eq_types: &NonEqTypeSet) -> bool {
    match ty {
        // Handles and async values have no Rust binding yet; see
        // [`unsupported_type`].
        TypeRef::Primitive(Primitive::F32 | Primitive::F64)
        | TypeRef::Stream(_)
        | TypeRef::Future(_)
        | TypeRef::Own(_)
        | TypeRef::Borrow(_) => true,
        TypeRef::Primitive(_) | TypeRef::Qualified { .. } | TypeRef::Unit => false,
        TypeRef::Named(name) => non_eq_types.contains(name),
        TypeRef::Option(inner) | TypeRef::List(inner) => type_contains_non_eq(inner, non_eq_types),
        TypeRef::Map(key, value) | TypeRef::Result(key, value) => {
            type_contains_non_eq(key, non_eq_types) || type_contains_non_eq(value, non_eq_types)
        }
        TypeRef::Tuple(items) => items
            .iter()
            .any(|item| type_contains_non_eq(item, non_eq_types)),
    }
}

fn own_type_map(schema: &Schema) -> OwnTypeMap {
    schema
        .interfaces
        .iter()
        .map(|iface| {
            let types = iface
                .items
                .iter()
                .filter_map(crate::validator::defined_type_name)
                .cloned()
                .collect::<BTreeSet<_>>();
            (iface.name.clone(), types)
        })
        .collect()
}

fn emit_capabilities(capabilities: &[CapabilityDecl], out: &mut String) {
    if capabilities.is_empty() {
        return;
    }

    out.push_str("/// Capability identifiers declared by this plugin schema.\n");
    out.push_str("pub mod capabilities {\n");
    for capability in capabilities {
        let _ = writeln!(
            out,
            "    /// Capability id `{}`.\n    pub const {}: ::bmux_plugin_sdk::CapabilityId = ::bmux_plugin_sdk::CapabilityId::from_static(\"{}\");\n",
            capability.id, capability.name, capability.id,
        );
    }
    out.push_str("}\n\n");
}

fn emit_interface(
    schema: &Schema,
    iface: &Interface,
    duplicate_name: bool,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    non_eq_types: &NonEqTypeSet,
    out: &mut String,
) {
    let plugin_id = &schema.plugin.plugin_id;
    let module_name = interface_module_name(iface, duplicate_name);
    let _ = writeln!(out, "pub mod {module_name} {{");
    let contract = crate::canonical::interface_contract(schema, iface, imports);
    let _ = writeln!(
        out,
        "    /// SHA-256 of canonical interface wire contract v2.\n    pub const SERVICE_CONTRACT: &[u8] = &{contract:?};\n"
    );

    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => emit_record(r, imports, own_types, non_eq_types, out),
            InterfaceItem::Variant(v) => emit_variant(v, imports, own_types, non_eq_types, out),
            InterfaceItem::Enum(e) => emit_enum(e, out),
            InterfaceItem::Flags(f) => emit_flags(f, out),
            InterfaceItem::Resource(r) => {
                let message = format!(
                    "BPDL resource `{}` has no Rust binding yet: resource handles need the host handle table",
                    r.name
                );
                let _ = writeln!(out, "    ::core::compile_error!({message:?});");
            }
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_) => {}
        }
    }

    // Service trait contains queries + commands. Events are exposed
    // separately as a typed `EVENT_KIND` constant + payload type
    // alias below.
    emit_interface_service_descriptor(iface, out);
    emit_service_trait(iface, imports, own_types, out);
    emit_transport_client(iface, imports, own_types, out);

    // If this interface declares `events <type>`, emit a canonical
    // `PluginEventKind` constant plus a `EventPayload` type alias so
    // both producers and subscribers import from the same place.
    emit_event_bindings(plugin_id, iface, imports, own_types, out);

    out.push_str("}\n\n");
}

/// Never inspect secret values while formatting diagnostics.
fn emit_redacted_debug(name: &str, fields: &[Field], out: &mut String) {
    let _ = writeln!(out, "impl ::core::fmt::Debug for {name} {{");
    out.push_str("fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n");
    let _ = writeln!(out, "let mut debug = f.debug_struct({name:?});");
    for field in fields {
        let member = snake_case(&field.name);
        if field.secret {
            let _ = writeln!(out, "debug.field({member:?}, &\"[REDACTED]\");");
        } else {
            let _ = writeln!(out, "debug.field({member:?}, &self.{member});");
        }
    }
    out.push_str("debug.finish()\n}\n}\n");
}

fn emit_record(
    r: &RecordDef,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    non_eq_types: &NonEqTypeSet,
    out: &mut String,
) {
    let name = pascal_case(&r.name);
    let secret = r.fields.iter().any(|field| field.secret);
    let debug = if secret { "" } else { "Debug, " };
    emit_docs(&r.docs, "    ", out);
    if non_eq_types.contains(&r.name) {
        let _ = writeln!(
            out,
            "    #[derive({debug}Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]"
        );
    } else {
        let _ = writeln!(
            out,
            "    #[derive({debug}Clone, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]"
        );
    }
    let _ = writeln!(out, "    pub struct {name} {{");
    for f in &r.fields {
        let field_name = snake_case(&f.name);
        let ty = rust_type(&f.ty, imports, own_types);
        // Emit `#[serde(default)]` on fields whose Rust type carries a
        // sensible Default impl. This lets additively-added fields
        // round-trip old payloads: a missing TOML/JSON key parses as
        // the type's default value instead of rejecting the whole
        // record. Option<T> already defaults to None without the
        // attribute, but adding it is harmless.
        if type_has_default(&f.ty) {
            out.push_str("        #[serde(default)]\n");
        }
        if let Some(adapter) = serde_bytes_adapter(&f.ty) {
            let _ = writeln!(out, "        #[serde(with = \"{adapter}\")]");
        }
        emit_docs(&f.docs, "        ", out);
        let _ = writeln!(out, "        pub {field_name}: {ty},");
    }
    out.push_str("    }\n\n");
    if secret {
        emit_redacted_debug(&name, &r.fields, out);
    }
}

/// Best-effort check: does the type we'd emit for `ty` derive
/// [`Default`]? Used by [`emit_record`] to decide whether to add
/// `#[serde(default)]`. Types that definitely have a `Default`:
/// primitives, `Vec`, `Option`, `BTreeMap`. User-defined
/// record/variant references don't necessarily derive `Default`, so we
/// conservatively skip them.
fn type_has_default(ty: &crate::ast::TypeRef) -> bool {
    use crate::ast::TypeRef;
    match ty {
        TypeRef::Primitive(_) | TypeRef::List(_) | TypeRef::Map(_, _) | TypeRef::Option(_) => true,
        TypeRef::Named(_)
        | TypeRef::Qualified { .. }
        | TypeRef::Result(_, _)
        | TypeRef::Unit
        | TypeRef::Tuple(_)
        | TypeRef::Stream(_)
        | TypeRef::Future(_)
        | TypeRef::Own(_)
        | TypeRef::Borrow(_) => false,
    }
}

/// Emit a `flags` type as a transparent `u64` bit set: flag `i` is bit `i`
/// in declaration order, so the wire form is one integer.
fn emit_flags(flags: &crate::ast::FlagsDef, out: &mut String) {
    let name = pascal_case(&flags.name);
    emit_docs(&flags.docs, "    ", out);
    out.push_str("    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, ::serde::Serialize, ::serde::Deserialize)]
");
    out.push_str(
        "    #[serde(transparent)]
",
    );
    let _ = writeln!(
        out,
        "    pub struct {name}(u64);
"
    );
    let _ = writeln!(out, "    impl {name} {{");
    for (bit, flag) in flags.flags.iter().enumerate() {
        emit_docs(&flag.docs, "        ", out);
        let _ = writeln!(
            out,
            "        pub const {}: Self = Self(1 << {bit});",
            snake_case(&flag.name).to_uppercase()
        );
    }
    let all = flags
        .flags
        .iter()
        .map(|flag| format!("Self::{}.0", snake_case(&flag.name).to_uppercase()))
        .collect::<Vec<_>>()
        .join(" | ");
    out.push_str(
        "        /// No flags set.
        #[must_use] pub const fn empty() -> Self { Self(0) }
",
    );
    let _ = writeln!(
        out,
        "        /// Every declared flag.
        #[must_use] pub const fn all() -> Self {{ Self({all}) }}"
    );
    out.push_str(
        "        /// Raw bits; undeclared bits are never set by these constructors.
        #[must_use] pub const fn bits(self) -> u64 { self.0 }
",
    );
    out.push_str("        /// Keep only declared flags from raw bits.
        #[must_use] pub const fn from_bits_truncate(bits: u64) -> Self { Self(bits & Self::all().0) }
");
    out.push_str(
        "        /// Whether every flag in `other` is set.
        #[must_use] pub const fn contains(self, other: Self) -> bool { self.0 & other.0 == other.0 }
",
    );
    out.push_str(
        "        /// Flags set in either value.
        #[must_use] pub const fn union(self, other: Self) -> Self { Self(self.0 | other.0) }
",
    );
    out.push_str(
        "    }

",
    );
}

/// Emit `///` documentation lines at `indent`.
fn emit_docs(docs: &[String], indent: &str, out: &mut String) {
    for line in docs {
        if line.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

fn emit_variant(
    v: &VariantDef,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    non_eq_types: &NonEqTypeSet,
    out: &mut String,
) {
    let name = pascal_case(&v.name);
    emit_docs(&v.docs, "    ", out);
    if non_eq_types.contains(&v.name) {
        out.push_str(
            "    #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]\n",
        );
    } else {
        out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, ::serde::Serialize, ::serde::Deserialize)]\n");
    }
    // External (default) tagging. Internally-tagged variants
    // (`#[serde(tag = ...)]`) require `deserialize_any`, which the
    // non-self-describing `bmux_codec` cannot implement. External
    // tagging serializes the variant discriminant as a length-
    // prefixed key for struct/tuple variants and works uniformly
    // across codec and JSON encodings.
    out.push_str("    #[serde(rename_all = \"snake_case\")]\n");
    let _ = writeln!(out, "    pub enum {name} {{");
    for c in &v.cases {
        emit_variant_case(c, imports, own_types, out);
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

fn emit_variant_case(
    case: &VariantCase,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let case_name = pascal_case(&case.name);
    emit_docs(&case.docs, "        ", out);
    if case.payload.is_empty() {
        let _ = writeln!(out, "        {case_name},");
    } else {
        let _ = writeln!(out, "        {case_name} {{");
        for f in &case.payload {
            let field_name = snake_case(&f.name);
            let ty = rust_type(&f.ty, imports, own_types);
            if let Some(adapter) = serde_bytes_adapter(&f.ty) {
                let _ = writeln!(out, "            #[serde(with = \"{adapter}\")]");
            }
            let _ = writeln!(out, "            {field_name}: {ty},");
        }
        out.push_str("        },\n");
    }
}

fn emit_enum(e: &EnumDef, out: &mut String) {
    let name = pascal_case(&e.name);
    emit_docs(&e.docs, "    ", out);
    out.push_str("    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ::serde::Serialize, ::serde::Deserialize)]\n");
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

fn emit_schema_service_declarations(schema: &Schema, imports: &ImportMap, out: &mut String) {
    let module_counts = interface_module_counts(schema);
    let modules = schema
        .interfaces
        .iter()
        .filter(|iface| iface.capability.is_some() && interface_service_kind(iface).is_some())
        .map(|iface| {
            interface_module_name(
                iface,
                module_counts.get(iface.name.as_str()).copied().unwrap_or(0) > 1,
            )
        })
        .collect::<Vec<_>>();

    out.push_str("/// Type-level BPDL contract for this plugin API crate.\n");
    out.push_str("pub struct Contract;\n\n");
    out.push_str("impl Contract {\n");
    out.push_str(
        "    /// Exact modeled service evidence for static and negotiated native declarations.\n",
    );
    out.push_str("    #[must_use]\n");
    out.push_str("    pub const fn service_contract_descriptors() -> &'static [::bmux_plugin_sdk::typed_dispatch::ModeledServiceContractDescriptor] {\n");
    out.push_str("        &[\n");
    for iface in &schema.interfaces {
        // Unresolved imported wire types have no compatibility evidence. Keep
        // their legacy routing declarations, but never advertise negotiation.
        if crate::canonical::interface_contract(schema, iface, imports).is_empty() {
            continue;
        }
        let Some(capability_name) = iface.capability.as_deref() else {
            continue;
        };
        let Some(kind) = interface_service_kind(iface) else {
            continue;
        };
        let Some(capability) = schema
            .capabilities
            .iter()
            .find(|capability| capability.name == capability_name)
        else {
            continue;
        };
        let module = interface_module_name(
            iface,
            module_counts.get(iface.name.as_str()).copied().unwrap_or(0) > 1,
        );
        let interface = iface.interface_version.map_or_else(
            || iface.name.clone(),
            |version| format!("{}/v{version}", iface.name),
        );
        let _ = writeln!(
            out,
            "            ::bmux_plugin_sdk::typed_dispatch::ModeledServiceContractDescriptor {{ capability: {:?}, kind: ::bmux_plugin_sdk::ServiceKind::{kind}, interface_id: {interface:?}, contract: {module}::SERVICE_CONTRACT }},",
            capability.id
        );
    }
    out.push_str("        ]\n    }\n}\n\n");
    out.push_str("impl ::bmux_plugin_sdk::PluginContract for Contract {\n");
    out.push_str("    fn service_contract_descriptors() -> &'static [::bmux_plugin_sdk::typed_dispatch::ModeledServiceContractDescriptor] { Self::service_contract_descriptors() }\n");
    out.push_str("    fn service_contract_for(interface_id: &str) -> Option<&'static [u8]> {\n");
    for module in &modules {
        let _ = writeln!(
            out,
            "        if interface_id == {module}::INTERFACE_ID.as_str() && !{module}::SERVICE_CONTRACT.is_empty() {{ return Some({module}::SERVICE_CONTRACT); }}"
        );
    }
    out.push_str("        None\n    }\n");
    out.push_str("    fn service_declarations() -> ::bmux_plugin_sdk::Result<Vec<::bmux_plugin_sdk::PluginService>> {\n");
    out.push_str("        service_declarations()\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");

    out.push_str("/// Service declarations generated from this BPDL schema.\n");
    out.push_str("///\n");
    out.push_str("/// Every service interface this schema models. Plugin manifests route only\n");
    out.push_str("/// the subset a package world provides; see `bmux_plugin_schema::manifest`.\n");
    out.push_str("///\n");
    out.push_str("/// # Errors\n");
    out.push_str("///\n");
    out.push_str(
        "/// Returns if any generated capability id cannot be represented as a host scope.\n",
    );
    out.push_str("pub fn service_declarations() -> ::bmux_plugin_sdk::Result<Vec<::bmux_plugin_sdk::PluginService>> {\n");
    out.push_str("    Ok(vec![\n");
    for module in modules {
        let _ = writeln!(out, "        {module}::service_declaration()?,");
    }
    out.push_str("    ])\n");
    out.push_str("}\n\n");
}

fn emit_interface_service_descriptor(iface: &Interface, out: &mut String) {
    let Some(capability) = iface.capability.as_deref() else {
        return;
    };
    let Some(kind) = interface_service_kind(iface) else {
        return;
    };
    let reentrant = iface
        .items
        .iter()
        .filter_map(|item| match item {
            InterfaceItem::Query(op) if op.reentrant => Some(format!("{:?}", op.name)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(
        "    /// Query operations this interface declares `@reentrant` in BPDL.\n    ///\n    /// The host may admit these while another callback entry of the same\n    /// provider instance is active on the call chain; every other operation\n    /// keeps the default same-instance re-entry rejection.\n",
    );
    let _ = writeln!(
        out,
        "    pub const REENTRANT_OPERATIONS: &[&str] = &[{reentrant}];\n"
    );
    let _ = writeln!(
        out,
        "    /// BPDL-generated service descriptor for this interface.\n    pub const SERVICE_DESCRIPTOR: ::bmux_plugin_sdk::ServiceInterfaceDescriptor = ::bmux_plugin_sdk::ServiceInterfaceDescriptor {{\n        capability: super::capabilities::{capability},\n        kind: ::bmux_plugin_sdk::ServiceKind::{kind},\n        interface_id: INTERFACE_ID,\n        contract: SERVICE_CONTRACT,\n        reentrant_operations: REENTRANT_OPERATIONS,\n    }};\n"
    );
    out.push_str(
        "    /// Convert this BPDL interface descriptor into a plugin service declaration.\n",
    );
    out.push_str("    ///\n");
    out.push_str("    /// # Errors\n");
    out.push_str("    ///\n");
    out.push_str(
        "    /// Returns if the generated capability id cannot be represented as a host scope.\n",
    );
    out.push_str("    pub fn service_declaration() -> ::bmux_plugin_sdk::Result<::bmux_plugin_sdk::PluginService> {\n");
    out.push_str("        SERVICE_DESCRIPTOR.to_plugin_service()\n");
    out.push_str("    }\n\n");
}

fn interface_service_kind(iface: &Interface) -> Option<&'static str> {
    let mut kind = None;
    for item in &iface.items {
        match item {
            InterfaceItem::Query(_) => kind = Some("Query"),
            InterfaceItem::Command(_) => kind = Some("Command"),
            InterfaceItem::Events(_) => kind = Some("Event"),
            InterfaceItem::Record(_)
            | InterfaceItem::Variant(_)
            | InterfaceItem::Enum(_)
            | InterfaceItem::Flags(_)
            | InterfaceItem::Resource(_) => {}
        }
    }
    kind
}

fn emit_service_trait(
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let trait_name = format!("{}Service", pascal_case(&iface.name));
    // Canonical interface identifier used to look up a typed service via
    // the plugin host registry. `@interface-version(N)` appends `/vN` to
    // the source name. A unique interface keeps the unsuffixed Rust module;
    // duplicate explicitly versioned service families emit `_vN` modules.
    let interface_id = iface.interface_version.map_or_else(
        || iface.name.clone(),
        |version| format!("{}/v{version}", iface.name),
    );
    let _ = writeln!(
        out,
        "    /// Canonical identifier for this interface; used to look up a provider via\n    /// the plugin host registry.\n    pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"{interface_id}\");\n",
    );
    emit_operation_constants(iface, out);
    out.push_str("    /// Service trait for this interface.\n");
    out.push_str("    ///\n");
    out.push_str("    /// Consumers call through a `&dyn` reference; providers `impl`\n");
    out.push_str("    /// this trait on their plugin type. Returned futures are\n");
    out.push_str("    /// `Pin<Box<dyn Future + Send>>` to keep the trait object-safe.\n");
    let _ = writeln!(out, "    pub trait {trait_name}: Send + Sync {{");
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            emit_operation_signature(op, imports, own_types, out);
        }
    }
    out.push_str("    }\n\n");

    emit_service_client(iface, imports, own_types, out, &trait_name);
    emit_provider_adapter(iface, imports, own_types, out, &trait_name);
}

/// Emit one checked byte adapter over the existing typed service object. No
/// provider-specific transport implementation or duplicate operation model.
fn emit_provider_adapter(
    iface: &Interface,
    _imports: &ImportMap,
    _own_types: &OwnTypeMap,
    out: &mut String,
    trait_name: &str,
) {
    if iface.capability.is_none() || interface_service_kind(iface).is_none() {
        return;
    }
    let adapter = format!("{}ProviderAdapter", pascal_case(&iface.name));
    let _ = writeln!(
        out,
        "    pub struct {adapter} {{ inner: ::std::sync::Arc<dyn {trait_name} + Send + Sync> }}"
    );
    let _ = writeln!(out, "    impl {adapter} {{");
    let _ = writeln!(
        out,
        "        #[must_use] pub fn new(inner: ::std::sync::Arc<dyn {trait_name} + Send + Sync>) -> Self {{ Self {{ inner }} }}"
    );
    out.push_str("    }\n");
    let _ = writeln!(
        out,
        "    impl ::bmux_plugin_sdk::typed_dispatch::PortableServiceProvider for {adapter} {{"
    );
    out.push_str("        fn descriptor(&self) -> ::bmux_plugin_sdk::ServiceInterfaceDescriptor { SERVICE_DESCRIPTOR }\n");
    let streaming = iface
        .items
        .iter()
        .filter_map(|item| match item {
            InterfaceItem::Command(op) | InterfaceItem::Query(op) if op.emits.is_some() => {
                Some(format!("\"{}\"", op.name))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !streaming.is_empty() {
        let _ = writeln!(
            out,
            "        fn supports_streaming(&self, operation: &str) -> bool {{ matches!(operation, {}) }}",
            streaming.join(" | ")
        );
    }
    out.push_str("        fn dispatch_checked<'a>(&'a self, operation: &'a str, payload: &'a [u8], stream: Option<::bmux_plugin_sdk::typed_dispatch::ServiceStreamContext>) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = ::bmux_plugin_sdk::Result<Vec<u8>>> + Send + 'a>> {\n");
    out.push_str("            Box::pin(async move {\n");
    out.push_str("                let payload = ::bmux_plugin_sdk::service_contract::decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, payload)?;\n");
    out.push_str("                match operation {\n");
    for item in &iface.items {
        let (InterfaceItem::Query(op) | InterfaceItem::Command(op)) = item else {
            continue;
        };
        let name = snake_case(&op.name);
        let request_ty = transport_request_type(op);
        let request_ty = if op.params.is_empty() {
            request_ty
        } else {
            format!("client::{request_ty}")
        };
        let _ = writeln!(out, "                    \"{}\" => {{", op.name);
        if op.emits.is_some() {
            out.push_str("                        let stream = stream.ok_or_else(|| ::bmux_plugin_sdk::PluginError::ServiceProtocol { details: \"streaming operation requires an event sink\".to_string() })?;\n");
            out.push_str("                        let (stream, completion) = stream.scoped();\n");
            out.push_str("                        if stream.cancellation.is_cancelled() { return Err(::bmux_plugin_sdk::PluginError::ServiceProtocol { details: \"service stream cancelled\".to_string() }); }\n");
        } else {
            out.push_str("                        if stream.is_some() { return Err(::bmux_plugin_sdk::PluginError::ServiceProtocol { details: \"unary operation does not accept streaming\".to_string() }); }\n");
        }
        let binding = if op.params.is_empty() {
            "_request"
        } else {
            "request"
        };
        let _ = writeln!(
            out,
            "                        let {binding}: {request_ty} = ::bmux_plugin_sdk::decode_service_message(payload)?;"
        );
        let mut args = op
            .params
            .iter()
            .map(|p| format!("request.{}", snake_case(&p.name)))
            .collect::<Vec<_>>();
        if op.emits.is_some() {
            args.push("stream.typed_sink()".to_string());
            args.push("stream.cancellation.clone()".to_string());
        }
        let _ = writeln!(
            out,
            "                        let response = self.inner.{name}({}).await;",
            args.join(", ")
        );
        if op.emits.is_some() {
            out.push_str("                        completion.finish()?;\n");
            out.push_str("                        if stream.cancellation.is_cancelled() { return Err(::bmux_plugin_sdk::PluginError::ServiceProtocol { details: \"service stream cancelled\".to_string() }); }\n");
        }
        out.push_str("                        Ok(::bmux_plugin_sdk::service_contract::encode_response(&::bmux_plugin_sdk::encode_service_message(&response)?))\n");
        out.push_str("                    }\n");
    }
    out.push_str("                    _ => Err(::bmux_plugin_sdk::PluginError::ServiceProtocol { details: format!(\"unknown service operation: {operation}\") }),\n");
    out.push_str("                }\n            })\n        }\n    }\n\n");
}

fn emit_operation_constants(iface: &Interface, out: &mut String) {
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            let const_name = operation_const_name(&op.name);
            let _ = writeln!(
                out,
                "    /// Canonical operation identifier for `{}`.\n    pub const {}: ::bmux_plugin_sdk::OperationId = ::bmux_plugin_sdk::OperationId::from_static(\"{}\");\n",
                op.name, const_name, op.name,
            );
        }
    }
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
fn emit_event_bindings(
    plugin_id: &str,
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let Some(decl) = iface.items.iter().find_map(|item| match item {
        InterfaceItem::Events(decl) => Some(decl),
        _ => None,
    }) else {
        return;
    };
    let kind_literal = format!("{plugin_id}/{}", iface.name);
    let ty = rust_type(&decl.ty, imports, own_types);
    match decl.delivery {
        DeliveryMode::Broadcast => {
            let _ = writeln!(
                out,
                "    /// Canonical [`bmux_plugin_sdk::PluginEventKind`] for this\n    /// interface's event stream. Publishers and subscribers both\n    /// reference this constant; the underlying wire value is\n    /// `\"{kind_literal}\"`.\n    pub const EVENT_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"{kind_literal}\");\n"
            );
            let _ = writeln!(
                out,
                "    /// Payload type published on this interface's event stream.\n    pub type EventPayload = {ty};\n"
            );
        }
        DeliveryMode::State => {
            let _ = writeln!(
                out,
                "    /// Canonical [`bmux_plugin_sdk::PluginEventKind`] for this\n    /// interface's state channel. Publishers call\n    /// `EventBus::publish_state`; subscribers call\n    /// `EventBus::subscribe_state` and receive the current value\n    /// synchronously followed by a live-update receiver. The\n    /// underlying wire value is `\"{kind_literal}\"`.\n    pub const STATE_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"{kind_literal}\");\n"
            );
            let _ = writeln!(
                out,
                "    /// Payload type published on this interface's state channel.\n    pub type StatePayload = {ty};\n"
            );
        }
    }
}

fn emit_service_client(
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
    trait_name: &str,
) {
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

    if iface.capability.is_some() && interface_service_kind(iface).is_some() {
        let _ = writeln!(
            out,
            "    /// Register a provider for this interface using the BPDL-generated service descriptor.\n    ///\n    /// # Errors\n    ///\n    /// Returns if the generated capability id cannot be represented as a host scope.\n    pub fn register_provider(registry: &mut ::bmux_plugin_sdk::TypedServiceRegistry, provider: ::std::sync::Arc<dyn {trait_name} + Send + Sync>) -> ::bmux_plugin_sdk::Result<()> {{\n        registry.insert_typed_provider(SERVICE_DESCRIPTOR, provider.clone(), ::std::sync::Arc::new({}ProviderAdapter::new(provider)))\n    }}\n",
            pascal_case(&iface.name)
        );
    }

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
            emit_client_forwarder(op, imports, own_types, out);
        }
    }

    out.push_str("    }\n\n");
}

fn emit_transport_client(
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let Some(capability) = iface.capability.as_deref() else {
        return;
    };
    let operations = iface
        .items
        .iter()
        .filter_map(|item| match item {
            InterfaceItem::Query(op) => Some((op, "Query")),
            InterfaceItem::Command(op) => Some((op, "Command")),
            InterfaceItem::Record(_)
            | InterfaceItem::Variant(_)
            | InterfaceItem::Enum(_)
            | InterfaceItem::Flags(_)
            | InterfaceItem::Resource(_)
            | InterfaceItem::Events(_) => None,
        })
        .collect::<Vec<_>>();
    if operations.is_empty() {
        return;
    }

    out.push_str("    /// Generated transport clients for this interface.\n");
    out.push_str("    pub mod client {\n");
    for (op, kind) in operations {
        emit_transport_endpoint(iface, op, kind, capability, imports, own_types, out);
        emit_transport_client_function(op, imports, own_types, out);
        emit_dispatch_client_function(iface, op, out);
    }
    out.push_str("    }\n\n");
}

fn emit_dispatch_client_function(iface: &Interface, op: &Operation, out: &mut String) {
    let name = snake_case(&op.name);
    let endpoint = format!("{}Endpoint", pascal_case(&op.name));
    let request = transport_request_type(op);
    let service = format!("{}Service", pascal_case(&iface.name));
    let stream_param = if op.emits.is_some() {
        ", stream: ::bmux_plugin_sdk::typed_dispatch::ServiceStreamContext"
    } else {
        ""
    };
    let stream = if op.emits.is_some() {
        "Some(stream)"
    } else {
        "None"
    };
    out.push_str("        /// Dispatch an admitted operation through native typed or checked portable transport.\n");
    out.push_str("        ///\n        /// The caller must apply the same authorization, provenance and deadlines on either target.\n");
    out.push_str("        ///\n        /// # Errors\n        ///\n        /// Returns contract mismatch, unavailable provider, stream negotiation, cancellation, event delivery or codec failures.\n");
    let _ = writeln!(
        out,
        "        pub async fn {name}_dispatch(target: ::bmux_plugin_sdk::typed_dispatch::ServiceDispatchTarget<'_>, request: {request}{stream_param}) -> ::bmux_plugin_sdk::Result<<{endpoint} as ::bmux_plugin_sdk::TypedServiceEndpoint>::Response> {{"
    );
    let _ = writeln!(
        out,
        "            ::bmux_plugin_sdk::typed_dispatch::dispatch_service::<{endpoint}, dyn super::{service} + Send + Sync, _, _>(target, request, {stream}, |provider, request, stream| async move {{"
    );
    let mut args = op
        .params
        .iter()
        .map(|field| format!("request.{}", snake_case(&field.name)))
        .collect::<Vec<_>>();
    if op.params.is_empty() {
        out.push_str("                let _ = request;\n");
    }
    if op.emits.is_some() {
        out.push_str("                let stream = stream.ok_or_else(|| ::bmux_plugin_sdk::PluginError::ServiceProtocol { details: \"streaming operation requires an event sink\".to_string() })?;\n");
        args.push("stream.typed_sink()".to_string());
        args.push("stream.cancellation.clone()".to_string());
    } else {
        out.push_str("                let _ = stream;\n");
    }
    let _ = writeln!(
        out,
        "                Ok(provider.{name}({}).await)",
        args.join(", ")
    );
    out.push_str("            }).await\n        }\n\n");
}

fn emit_transport_endpoint(
    _iface: &Interface,
    op: &Operation,
    kind: &str,
    capability: &str,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let endpoint_name = format!("{}Endpoint", pascal_case(&op.name));
    let request_ty = transport_request_type(op);
    let returns = client_rust_type(&op.returns, imports, own_types);
    if let Some(emits) = &op.emits {
        let event_ty = client_rust_type(emits, imports, own_types);
        let event_sink_name = format!("{}EventSink", pascal_case(&op.name));
        out.push_str("        /// Request-scoped event sink for streaming operation frames.\n");
        let _ = writeln!(
            out,
            "        pub type {event_sink_name} = ::bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<{event_ty}>;"
        );
        let _ = writeln!(
            out,
            "        /// Event payload type emitted by `{}`.",
            op.name
        );
        let _ = writeln!(
            out,
            "        pub type {}Event = {event_ty};",
            pascal_case(&op.name)
        );
    }
    if !op.params.is_empty() {
        let request_name = request_ty.clone();
        let secret = op.params.iter().any(|field| field.secret);
        let debug = if secret { "" } else { "Debug, " };
        let _ = writeln!(
            out,
            "        #[derive({debug}Clone, ::serde::Serialize, ::serde::Deserialize)]"
        );
        let _ = writeln!(out, "        pub struct {request_name} {{");
        for param in &op.params {
            let field_name = snake_case(&param.name);
            let ty = client_rust_type(&param.ty, imports, own_types);
            if let Some(adapter) = serde_bytes_adapter(&param.ty) {
                let _ = writeln!(out, "            #[serde(with = \"{adapter}\")]");
            }
            let _ = writeln!(out, "            pub {field_name}: {ty},");
        }
        out.push_str("        }\n\n");
        if secret {
            emit_redacted_debug(&request_ty, &op.params, out);
        }
    }

    let _ = writeln!(out, "        /// Typed endpoint marker for `{}`.", op.name);
    let _ = writeln!(out, "        pub struct {endpoint_name};");
    let _ = writeln!(
        out,
        "        impl ::bmux_plugin_sdk::TypedServiceEndpoint for {endpoint_name} {{"
    );
    let _ = writeln!(out, "            type Request = {request_ty};");
    let _ = writeln!(out, "            type Response = {returns};");
    let _ = writeln!(
        out,
        "            const CAPABILITY: ::bmux_plugin_sdk::CapabilityId = super::super::capabilities::{capability};"
    );
    let _ = writeln!(
        out,
        "            const KIND: ::bmux_ipc::InvokeServiceKind = ::bmux_ipc::InvokeServiceKind::{kind};"
    );
    let _ = writeln!(
        out,
        "            const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = super::INTERFACE_ID;"
    );
    out.push_str("            const SERVICE_CONTRACT: &'static [u8] = super::SERVICE_CONTRACT;\n");
    let _ = writeln!(
        out,
        "            const OPERATION: ::bmux_plugin_sdk::OperationId = super::{};",
        operation_const_name(&op.name)
    );
    if op.emits.is_some() {
        out.push_str("            const STREAMING: bool = true;\n");
    }
    out.push_str("        }\n");
    if let Some(emits) = &op.emits {
        let event_ty = client_rust_type(emits, imports, own_types);
        let _ = writeln!(
            out,
            "        impl ::bmux_plugin_sdk::StreamingTypedServiceEndpoint for {endpoint_name} {{"
        );
        let _ = writeln!(out, "            type Event = {event_ty};");
        out.push_str("        }\n");
    }
    out.push('\n');
}

fn emit_transport_client_function(
    op: &Operation,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let name = snake_case(&op.name);
    let endpoint_name = format!("{}Endpoint", pascal_case(&op.name));
    let params = op
        .params
        .iter()
        .map(|f| {
            format!(
                "{}: {}",
                snake_case(&f.name),
                client_rust_type(&f.ty, imports, own_types)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sep = if params.is_empty() { "" } else { ", " };
    let returns = client_rust_type(&op.returns, imports, own_types);
    if op.emits.is_some() {
        let _ = writeln!(
            out,
            "        /// Invoke `{}` as a request-scoped streaming operation.",
            op.name
        );
        out.push_str("        ///\n        /// The provider may emit zero or more typed event frames before the final response; cancellation and bounded backpressure are carried by the invocation transport.\n\n");
    }
    let request_expr = if op.params.is_empty() {
        "()".to_string()
    } else {
        let fields = op
            .params
            .iter()
            .map(|f| snake_case(&f.name))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}Request {{ {fields} }}", pascal_case(&op.name))
    };
    let _ = writeln!(
        out,
        "        /// Invoke `{}` through a typed dispatch client.",
        op.name
    );
    out.push_str("        ///\n        /// # Errors\n        ///\n        /// Returns encoding, transport, compatibility, or response-decoding failures.\n");
    if op.params.len() >= 7 {
        out.push_str("        #[allow(clippy::too_many_arguments)]\n");
    }
    let stream_params = op.emits.as_ref().map_or_else(String::new, |emits| format!(
        ", events: ::bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<{}>, cancellation: ::bmux_plugin_sdk::ServiceCancellation",
        client_rust_type(emits, imports, own_types)
    ));
    let _ = writeln!(
        out,
        "        pub async fn {name}<C: ::bmux_plugin_sdk::TypedDispatchClient>(client: &mut C{sep}{params}{stream_params}) -> ::bmux_plugin_sdk::TypedServiceClientResult<{returns}> {{"
    );
    let _ = writeln!(out, "            let request = {request_expr};");
    let invoke_fn = if op.emits.is_some() {
        "invoke_typed_streaming_service"
    } else {
        "invoke_typed_service"
    };
    let stream_args = if op.emits.is_some() {
        ", events, cancellation"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "            ::bmux_plugin_sdk::{invoke_fn}::<C, {endpoint_name}>(client, &request{stream_args}).await"
    );
    out.push_str("        }\n\n");
}

fn transport_request_type(op: &Operation) -> String {
    if op.params.is_empty() {
        "()".to_string()
    } else {
        format!("{}Request", pascal_case(&op.name))
    }
}

fn emit_client_forwarder(
    op: &Operation,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let name = snake_case(&op.name);
    let params = op
        .params
        .iter()
        .map(|f: &Field| {
            format!(
                "{}: {}",
                snake_case(&f.name),
                rust_type(&f.ty, imports, own_types)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let returns = rust_type(&op.returns, imports, own_types);
    let sep = if op.params.is_empty() { "" } else { ", " };
    let arg_names = op
        .params
        .iter()
        .map(|f| snake_case(&f.name))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str("\n        /// Forward to the provider's trait method.\n");
    out.push_str("        #[must_use = \"futures do nothing unless awaited or polled\"]\n");
    if op.params.len() >= 7 {
        out.push_str("        #[allow(clippy::too_many_arguments)]\n");
    }
    let stream_params = op.emits.as_ref().map_or_else(String::new, |emits| format!(
        ", events: ::bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<{}>, cancellation: ::bmux_plugin_sdk::ServiceCancellation",
        rust_type(emits, imports, own_types)
    ));
    let stream_args = if op.emits.is_some() {
        if arg_names.is_empty() {
            "events, cancellation"
        } else {
            ", events, cancellation"
        }
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "        pub fn {name}<'a>(&'a self{sep}{params}{stream_params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>> {{",
    );
    let _ = writeln!(
        out,
        "            self.inner.{name}({arg_names}{stream_args})"
    );
    out.push_str("        }\n");
}

fn emit_operation_signature(
    op: &Operation,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let name = snake_case(&op.name);
    let params = op
        .params
        .iter()
        .map(|f: &Field| {
            format!(
                "{}: {}",
                snake_case(&f.name),
                rust_type(&f.ty, imports, own_types)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let returns = rust_type(&op.returns, imports, own_types);
    let sep = if op.params.is_empty() { "" } else { ", " };
    if op.params.len() >= 7 {
        out.push_str("        #[allow(clippy::too_many_arguments)]\n");
    }
    let stream_params = op.emits.as_ref().map_or_else(String::new, |emits| format!(
        ", events: ::bmux_plugin_sdk::typed_dispatch::TypedServiceEventSink<{}>, cancellation: ::bmux_plugin_sdk::ServiceCancellation",
        rust_type(emits, imports, own_types)
    ));
    let _ = writeln!(
        out,
        "        fn {name}<'a>(&'a self{sep}{params}{stream_params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>>;"
    );
}

fn rust_type(ty: &TypeRef, imports: &ImportMap, own_types: &OwnTypeMap) -> String {
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
            Primitive::Char => "char".to_string(),
        },
        TypeRef::Named(name) => pascal_case(name),
        TypeRef::Qualified { alias, name } => resolve_qualified(alias, name, imports, own_types),
        TypeRef::Option(inner) => format!("Option<{}>", rust_type(inner, imports, own_types)),
        TypeRef::List(inner) => format!("Vec<{}>", rust_type(inner, imports, own_types)),
        TypeRef::Map(key, value) => {
            format!(
                "::std::collections::BTreeMap<{}, {}>",
                rust_type(key, imports, own_types),
                rust_type(value, imports, own_types)
            )
        }
        TypeRef::Result(ok, err) => {
            format!(
                "::std::result::Result<{}, {}>",
                rust_type(ok, imports, own_types),
                rust_type(err, imports, own_types)
            )
        }
        TypeRef::Unit => "()".to_string(),
        TypeRef::Tuple(items) => tuple_type(items, |item| rust_type(item, imports, own_types)),
        TypeRef::Stream(_) | TypeRef::Future(_) | TypeRef::Own(_) | TypeRef::Borrow(_) => {
            unsupported_type(ty)
        }
    }
}

fn tuple_type(items: &[TypeRef], render: impl Fn(&TypeRef) -> String) -> String {
    let rendered = items.iter().map(render).collect::<Vec<_>>();
    if rendered.len() == 1 {
        format!("({},)", rendered[0])
    } else {
        format!("({})", rendered.join(", "))
    }
}

/// Handles and asynchronous values need the host handle table and stream
/// transport, which the Rust tier does not provide yet. A `compile_error!`
/// in type position fails the consuming build with an explicit reason
/// instead of silently degrading the contract.
fn unsupported_type(ty: &TypeRef) -> String {
    let construct = match ty {
        TypeRef::Stream(_) => "stream<T>",
        TypeRef::Future(_) => "future<T>",
        TypeRef::Own(_) => "own<R>",
        _ => "borrow<R>",
    };
    format!(
        "::core::compile_error!({:?})",
        format!("BPDL `{construct}` has no Rust binding yet; it lowers to WIT only")
    )
}

// Transport functions are nested one module below their interface types.
fn client_rust_type(ty: &TypeRef, imports: &ImportMap, own_types: &OwnTypeMap) -> String {
    match ty {
        TypeRef::Named(name) => format!("super::{}", pascal_case(name)),
        TypeRef::Qualified { .. } => {
            let path = rust_type(ty, imports, own_types);
            path.strip_prefix("super::").map_or_else(
                || path.clone(),
                |relative| format!("super::super::{relative}"),
            )
        }
        TypeRef::List(inner) => format!("Vec<{}>", client_rust_type(inner, imports, own_types)),
        TypeRef::Map(key, value) => format!(
            "::std::collections::BTreeMap<{}, {}>",
            client_rust_type(key, imports, own_types),
            client_rust_type(value, imports, own_types)
        ),
        TypeRef::Option(inner) => {
            format!("Option<{}>", client_rust_type(inner, imports, own_types))
        }
        TypeRef::Result(ok, error) => format!(
            "::std::result::Result<{}, {}>",
            client_rust_type(ok, imports, own_types),
            client_rust_type(error, imports, own_types)
        ),
        TypeRef::Tuple(items) => {
            tuple_type(items, |item| client_rust_type(item, imports, own_types))
        }
        TypeRef::Primitive(_)
        | TypeRef::Unit
        | TypeRef::Stream(_)
        | TypeRef::Future(_)
        | TypeRef::Own(_)
        | TypeRef::Borrow(_) => rust_type(ty, imports, own_types),
    }
}

fn serde_bytes_adapter(ty: &TypeRef) -> Option<&'static str> {
    match ty {
        TypeRef::Primitive(Primitive::Bytes) => Some("bmux_codec::serde_bytes_vec"),
        TypeRef::List(inner) if matches!(inner.as_ref(), TypeRef::Primitive(Primitive::U8)) => {
            Some("bmux_codec::serde_bytes_vec")
        }
        TypeRef::Option(inner) if serde_bytes_adapter(inner).is_some() => {
            Some("bmux_codec::serde_bytes_vec::option")
        }
        _ => None,
    }
}

/// Resolve `alias.type-name` to a concrete Rust path by consulting the
/// imports table first, then same-schema interface names. If the alias
/// is unknown at codegen time we emit a
/// `::bmux_plugin_schema_unresolved::<alias>::<type>` path that will
/// trigger an obvious compile error; normal validated schemas never hit
/// this branch because the validator requires declared aliases or
/// same-schema interfaces.
fn resolve_qualified(
    alias: &str,
    name: &str,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
) -> String {
    let Some(info) = imports.get(alias) else {
        if own_types
            .get(alias)
            .is_some_and(|types| types.contains(name))
        {
            return format!("super::{}::{}", snake_case(alias), pascal_case(name));
        }
        return format!(
            "::bmux_plugin_schema_unresolved::{}::{}",
            snake_case(alias),
            pascal_case(name)
        );
    };
    // Locate the interface in the imported schema that defines `name`.
    for iface in &info.schema.interfaces {
        for item in &iface.items {
            let Some(defined) = crate::validator::defined_type_name(item) else {
                continue;
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

fn operation_const_name(s: &str) -> String {
    format!("OP_{}", snake_case(s).to_ascii_uppercase())
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
    fn secret_fields_redact_records_and_requests_without_changing_contract() {
        let source = "plugin example version 1; capability SEND = example.send; @capability(SEND) interface send { record payload { public: string, @secret credential: bytes } command send(@secret credential: bytes, payload: payload) -> payload; }";
        let schema = compile(source).unwrap();
        let plain = compile(&source.replace("@secret ", "")).unwrap();
        let generated = emit(&schema);
        assert!(generated.contains("impl ::core::fmt::Debug for Payload"));
        assert!(generated.contains("impl ::core::fmt::Debug for SendRequest"));
        assert!(generated.contains("debug.field(\"credential\", &\"[REDACTED]\")"));
        assert!(generated.contains("debug.field(\"public\", &self.public)"));
        assert!(!generated.contains("&self.credential"));
        assert_ne!(
            crate::canonical::interface_contract(&schema, &schema.interfaces[0], &ImportMap::new()),
            [] as [u8; 0]
        );
        assert_eq!(
            crate::canonical::interface_contract(&schema, &schema.interfaces[0], &ImportMap::new()),
            crate::canonical::interface_contract(&plain, &plain.interfaces[0], &ImportMap::new())
        );
    }

    #[test]
    fn secret_annotations_reject_unsupported_or_ambiguous_positions() {
        for source in [
            "plugin example version 1; interface types { record payload { @unknown value: string } }",
            "plugin example version 1; interface types { record payload { @secret @secret value: string } }",
            "plugin example version 1; interface types { variant payload { value { @secret value: string } } }",
            "plugin example version 1; interface types { @secret record payload { value: string } }",
            "plugin example version 1; interface types { enum payload { @secret value } }",
            "plugin example version 1; capability SEND = example.send; @capability(SEND) interface send { @secret command send(value: string) -> string; }",
            "plugin example version 1; capability SEND = example.send; @capability(SEND) interface send { command send(@unknown value: string) -> string; }",
        ] {
            assert!(compile(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn generated_clients_are_explicit_and_document_fallibility() {
        let schema = compile("plugin p version 1; capability READ = p.read; interface shared { record row { n: u32 } } @capability(READ) interface read { record local { n: u32 } query get(value: local, other: shared.row) -> result<local, string>; }").unwrap();
        let rust = emit(&schema);
        assert!(!rust.contains("use super::*"));
        assert!(!rust.contains("use serde::"));
        assert!(rust.contains("::serde::Serialize, ::serde::Deserialize"));
        assert!(rust.contains("pub value: super::Local"));
        assert!(rust.contains("pub other: super::super::shared::Row"));
        assert!(rust.contains("type Response = ::std::result::Result<super::Local, String>"));
        assert!(rust.contains("/// # Errors\n        ///\n        /// Returns encoding, transport, compatibility, or response-decoding failures."));
        assert!(rust.contains("#[must_use]\n        pub fn new("));
        assert!(rust.contains("#[must_use]\n        pub fn as_service("));
        assert!(rust.contains(
            "#[must_use = \"futures do nothing unless awaited or polled\"]\n        pub fn get<'a>("
        ));
    }

    #[test]
    fn generated_endpoints_and_provider_lookup_use_interface_identity() {
        let schema = compile("plugin p version 1; capability READ = p.read; @capability(READ) interface first { query get() -> u32; } @capability(READ) interface second { query get() -> string; }").unwrap();
        let rust = emit(&schema);
        assert_eq!(rust.matches("pub const SERVICE_CONTRACT: &[u8]").count(), 2);
        assert!(rust.contains("const SERVICE_CONTRACT: &'static [u8] = super::SERVICE_CONTRACT;"));
        assert!(rust.contains("interface_id == first::INTERFACE_ID.as_str()"));
        assert!(rust.contains("return Some(first::SERVICE_CONTRACT)"));
        assert!(rust.contains("interface_id == second::INTERFACE_ID.as_str()"));
        assert!(!rust.contains("super::super::SERVICE_CONTRACT"));
        assert!(!rust.contains("fn service_contract()"));
    }

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
    fn emits_request_scoped_streaming_operation_metadata() {
        let src = "plugin p version 1;\n\
                   capability I_RUN = bmux.i.run;\n\
                   @capability(I_RUN)\n\
                   interface i {\n\
                     record start { prompt: string }\n\
                     record finish { text: string }\n\
                     variant turn-event { delta { text: string }, done }\n\
                     command start-turn(request: start) -> finish emits turn-event;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub type StartTurnEvent = super::TurnEvent;"));
        assert!(rust.contains("type Event = super::TurnEvent;"));
        assert!(rust.contains("type Response = super::Finish;"));
        assert!(rust.contains("pub request: super::Start"));
        assert!(rust.contains(
            "impl ::bmux_plugin_sdk::StreamingTypedServiceEndpoint for StartTurnEndpoint"
        ));
        assert!(
            rust.contains(
                "::bmux_plugin_sdk::invoke_typed_streaming_service::<C, StartTurnEndpoint>"
            )
        );
        assert!(rust.contains("Request-scoped event sink"));
        assert!(rust.contains("zero or more typed event frames"));
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
    fn emits_byte_buffer_adapter_for_raw_byte_fields() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record raw-payload { data: bytes, legacy: list<u8>, maybe: bytes? }\n\
                     variant event { frame { data: list<u8> } }\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "#[serde(with = \"bmux_codec::serde_bytes_vec\")]\n        pub data: Vec<u8>"
            ),
            "bytes fields should use the codec byte-buffer adapter; got: {rust}"
        );
        assert!(
            rust.contains(
                "#[serde(with = \"bmux_codec::serde_bytes_vec\")]\n        pub legacy: Vec<u8>"
            ),
            "list<u8> fields should use the codec byte-buffer adapter; got: {rust}"
        );
        assert!(
            rust.contains("#[serde(with = \"bmux_codec::serde_bytes_vec::option\")]\n        pub maybe: Option<Vec<u8>>"),
            "optional bytes fields should use the optional byte-buffer adapter; got: {rust}"
        );
        assert!(
            rust.contains(
                "#[serde(with = \"bmux_codec::serde_bytes_vec\")]\n            data: Vec<u8>"
            ),
            "variant list<u8> payloads should use the codec byte-buffer adapter; got: {rust}"
        );
    }

    #[test]
    fn emits_service_trait_with_queries_and_commands() {
        let src = "plugin p version 1;\n\
                   capability TABS_READ = bmux.tabs.read;\n\
                   capability TABS_WRITE = bmux.tabs.write;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
                      record pane-state { id: uuid }\n\
                      query pane-state(id: uuid) -> pane-state?;\n\
                   }\n\
                   @capability(TABS_WRITE)\n\
                   interface tabs-commands {\n\
                      command focus-pane(id: uuid) -> result<unit, string>;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub trait TabsStateService"));
        assert!(rust.contains("pub trait TabsCommandsService"));
        assert!(rust.contains("fn pane_state"));
        assert!(rust.contains("fn focus_pane"));
        assert!(rust.contains("Option<PaneState>"));
        assert!(rust.contains("::std::result::Result<(), String>"));
        assert!(rust.contains("pub const SERVICE_DESCRIPTOR"));
        assert!(rust.contains("pub fn service_declarations()"));
    }

    #[test]
    fn service_descriptor_exposes_reentrant_query_set() {
        let src = "plugin p version 1;\n\
                   capability READ = bmux.p.read;\n\
                   @capability(READ)\n\
                   interface state {\n\
                      @reentrant query provenance() -> u32;\n\
                      query plain() -> u32;\n\
                      @reentrant query other-thing(id: uuid) -> bool;\n\
                   }\n\
                   @capability(READ)\n\
                   interface unmarked {\n\
                      query get() -> u32;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const REENTRANT_OPERATIONS: &[&str] = &[\"provenance\", \"other-thing\"];"
            ),
            "reentrant set must list exactly the marked queries; got: {rust}"
        );
        assert!(rust.contains("pub const REENTRANT_OPERATIONS: &[&str] = &[];"));
        assert!(rust.contains("reentrant_operations: REENTRANT_OPERATIONS,"));
    }

    #[test]
    fn emits_service_client_with_forwarders() {
        let src = "plugin p version 1;\n\
                   capability TABS_READ = bmux.tabs.read;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
                      record pane-state { id: uuid }\n\
                      query pane-state(id: uuid) -> pane-state?;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub struct TabsStateClient"),
            "client wrapper not emitted; got: {rust}"
        );
        assert!(
            rust.contains("inner: ::std::sync::Arc<dyn TabsStateService + Send + Sync>"),
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
            rust.contains("pub fn register_provider("),
            "client module should expose descriptor-backed provider registration; got: {rust}"
        );
    }

    #[test]
    fn emits_transport_client_with_endpoint_metadata() {
        let src = "plugin p version 1;\n\
                   capability TABS_READ = bmux.tabs.read;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
                     record pane-state { id: uuid }\n\
                     query pane-state(id: uuid) -> pane-state?;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub mod client"),
            "transport client module should be emitted; got: {rust}"
        );
        assert!(
            rust.contains("pub struct PaneStateEndpoint"),
            "endpoint marker should be emitted; got: {rust}"
        );
        assert!(
            rust.contains("const CAPABILITY: ::bmux_plugin_sdk::CapabilityId = super::super::capabilities::TABS_READ;"),
            "endpoint should carry explicit capability; got: {rust}"
        );
        assert!(
            rust.contains(
                "const KIND: ::bmux_ipc::InvokeServiceKind = ::bmux_ipc::InvokeServiceKind::Query;"
            ),
            "endpoint should carry query kind; got: {rust}"
        );
        assert!(
            rust.contains("pub async fn pane_state<C: ::bmux_plugin_sdk::TypedDispatchClient>"),
            "transport client function should be emitted; got: {rust}"
        );
        assert!(
            rust.contains("SERVICE_DESCRIPTOR.to_plugin_service()"),
            "interface should expose manifest-compatible service declaration conversion; got: {rust}"
        );
    }

    #[test]
    fn emits_one_checked_provider_adapter_and_real_stream_parameters() {
        let schema = compile("plugin example version 1; capability WRITE = example.write; @capability(WRITE) @interface-version(2) interface stream { command run(value: u32) -> u32 emits u32; }").expect("valid streaming schema");
        let rust = emit(&schema);
        assert!(rust.contains("pub struct StreamProviderAdapter"));
        assert!(rust.contains("PortableServiceProvider for StreamProviderAdapter"));
        assert!(rust.contains("decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, payload)?"));
        assert!(rust.contains("streaming operation requires an event sink"));
        assert!(rust.contains("let response = self.inner.run(request.value, stream.typed_sink(), stream.cancellation.clone()).await"));
        assert!(rust.contains("completion.finish()?"));
        assert!(rust.contains("TypedServiceEventSink<u32>"));
        assert!(rust.contains("const STREAMING: bool = true"));
        assert!(rust.contains("fn supports_streaming(&self, operation: &str)"));
        assert!(rust.contains("pub async fn run_dispatch(target:"));
        assert!(
            rust.contains("dispatch_service::<RunEndpoint, dyn super::StreamService + Send + Sync")
        );
        assert!(
            rust.contains("registry.insert_typed_provider(SERVICE_DESCRIPTOR, provider.clone()")
        );
        assert!(rust.contains("client, &request, events, cancellation"));
        assert!(!rust.contains("noop()"));
    }

    #[test]
    fn emits_interface_id_const() {
        let src = "plugin p version 1;\n\
                   capability TABS_READ = bmux.tabs.read;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
                      query ping() -> bool;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"tabs-state\");"
            ),
            "codegen must emit the canonical interface id as a typed const; got: {rust}"
        );
    }

    #[test]
    fn emits_capability_constants() {
        let src = "plugin bmux.foo version 1;\n\
                   capability FOO_READ = bmux.foo.read;\n\
                   capability FOO_WRITE = bmux.foo.write;\n\
                   @capability(FOO_READ)\n\
                   interface foo-state { query ping() -> bool; }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub mod capabilities"),
            "codegen must emit capabilities module; got: {rust}"
        );
        assert!(
            rust.contains("pub const FOO_READ: ::bmux_plugin_sdk::CapabilityId = ::bmux_plugin_sdk::CapabilityId::from_static(\"bmux.foo.read\");"),
            "codegen must emit typed capability constants; got: {rust}"
        );
        assert!(
            rust.contains("pub const FOO_WRITE: ::bmux_plugin_sdk::CapabilityId = ::bmux_plugin_sdk::CapabilityId::from_static(\"bmux.foo.write\");"),
            "codegen must emit every declared capability; got: {rust}"
        );
    }

    #[test]
    fn emits_operation_id_constants() {
        let src = "plugin p version 1;\n\
                   capability TABS_READ = bmux.tabs.read;\n\
                   capability TABS_WRITE = bmux.tabs.write;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
                      query list-panes(session: uuid) -> unit;\n\
                   }\n\
                   @capability(TABS_WRITE)\n\
                   interface tabs-commands {\n\
                      command focus-pane(id: uuid) -> unit;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub const OP_LIST_PANES: ::bmux_plugin_sdk::OperationId = ::bmux_plugin_sdk::OperationId::from_static(\"list-panes\");"),
            "codegen must emit query operation id constants; got: {rust}"
        );
        assert!(
            rust.contains("pub const OP_FOCUS_PANE: ::bmux_plugin_sdk::OperationId = ::bmux_plugin_sdk::OperationId::from_static(\"focus-pane\");"),
            "codegen must emit command operation id constants; got: {rust}"
        );
    }

    #[test]
    fn unversioned_hyphenated_route_keeps_its_exact_generated_identity() {
        let schema = compile(
            "plugin example version 1; capability READ = example.read; \
             @capability(READ) interface example-state { \
             record reply { value: u32 } query get() -> reply; }",
        )
        .expect("valid neutral contract");
        let interface = schema
            .interfaces
            .iter()
            .find(|interface| interface.name == "example-state")
            .expect("modeled example interface");
        assert_eq!(interface.interface_version, None);
        let evidence = crate::canonical::interface_contract(&schema, interface, &ImportMap::new());
        assert_eq!(evidence.len(), 32);
        let rust = emit(&schema);
        assert!(rust.contains(
            "interface_id: \"example-state\", contract: example_state::SERVICE_CONTRACT"
        ));
        assert!(rust.contains("interface_id == example_state::INTERFACE_ID.as_str() && !example_state::SERVICE_CONTRACT.is_empty()"));
        assert!(rust.contains("InterfaceId::from_static(\"example-state\")"));
        assert!(
            rust.contains(
                "const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = super::INTERFACE_ID;"
            )
        );
        assert!(rust.contains("decode_for(INTERFACE_ID.as_str(), SERVICE_CONTRACT, payload)?"));
        assert!(rust.contains(
            "dispatch_service::<GetEndpoint, dyn super::ExampleStateService + Send + Sync"
        ));
        assert!(!rust.contains("interface_id: \"example-state/v1\", contract:"));
    }

    #[test]
    fn modeled_contract_exports_preserve_versions_and_exclude_unmodeled_interfaces() {
        // A type-only interface is valid without a capability; callable legacy
        // aliases are host metadata, not undeclared BPDL service interfaces.
        let schema = compile("plugin example version 1; capability READ = example.read; @capability(READ) @interface-version(1) interface state { query get() -> u32; } @capability(READ) @interface-version(2) interface state { query get() -> u64; } interface legacy-alias { record metadata { value: u32 } }").expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("impl Contract {"));
        assert!(rust.contains("pub const fn service_contract_descriptors()"));
        assert!(rust.contains("{ Self::service_contract_descriptors() }"));
        assert!(!rust.contains("{ Contract::service_contract_descriptors() }"));
        assert!(rust.contains("fn service_contract_descriptors() -> &'static [::bmux_plugin_sdk::typed_dispatch::ModeledServiceContractDescriptor]"));
        assert!(rust.contains("interface_id: \"state/v1\", contract: state_v1::SERVICE_CONTRACT"));
        assert!(rust.contains("interface_id: \"state/v2\", contract: state_v2::SERVICE_CONTRACT"));
        assert!(rust.contains("pub mod legacy_alias"));
        assert!(rust.contains("pub struct Metadata"));
        assert!(rust.contains("pub use state_v1 as state;"));
        assert!(!rust.contains("interface_id: \"legacy-alias\", contract:"));
        assert!(!rust.contains("interface_id: \"state\", contract:"));
        assert!(!rust.contains("MODELED_SERVICE_CONTRACT"));
    }

    #[test]
    fn emits_distinct_modules_for_versioned_service_family() {
        let src = "plugin p version 1;\n\
                   capability RUN = bmux.test.run;\n\
                   @capability(RUN) @interface-version(1)\n\
                   interface actions { command run() -> unit; }\n\
                   @capability(RUN) @interface-version(2)\n\
                   interface actions { command run() -> unit; command stop() -> unit; }";
        let schema = compile(src).expect("valid versioned family");
        let rust = emit(&schema);
        assert!(rust.contains("pub mod actions_v1"));
        assert!(rust.contains("pub mod actions_v2"));
        assert!(rust.contains("pub use actions_v1 as actions;"));
        assert!(rust.contains("InterfaceId::from_static(\"actions/v1\")"));
        assert!(rust.contains("InterfaceId::from_static(\"actions/v2\")"));
        assert!(rust.contains("actions_v1::service_declaration()?"));
        assert!(rust.contains("actions_v2::service_declaration()?"));
    }

    #[test]
    fn emits_targeted_clippy_allow_for_wide_generated_operations() {
        let src = "plugin p version 1;\n\
                   capability RUN = bmux.test.run;\n\
                   @capability(RUN)\n\
                   interface actions {\n\
                     command run(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> unit;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert_eq!(
            rust.matches("#[allow(clippy::too_many_arguments)]").count(),
            3,
            "service trait, typed forwarder, and transport client should each carry a targeted allow; got: {rust}"
        );
    }

    #[test]
    fn emits_versioned_interface_id_without_changing_module_name() {
        let src = "plugin p version 1;\n\
                   capability CLUSTER_READ = bmux.server_clusters.read;\n\
                   @capability(CLUSTER_READ)\n\
                   @interface-version(1)\n\
                   interface cluster-query {\n\
                      query ping() -> bool;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("pub mod cluster_query"),
            "source name should continue to define the Rust module; got: {rust}"
        );
        assert!(
            rust.contains(
                "pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"cluster-query/v1\");"
            ),
            "codegen must append the declared wire version; got: {rust}"
        );
    }

    #[test]
    fn emits_event_bindings_for_events_declaration() {
        let src = "plugin bmux.tabs version 1;\n\
                   interface tabs-events {\n\
                     variant pane-event { focused { pane_id: uuid }, closed { pane_id: uuid } }\n\
                     events pane-event;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const EVENT_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"bmux.tabs/tabs-events\");"
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
                   capability TABS_READ = bmux.tabs.read;\n\
                   @capability(TABS_READ)\n\
                   interface tabs-state {\n\
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
        assert!(
            !rust.contains("STATE_KIND"),
            "interfaces without events must not emit STATE_KIND; got: {rust}"
        );
        assert!(
            !rust.contains("StatePayload"),
            "interfaces without events must not emit StatePayload; got: {rust}"
        );
    }

    #[test]
    fn emits_state_bindings_for_state_annotated_events() {
        let src = "plugin bmux.pane_runtime version 1;\n\
                   interface pane-runtime-focus {\n\
                     record focus-state { focused_pane_id: uuid }\n\
                     @state events focus-state;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains(
                "pub const STATE_KIND: ::bmux_plugin_sdk::PluginEventKind = ::bmux_plugin_sdk::PluginEventKind::from_static(\"bmux.pane_runtime/pane-runtime-focus\");"
            ),
            "@state events must emit STATE_KIND; got: {rust}"
        );
        assert!(
            rust.contains("pub type StatePayload = FocusState;"),
            "@state events must emit StatePayload alias; got: {rust}"
        );
        assert!(
            !rust.contains("EVENT_KIND"),
            "@state events must not emit EVENT_KIND; got: {rust}"
        );
        assert!(
            !rust.contains("pub type EventPayload"),
            "@state events must not emit EventPayload; got: {rust}"
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
                        import windows = bmux.tabs;\n\
                        capability IMPORTER_READ = importer.read;\n\
                        @capability(IMPORTER_READ)\n\
                        interface my-iface {\n\
                          query pane-ref(id: uuid) -> windows.pane-state;\n\
                        }";
        let imported_src = "plugin bmux.tabs version 1;\n\
                            interface tabs-state {\n\
                              record pane-state { id: uuid }\n\
                            }";
        let schema = compile(importer).expect("valid");
        let unresolved = emit(&schema);
        assert!(unresolved.contains("pub const SERVICE_CONTRACT: &[u8] = &[]"));
        assert!(!unresolved.contains("interface_id: \"my-iface\", contract:"));
        assert!(unresolved.contains("my_iface::service_declaration()?"));
        assert!(unresolved.contains("&& !my_iface::SERVICE_CONTRACT.is_empty()"));
        let imported_schema = compile(imported_src).expect("valid");
        let mut imports = ImportMap::new();
        imports.insert(
            "windows".to_string(),
            ImportInfo {
                crate_path: "::bmux_tabs_plugin_api".to_string(),
                schema: imported_schema,
            },
        );
        let rust = emit_with_imports(&schema, &imports);
        assert!(rust.contains("interface_id: \"my-iface\", contract: my_iface::SERVICE_CONTRACT"));
        assert!(
            rust.contains("::bmux_tabs_plugin_api::tabs_state::PaneState"),
            "qualified type should resolve to imported crate path; got: {rust}"
        );
    }

    #[test]
    fn emits_qualified_type_via_same_schema_interface() {
        let src = "plugin p version 1;\n\
                   capability STATE_READ = p.state.read;\n\
                   interface shared-types { record shared-row { id: uuid } }\n\
                   @capability(STATE_READ)\n\
                   interface state { query row() -> shared-types.shared-row; }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(
            rust.contains("fn row<'a>(&'a self) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = super::shared_types::SharedRow> + Send + 'a>>;"),
            "same-schema qualified type should resolve to sibling module path; got: {rust}"
        );
    }
}
