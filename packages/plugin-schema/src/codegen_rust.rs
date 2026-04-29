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
/// (`::bmux_windows_plugin_api::windows_state::PaneState`).
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
    let own_types = own_type_map(schema);
    let non_eq_types = non_eq_type_set(schema);
    out.push_str("// AUTO-GENERATED FROM BPDL. DO NOT EDIT BY HAND.\n\n");
    out.push_str("use serde::{Deserialize, Serialize};\n\n");
    emit_capabilities(&schema.capabilities, &mut out);
    for iface in &schema.interfaces {
        emit_interface(
            &schema.plugin.plugin_id,
            iface,
            imports,
            &own_types,
            &non_eq_types,
            &mut out,
        );
    }
    out
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
        TypeRef::Primitive(Primitive::F32 | Primitive::F64) => true,
        TypeRef::Primitive(_) | TypeRef::Qualified { .. } | TypeRef::Unit => false,
        TypeRef::Named(name) => non_eq_types.contains(name),
        TypeRef::Option(inner) | TypeRef::List(inner) => type_contains_non_eq(inner, non_eq_types),
        TypeRef::Map(key, value) | TypeRef::Result(key, value) => {
            type_contains_non_eq(key, non_eq_types) || type_contains_non_eq(value, non_eq_types)
        }
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
                .filter_map(|item| match item {
                    InterfaceItem::Record(r) => Some(r.name.clone()),
                    InterfaceItem::Variant(v) => Some(v.name.clone()),
                    InterfaceItem::Enum(e) => Some(e.name.clone()),
                    InterfaceItem::Query(_)
                    | InterfaceItem::Command(_)
                    | InterfaceItem::Events(_) => None,
                })
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
    plugin_id: &str,
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    non_eq_types: &NonEqTypeSet,
    out: &mut String,
) {
    let module_name = snake_case(&iface.name);
    let _ = writeln!(out, "pub mod {module_name} {{");
    out.push_str("    use super::*;\n\n");

    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => emit_record(r, imports, own_types, non_eq_types, out),
            InterfaceItem::Variant(v) => emit_variant(v, imports, own_types, non_eq_types, out),
            InterfaceItem::Enum(e) => emit_enum(e, out),
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_) => {}
        }
    }

    // Service trait contains queries + commands. Events are exposed
    // separately as a typed `EVENT_KIND` constant + payload type
    // alias below.
    emit_service_trait(iface, imports, own_types, out);
    emit_transport_client(iface, imports, own_types, out);

    // If this interface declares `events <type>`, emit a canonical
    // `PluginEventKind` constant plus a `EventPayload` type alias so
    // both producers and subscribers import from the same place.
    emit_event_bindings(plugin_id, iface, imports, own_types, out);

    out.push_str("}\n\n");
}

fn emit_record(
    r: &RecordDef,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    non_eq_types: &NonEqTypeSet,
    out: &mut String,
) {
    let name = pascal_case(&r.name);
    if non_eq_types.contains(&r.name) {
        out.push_str("    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]\n");
    } else {
        out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
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
        let _ = writeln!(out, "        pub {field_name}: {ty},");
    }
    out.push_str("    }\n\n");
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
        TypeRef::Named(_) | TypeRef::Qualified { .. } | TypeRef::Result(_, _) | TypeRef::Unit => {
            false
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
    if non_eq_types.contains(&v.name) {
        out.push_str("    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]\n");
    } else {
        out.push_str("    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]\n");
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
    out.push_str("    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]\n");
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

fn emit_service_trait(
    iface: &Interface,
    imports: &ImportMap,
    own_types: &OwnTypeMap,
    out: &mut String,
) {
    let trait_name = format!("{}Service", pascal_case(&iface.name));
    // Canonical interface identifier used to look up a typed service via
    // the plugin host registry. Matches the BPDL `interface <name>` name.
    let _ = writeln!(
        out,
        "    /// Canonical identifier for this interface. Matches the `interface`\n    /// name in the BPDL source exactly; used to look up a provider via\n    /// the plugin host registry.\n    pub const INTERFACE_ID: ::bmux_plugin_sdk::InterfaceId = ::bmux_plugin_sdk::InterfaceId::from_static(\"{}\");\n",
        iface.name
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
            | InterfaceItem::Events(_) => None,
        })
        .collect::<Vec<_>>();
    if operations.is_empty() {
        return;
    }

    out.push_str("    /// Generated transport clients for this interface.\n");
    out.push_str("    pub mod client {\n");
    out.push_str("        use super::*;\n\n");
    for (op, kind) in operations {
        emit_transport_endpoint(iface, op, kind, capability, imports, own_types, out);
        emit_transport_client_function(op, imports, own_types, out);
    }
    out.push_str("    }\n\n");
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
    let returns = rust_type(&op.returns, imports, own_types);
    if !op.params.is_empty() {
        let request_name = request_ty.clone();
        out.push_str("        #[derive(Debug, Clone, Serialize)]\n");
        let _ = writeln!(out, "        pub struct {request_name} {{");
        for param in &op.params {
            let field_name = snake_case(&param.name);
            let ty = rust_type(&param.ty, imports, own_types);
            if let Some(adapter) = serde_bytes_adapter(&param.ty) {
                let _ = writeln!(out, "            #[serde(with = \"{adapter}\")]");
            }
            let _ = writeln!(out, "            pub {field_name}: {ty},");
        }
        out.push_str("        }\n\n");
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
    let _ = writeln!(
        out,
        "            const OPERATION: ::bmux_plugin_sdk::OperationId = super::{};",
        operation_const_name(&op.name)
    );
    out.push_str("        }\n\n");
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
                rust_type(&f.ty, imports, own_types)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sep = if params.is_empty() { "" } else { ", " };
    let returns = rust_type(&op.returns, imports, own_types);
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
    let _ = writeln!(
        out,
        "        pub async fn {name}<C: ::bmux_plugin_sdk::TypedDispatchClient>(client: &mut C{sep}{params}) -> ::bmux_plugin_sdk::TypedServiceClientResult<{returns}> {{"
    );
    let _ = writeln!(out, "            let request = {request_expr};");
    let _ = writeln!(
        out,
        "            ::bmux_plugin_sdk::invoke_typed_service::<C, {endpoint_name}>(client, &request).await"
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
    let _ = writeln!(
        out,
        "        pub fn {name}<'a>(&'a self{sep}{params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>> {{",
    );
    let _ = writeln!(out, "            self.inner.{name}({arg_names})");
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
    let _ = writeln!(
        out,
        "        fn {name}<'a>(&'a self{sep}{params}) -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = {returns}> + Send + 'a>>;"
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
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   capability WINDOWS_WRITE = bmux.windows.write;\n\
                   @capability(WINDOWS_READ)\n\
                   interface windows-state {\n\
                      record pane-state { id: uuid }\n\
                      query pane-state(id: uuid) -> pane-state?;\n\
                   }\n\
                   @capability(WINDOWS_WRITE)\n\
                   interface windows-commands {\n\
                      command focus-pane(id: uuid) -> result<unit, string>;\n\
                   }";
        let schema = compile(src).expect("valid");
        let rust = emit(&schema);
        assert!(rust.contains("pub trait WindowsStateService"));
        assert!(rust.contains("pub trait WindowsCommandsService"));
        assert!(rust.contains("fn pane_state"));
        assert!(rust.contains("fn focus_pane"));
        assert!(rust.contains("Option<PaneState>"));
        assert!(rust.contains("::std::result::Result<(), String>"));
    }

    #[test]
    fn emits_service_client_with_forwarders() {
        let src = "plugin p version 1;\n\
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   @capability(WINDOWS_READ)\n\
                   interface windows-state {\n\
                      record pane-state { id: uuid }\n\
                      query pane-state(id: uuid) -> pane-state?;\n\
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
    }

    #[test]
    fn emits_transport_client_with_endpoint_metadata() {
        let src = "plugin p version 1;\n\
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   @capability(WINDOWS_READ)\n\
                   interface windows-state {\n\
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
            rust.contains("const CAPABILITY: ::bmux_plugin_sdk::CapabilityId = super::super::capabilities::WINDOWS_READ;"),
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
    }

    #[test]
    fn emits_interface_id_const() {
        let src = "plugin p version 1;\n\
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   @capability(WINDOWS_READ)\n\
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
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   capability WINDOWS_WRITE = bmux.windows.write;\n\
                   @capability(WINDOWS_READ)\n\
                   interface windows-state {\n\
                      query list-panes(session: uuid) -> unit;\n\
                   }\n\
                   @capability(WINDOWS_WRITE)\n\
                   interface windows-commands {\n\
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
                   capability WINDOWS_READ = bmux.windows.read;\n\
                   @capability(WINDOWS_READ)\n\
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
                        import windows = bmux.windows;\n\
                        capability IMPORTER_READ = importer.read;\n\
                        @capability(IMPORTER_READ)\n\
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
