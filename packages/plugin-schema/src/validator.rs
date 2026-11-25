//! Semantic validation for a parsed BPDL schema.
//!
//! Verifies that:
//! - Plugin id is non-empty.
//! - Type names are unique within an interface.
//! - Variant/enum/flags case names and resource method names are unique.
//! - Operation names (queries + commands) are unique within an interface.
//! - All `Named` type references resolve to a declared type in the same
//!   interface. Qualified (`alias.type`) references resolve against
//!   declared imports or same-schema interface names.
//! - `own<R>` / `borrow<R>` name a resource, and `borrow<R>` appears only in
//!   parameters (a borrow never outlives the call that lends it).
//! - `map<K, V>` keys are one of the allowed primitives.
//! - `@default` is used at most once per enum or variant.
//! - An interface declares at most one `events <type>`.
//! - Records/variants contain no direct structural cycles (cycles
//!   through `Option<T>`, `list<T>`, or `map<_, T>` value position are
//!   allowed because the generated Rust compiles).
//! - Import aliases are unique; declared plugin ids must match the
//!   imported schema's `plugin <id>` (when an imports table is supplied).
//! - Capability constant names and ids are unique.
//! - `@since`/`@deprecated` versions never exceed the package version and
//!   `@unstable` is not combined with them.
//! - World entries resolve to service interfaces, extension point
//!   contributions match the declared data record, and plugin command
//!   parameters are command-line representable.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Error,
    ast::{
        ExtensionOperation, Field, Interface, InterfaceItem, InterfaceRef, Literal, Primitive,
        RecordDef, Schema, Stability, TypeRef, VariantDef, WorldItem,
    },
};

fn invalid(message: impl Into<String>) -> Error {
    Error::Validate {
        message: message.into(),
    }
}

/// Validate a parsed BPDL schema in isolation.
///
/// Qualified type references (`alias.type`) are checked against the
/// schema's own `imports` list or interface names, but imported types are
/// not resolved. Use [`validate_with_imports`] for full cross-schema
/// validation at codegen time.
///
/// # Errors
///
/// Returns [`Error::Validate`] if the schema violates any semantic rule.
pub fn validate(schema: &Schema) -> Result<(), Error> {
    validate_with_imports(schema, &BTreeMap::new())
}

/// Validate a parsed BPDL schema against a set of resolved imports.
///
/// `imports` maps this schema's import alias to the pre-parsed imported
/// [`Schema`]. Qualified type references are fully resolved against that
/// table when they use an import alias, or against this schema's
/// interface names otherwise. Imported aliases must be declared via
/// `import`, and the imported schema's `plugin <id>` must match the
/// declared `plugin_id` in the `import` statement.
///
/// # Errors
///
/// Returns [`Error::Validate`] if the schema violates any semantic rule.
pub fn validate_with_imports(
    schema: &Schema,
    imports: &BTreeMap<String, Schema>,
) -> Result<(), Error> {
    if schema.plugin.plugin_id.trim().is_empty() {
        return Err(invalid("plugin id must not be empty"));
    }

    // Import aliases must be unique, and if a resolved schema is
    // supplied the declared plugin_id must match.
    let mut seen_aliases: BTreeSet<&str> = BTreeSet::new();
    for imp in &schema.imports {
        if !seen_aliases.insert(imp.alias.as_str()) {
            return Err(invalid(format!("duplicate import alias `{}`", imp.alias)));
        }
        if let Some(imported) = imports.get(&imp.alias)
            && imported.plugin.plugin_id != imp.plugin_id
        {
            return Err(invalid(format!(
                "import alias `{}` declares `{}` but imported schema is `{}`",
                imp.alias, imp.plugin_id, imported.plugin.plugin_id
            )));
        }
    }
    let declared_aliases: BTreeSet<&str> =
        schema.imports.iter().map(|i| i.alias.as_str()).collect();

    let mut capability_names: BTreeSet<&str> = BTreeSet::new();
    let mut capability_ids: BTreeSet<&str> = BTreeSet::new();
    for capability in &schema.capabilities {
        if !is_rust_const_identifier(&capability.name) {
            return Err(invalid(format!(
                "capability constant `{}` is not a valid Rust constant identifier",
                capability.name
            )));
        }
        if !capability_names.insert(capability.name.as_str()) {
            return Err(invalid(format!(
                "duplicate capability constant `{}`",
                capability.name
            )));
        }
        if !capability_ids.insert(capability.id.as_str()) {
            return Err(invalid(format!(
                "duplicate capability id `{}`",
                capability.id
            )));
        }
    }

    let declared_capabilities: BTreeSet<&str> = schema
        .capabilities
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    validate_interface_families(schema)?;

    let scope = Scope {
        declared_aliases: &declared_aliases,
        imports,
        schema_type_names: schema_type_names(schema),
        schema_resources: schema_resource_names(schema),
    };
    for iface in &schema.interfaces {
        validate_interface(iface, &scope, &declared_capabilities)?;
    }
    validate_stability_everywhere(schema)?;
    validate_extension_points(schema, &scope)?;
    validate_worlds(schema, imports)?;
    validate_plugin_commands(schema)?;
    validate_contributions(schema, imports)?;
    Ok(())
}

fn validate_interface_families(schema: &Schema) -> Result<(), Error> {
    let mut families: BTreeMap<&str, Vec<&Interface>> = BTreeMap::new();
    for iface in &schema.interfaces {
        families.entry(iface.name.as_str()).or_default().push(iface);
    }
    for (name, family) in families {
        if family.len() == 1 {
            continue;
        }
        let mut versions = BTreeSet::new();
        for iface in family {
            let Some(version) = iface.interface_version else {
                return Err(invalid(format!(
                    "duplicate interface `{name}` requires an explicit distinct @interface-version on every declaration"
                )));
            };
            if !versions.insert(version) {
                return Err(invalid(format!(
                    "duplicate interface `{name}` has repeated wire version {version}"
                )));
            }
            let service_only = iface
                .items
                .iter()
                .all(|item| matches!(item, InterfaceItem::Query(_) | InterfaceItem::Command(_)));
            if !service_only {
                return Err(invalid(format!(
                    "versioned interface family `{name}` must be service-only; move shared types and events to uniquely named interfaces"
                )));
            }
        }
    }
    Ok(())
}

/// Name of a user type defined by an interface item, if any.
pub(crate) const fn defined_type_name(item: &InterfaceItem) -> Option<&String> {
    match item {
        InterfaceItem::Record(r) => Some(&r.name),
        InterfaceItem::Variant(v) => Some(&v.name),
        InterfaceItem::Enum(e) => Some(&e.name),
        InterfaceItem::Flags(f) => Some(&f.name),
        InterfaceItem::Resource(r) => Some(&r.name),
        InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_) => None,
    }
}

fn schema_type_names(schema: &Schema) -> BTreeMap<String, BTreeSet<String>> {
    schema
        .interfaces
        .iter()
        .map(|iface| {
            let names = iface
                .items
                .iter()
                .filter_map(defined_type_name)
                .cloned()
                .collect();
            (iface.name.clone(), names)
        })
        .collect()
}

fn schema_resource_names(schema: &Schema) -> BTreeMap<String, BTreeSet<String>> {
    schema
        .interfaces
        .iter()
        .map(|iface| {
            let names = iface
                .items
                .iter()
                .filter_map(|item| match item {
                    InterfaceItem::Resource(r) => Some(r.name.clone()),
                    _ => None,
                })
                .collect();
            (iface.name.clone(), names)
        })
        .collect()
}

fn is_rust_const_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Schema-wide name resolution inputs shared by every type check.
struct Scope<'a> {
    declared_aliases: &'a BTreeSet<&'a str>,
    imports: &'a BTreeMap<String, Schema>,
    schema_type_names: BTreeMap<String, BTreeSet<String>>,
    schema_resources: BTreeMap<String, BTreeSet<String>>,
}

/// Per-position type checking context.
#[derive(Clone, Copy)]
struct TypeContext<'a> {
    scope: &'a Scope<'a>,
    /// Names of types defined by the enclosing interface (empty for
    /// extension points, which define no types of their own).
    known: &'a BTreeSet<String>,
    resources: &'a BTreeSet<String>,
    owner: &'a str,
    /// `borrow<R>` is legal only in parameter position.
    allow_borrow: bool,
}

fn validate_interface(
    iface: &Interface,
    scope: &Scope<'_>,
    declared_capabilities: &BTreeSet<&str>,
) -> Result<(), Error> {
    let type_names = collect_and_validate_names(iface)?;
    validate_interface_shape(iface, declared_capabilities)?;
    let resources = scope
        .schema_resources
        .get(&iface.name)
        .cloned()
        .unwrap_or_default();
    resolve_type_references(
        iface,
        TypeContext {
            scope,
            known: &type_names,
            resources: &resources,
            owner: &iface.name,
            allow_borrow: false,
        },
    )?;
    // Acyclic check on records and variants (compile-follows — `Option<T>`,
    // `list<T>`, and `map<_, T>` value break cycles).
    check_acyclic(iface)?;
    Ok(())
}

fn validate_interface_shape(
    iface: &Interface,
    declared_capabilities: &BTreeSet<&str>,
) -> Result<(), Error> {
    let has_queries = iface
        .items
        .iter()
        .any(|item| matches!(item, InterfaceItem::Query(_)));
    let has_commands = iface
        .items
        .iter()
        .any(|item| matches!(item, InterfaceItem::Command(_)));
    let has_events = iface
        .items
        .iter()
        .any(|item| matches!(item, InterfaceItem::Events(_)));

    if has_queries && has_commands {
        return Err(invalid(format!(
            "interface `{}` mixes queries and commands; split state and command interfaces",
            iface.name
        )));
    }
    if has_events && (has_queries || has_commands) {
        return Err(invalid(format!(
            "interface `{}` mixes events with service operations; split event interfaces from services",
            iface.name
        )));
    }

    if has_queries || has_commands {
        let Some(capability) = iface.capability.as_deref() else {
            return Err(invalid(format!(
                "service interface `{}` must declare `@capability(NAME)`",
                iface.name
            )));
        };
        if !declared_capabilities.contains(capability) {
            return Err(invalid(format!(
                "interface `{}` references unknown capability `{capability}`",
                iface.name
            )));
        }
    } else if iface.capability.is_some() {
        return Err(invalid(format!(
            "interface `{}` declares `@capability` but has no query or command operations",
            iface.name
        )));
    }
    // Access classes govern routed operations only.
    if iface.access.is_some() && !(has_queries || has_commands) {
        return Err(invalid(format!(
            "interface `{}` declares `@access` but has no query or command operations",
            iface.name
        )));
    }

    Ok(())
}

/// First validation pass: collect type names, reject duplicates,
/// reject duplicate case/op names, verify `@default` uniqueness and
/// single-events-declaration-per-interface.
fn collect_and_validate_names(iface: &Interface) -> Result<BTreeSet<String>, Error> {
    let mut type_names: BTreeSet<String> = BTreeSet::new();
    let mut op_names: BTreeSet<String> = BTreeSet::new();
    let mut events_declared = false;

    for item in &iface.items {
        if let Some(name) = defined_type_name(item) {
            insert_type_name(&mut type_names, name, &iface.name)?;
        }
        match item {
            InterfaceItem::Record(_) => {}
            InterfaceItem::Variant(v) => validate_variant_cases(v)?,
            InterfaceItem::Enum(e) => validate_enum_cases(e)?,
            InterfaceItem::Flags(flags) => {
                if flags.flags.is_empty() {
                    return Err(invalid(format!("flags `{}` declares no flags", flags.name)));
                }
                // Generated Rust stores flags in one `u64`.
                if flags.flags.len() > 64 {
                    return Err(invalid(format!(
                        "flags `{}` declares more than 64 flags",
                        flags.name
                    )));
                }
                let mut seen = BTreeSet::new();
                for flag in &flags.flags {
                    if !seen.insert(flag.name.as_str()) {
                        return Err(invalid(format!(
                            "duplicate flag `{}` in flags `{}`",
                            flag.name, flags.name
                        )));
                    }
                }
            }
            InterfaceItem::Resource(resource) => {
                let mut seen = BTreeSet::new();
                for method in &resource.methods {
                    if !seen.insert(method.name.as_str()) {
                        return Err(invalid(format!(
                            "duplicate method `{}` in resource `{}`",
                            method.name, resource.name
                        )));
                    }
                }
            }
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                if !op_names.insert(op.name.clone()) {
                    return Err(invalid(format!(
                        "duplicate operation `{}` in interface `{}`",
                        op.name, iface.name
                    )));
                }
                // The parser enforces this; re-check for programmatic ASTs.
                if op.reentrant && (matches!(item, InterfaceItem::Command(_)) || op.emits.is_some())
                {
                    return Err(invalid(format!(
                        "`@reentrant` on operation `{}` in interface `{}` is only allowed on non-streaming queries",
                        op.name, iface.name
                    )));
                }
            }
            InterfaceItem::Events(_) => {
                if events_declared {
                    return Err(invalid(format!(
                        "interface `{}` declares `events` more than once",
                        iface.name
                    )));
                }
                events_declared = true;
            }
        }
    }
    Ok(type_names)
}

fn insert_type_name(set: &mut BTreeSet<String>, name: &str, iface_name: &str) -> Result<(), Error> {
    if set.insert(name.to_string()) {
        Ok(())
    } else {
        Err(invalid(format!(
            "duplicate type `{name}` in interface `{iface_name}`"
        )))
    }
}

fn validate_variant_cases(v: &crate::ast::VariantDef) -> Result<(), Error> {
    if v.cases
        .iter()
        .any(|case| case.payload.iter().any(|field| field.secret))
    {
        return Err(invalid(
            "`@secret` is supported only on record fields and operation parameters",
        ));
    }
    let mut case_names = BTreeSet::new();
    let mut defaults = 0;
    for c in &v.cases {
        if !case_names.insert(c.name.clone()) {
            return Err(invalid(format!(
                "duplicate case `{}` in variant `{}`",
                c.name, v.name
            )));
        }
        if c.is_default {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(invalid(format!(
            "variant `{}` has multiple @default cases",
            v.name
        )));
    }
    Ok(())
}

fn validate_enum_cases(e: &crate::ast::EnumDef) -> Result<(), Error> {
    let mut case_names = BTreeSet::new();
    let mut defaults = 0;
    for c in &e.cases {
        if !case_names.insert(c.name.clone()) {
            return Err(invalid(format!(
                "duplicate case `{}` in enum `{}`",
                c.name, e.name
            )));
        }
        if c.is_default {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(invalid(format!(
            "enum `{}` has multiple @default cases",
            e.name
        )));
    }
    Ok(())
}

fn check_fields(fields: &[Field], cx: TypeContext<'_>) -> Result<(), Error> {
    for field in fields {
        check_type(&field.ty, cx)?;
    }
    Ok(())
}

/// Second validation pass: resolve every `TypeRef` in the interface.
fn resolve_type_references(iface: &Interface, cx: TypeContext<'_>) -> Result<(), Error> {
    let params = TypeContext {
        allow_borrow: true,
        ..cx
    };
    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => check_fields(&r.fields, cx)?,
            InterfaceItem::Variant(v) => {
                for c in &v.cases {
                    check_fields(&c.payload, cx)?;
                }
            }
            InterfaceItem::Enum(_) | InterfaceItem::Flags(_) => {}
            InterfaceItem::Resource(resource) => {
                for method in &resource.methods {
                    check_fields(&method.params, params)?;
                    if let Some(returns) = &method.returns {
                        check_type(returns, cx)?;
                    }
                }
            }
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                check_fields(&op.params, params)?;
                check_type(&op.returns, cx)?;
                if let Some(emits) = &op.emits {
                    check_type(emits, cx)?;
                }
            }
            InterfaceItem::Events(decl) => check_type(&decl.ty, cx)?,
        }
    }
    Ok(())
}

fn check_type(ty: &TypeRef, cx: TypeContext<'_>) -> Result<(), Error> {
    let scope = cx.scope;
    let iface_name = cx.owner;
    match ty {
        TypeRef::Primitive(_) | TypeRef::Unit => Ok(()),
        TypeRef::Named(name) => {
            if cx.known.contains(name) {
                Ok(())
            } else {
                Err(invalid(format!(
                    "unknown type `{name}` referenced in interface `{iface_name}`"
                )))
            }
        }
        TypeRef::Qualified { alias, name } => {
            if scope.declared_aliases.contains(alias.as_str()) {
                // If the caller supplied the resolved schema, confirm the
                // type exists in one of its interfaces.
                if let Some(imported) = scope.imports.get(alias)
                    && !imported_has_type(imported, name)
                {
                    return Err(invalid(format!(
                        "imported plugin `{}` (alias `{alias}`) has no type `{name}`",
                        imported.plugin.plugin_id
                    )));
                }
                return Ok(());
            }

            if let Some(types) = scope.schema_type_names.get(alias) {
                if types.contains(name) {
                    return Ok(());
                }
                return Err(invalid(format!(
                    "interface `{alias}` referenced in interface `{iface_name}` has no type `{name}`"
                )));
            }

            Err(invalid(format!(
                "unknown import alias or interface `{alias}` referenced in interface `{iface_name}`"
            )))
        }
        TypeRef::Option(inner)
        | TypeRef::List(inner)
        | TypeRef::Stream(inner)
        | TypeRef::Future(inner) => check_type(
            inner,
            TypeContext {
                allow_borrow: false,
                ..cx
            },
        ),
        TypeRef::Map(key, value) => {
            check_map_key(key, iface_name)?;
            check_type(key, cx)?;
            check_type(value, cx)
        }
        TypeRef::Result(a, b) => {
            check_type(a, cx)?;
            check_type(b, cx)
        }
        TypeRef::Tuple(items) => {
            for item in items {
                check_type(item, cx)?;
            }
            Ok(())
        }
        TypeRef::Own(inner) | TypeRef::Borrow(inner) => {
            let borrow = matches!(ty, TypeRef::Borrow(_));
            if borrow && !cx.allow_borrow {
                return Err(invalid(format!(
                    "`borrow<...>` in `{iface_name}` is only allowed in parameters; a borrowed handle never outlives its call"
                )));
            }
            check_type(inner, cx)?;
            if !names_resource(inner, cx) {
                return Err(invalid(format!(
                    "`{}<...>` in `{iface_name}` must name a resource type",
                    if borrow { "borrow" } else { "own" }
                )));
            }
            Ok(())
        }
    }
}

/// Does `ty` name a resource? Imported names without a resolved schema
/// cannot be checked here and are accepted; full validation resolves them.
fn names_resource(ty: &TypeRef, cx: TypeContext<'_>) -> bool {
    match ty {
        TypeRef::Named(name) => cx.resources.contains(name),
        TypeRef::Qualified { alias, name } => {
            if cx.scope.declared_aliases.contains(alias.as_str()) {
                cx.scope.imports.get(alias).is_none_or(|imported| {
                    imported.interfaces.iter().any(|iface| {
                        iface.items.iter().any(|item| {
                            matches!(item, InterfaceItem::Resource(resource) if resource.name == *name)
                        })
                    })
                })
            } else {
                cx.scope
                    .schema_resources
                    .get(alias)
                    .is_some_and(|resources| resources.contains(name))
            }
        }
        _ => false,
    }
}

/// Map keys must be one of the `Ord`-able primitives in
/// [`crate::ast::Primitive::is_valid_map_key`].
fn check_map_key(key: &TypeRef, iface_name: &str) -> Result<(), Error> {
    match key {
        TypeRef::Primitive(p) if p.is_valid_map_key() => Ok(()),
        TypeRef::Primitive(p) => Err(invalid(format!(
            "map key type `{}` is not allowed in interface `{iface_name}`; \
             map keys must be one of: string, uuid, integer primitives",
            p.keyword()
        ))),
        _ => Err(invalid(format!(
            "map key must be a primitive type in interface `{iface_name}`; \
             map keys must be one of: string, uuid, integer primitives"
        ))),
    }
}

fn imported_has_type(schema: &Schema, name: &str) -> bool {
    schema.interfaces.iter().any(|iface| {
        iface
            .items
            .iter()
            .any(|item| defined_type_name(item).is_some_and(|defined| defined == name))
    })
}

fn check_stability(stability: &Stability, version: u32, what: &str) -> Result<(), Error> {
    if stability.unstable.is_some() && (stability.since.is_some() || stability.deprecated.is_some())
    {
        return Err(invalid(format!(
            "{what} cannot be both `@unstable` and `@since`/`@deprecated`"
        )));
    }
    for (label, value) in [
        ("since", stability.since),
        ("deprecated", stability.deprecated),
    ] {
        if let Some(value) = value
            && (value == 0 || value > version)
        {
            return Err(invalid(format!(
                "{what} `@{label}({value})` must be between 1 and the package version {version}"
            )));
        }
    }
    if let Some(deprecated) = stability.deprecated {
        match stability.since {
            Some(since) if since <= deprecated => {}
            _ => {
                return Err(invalid(format!(
                    "{what} `@deprecated({deprecated})` requires an earlier or equal `@since`"
                )));
            }
        }
    }
    Ok(())
}

fn validate_stability_everywhere(schema: &Schema) -> Result<(), Error> {
    let version = schema.plugin.version;
    for iface in &schema.interfaces {
        let owner = format!("interface `{}`", iface.name);
        check_stability(&iface.stability, version, &owner)?;
        for item in &iface.items {
            let (stability, name) = match item {
                InterfaceItem::Record(r) => (&r.stability, &r.name),
                InterfaceItem::Variant(v) => (&v.stability, &v.name),
                InterfaceItem::Enum(e) => (&e.stability, &e.name),
                InterfaceItem::Flags(f) => (&f.stability, &f.name),
                InterfaceItem::Resource(r) => {
                    for method in &r.methods {
                        check_stability(
                            &method.stability,
                            version,
                            &format!("method `{}` of `{}`", method.name, r.name),
                        )?;
                    }
                    (&r.stability, &r.name)
                }
                InterfaceItem::Query(op) | InterfaceItem::Command(op) => (&op.stability, &op.name),
                InterfaceItem::Events(events) => {
                    check_stability(&events.stability, version, &format!("events of {owner}"))?;
                    continue;
                }
            };
            check_stability(stability, version, &format!("`{name}` in {owner}"))?;
        }
    }
    for world in &schema.worlds {
        check_stability(
            &world.stability,
            version,
            &format!("world `{}`", world.name),
        )?;
    }
    for command in &schema.commands {
        check_stability(
            &command.stability,
            version,
            &format!("command `{}`", command.name),
        )?;
    }
    for point in &schema.extension_points {
        check_stability(
            &point.stability,
            version,
            &format!("extension point `{}`", point.name),
        )?;
    }
    Ok(())
}

fn validate_extension_points(schema: &Schema, scope: &Scope<'_>) -> Result<(), Error> {
    let mut seen = BTreeSet::new();
    let none = BTreeSet::new();
    for point in &schema.extension_points {
        if !seen.insert((point.name.as_str(), point.version)) {
            return Err(invalid(format!(
                "duplicate extension point `{}` version {}",
                point.name, point.version
            )));
        }
        let owner = format!("extension point `{}`", point.name);
        let cx = TypeContext {
            scope,
            known: &none,
            resources: &none,
            owner: &owner,
            allow_borrow: false,
        };
        let mut fields = BTreeSet::new();
        for field in &point.data {
            if !fields.insert(field.name.as_str()) {
                return Err(invalid(format!(
                    "duplicate data field `{}` in {owner}",
                    field.name
                )));
            }
        }
        check_fields(&point.data, cx)?;
        let mut names = BTreeSet::new();
        for operation in &point.operations {
            let (ExtensionOperation::Query(op) | ExtensionOperation::Command(op)) = operation;
            if !names.insert(op.name.as_str()) {
                return Err(invalid(format!(
                    "duplicate operation `{}` in {owner}",
                    op.name
                )));
            }
            check_fields(
                &op.params,
                TypeContext {
                    allow_borrow: true,
                    ..cx
                },
            )?;
            check_type(&op.returns, cx)?;
            if let Some(emits) = &op.emits {
                check_type(emits, cx)?;
            }
        }
    }
    Ok(())
}

/// Resolution of a world interface reference.
pub(crate) enum ResolvedInterface<'a> {
    /// Found in this schema or a supplied import.
    Found(&'a Interface),
    /// Names a declared import whose schema was not supplied.
    Unchecked,
}

/// Resolve `[alias.]name[@N]` against this schema or its supplied imports.
///
/// # Errors
///
/// Unknown aliases, missing interfaces, ambiguous versioned families, and
/// `@N` on interfaces without an explicit wire version are rejected.
pub(crate) fn resolve_interface_ref<'a>(
    schema: &'a Schema,
    imports: &'a BTreeMap<String, Schema>,
    reference: &InterfaceRef,
) -> Result<ResolvedInterface<'a>, Error> {
    let target = match &reference.alias {
        None => schema,
        Some(alias) => {
            if !schema.imports.iter().any(|import| import.alias == *alias) {
                return Err(invalid(format!(
                    "world references unknown import alias `{alias}`"
                )));
            }
            match imports.get(alias) {
                Some(imported) => imported,
                None => return Ok(ResolvedInterface::Unchecked),
            }
        }
    };
    let display = reference.alias.as_ref().map_or_else(
        || reference.name.clone(),
        |alias| format!("{alias}.{}", reference.name),
    );
    let mut candidates = target
        .interfaces
        .iter()
        .filter(|iface| iface.name == reference.name);
    let found = if let Some(version) = reference.version {
        candidates
            .find(|iface| iface.interface_version == Some(version))
            .ok_or_else(|| {
                invalid(format!(
                    "world references `{display}@{version}`, but no interface declares `@interface-version({version})`"
                ))
            })?
    } else {
        let first = candidates
            .next()
            .ok_or_else(|| invalid(format!("world references unknown interface `{display}`")))?;
        if candidates.next().is_some() {
            return Err(invalid(format!(
                "world reference `{display}` is ambiguous; select a member with `@N`"
            )));
        }
        first
    };
    Ok(ResolvedInterface::Found(found))
}

fn interface_is_service(iface: &Interface) -> bool {
    iface.items.iter().any(|item| {
        matches!(
            item,
            InterfaceItem::Query(_) | InterfaceItem::Command(_) | InterfaceItem::Events(_)
        )
    })
}

fn validate_worlds(schema: &Schema, imports: &BTreeMap<String, Schema>) -> Result<(), Error> {
    let mut names = BTreeSet::new();
    for world in &schema.worlds {
        if !names.insert(world.name.as_str()) {
            return Err(invalid(format!("duplicate world `{}`", world.name)));
        }
        let mut provided = BTreeSet::new();
        for item in &world.items {
            match item {
                WorldItem::Provide {
                    interface,
                    capability,
                    ..
                } => {
                    if let ResolvedInterface::Found(iface) =
                        resolve_interface_ref(schema, imports, interface)?
                    {
                        if !interface_is_service(iface) {
                            return Err(invalid(format!(
                                "world `{}` cannot provide types-only interface `{}`",
                                world.name, iface.name
                            )));
                        }
                        match (&iface.capability, capability) {
                            (Some(_), Some(_)) => {
                                return Err(invalid(format!(
                                    "world `{}` must not restate the capability of `{}`; it is declared on the interface",
                                    world.name, iface.name
                                )));
                            }
                            (None, None) => {
                                return Err(invalid(format!(
                                    "world `{}` must name the `capability <id>` guarding event interface `{}`",
                                    world.name, iface.name
                                )));
                            }
                            _ => {}
                        }
                    }
                    let key = (
                        interface.alias.clone(),
                        interface.name.clone(),
                        interface.version,
                    );
                    if !provided.insert(key) {
                        return Err(invalid(format!(
                            "world `{}` provides `{}` more than once",
                            world.name, interface.name
                        )));
                    }
                }
                WorldItem::Require { interface, .. } => {
                    if let ResolvedInterface::Found(iface) =
                        resolve_interface_ref(schema, imports, interface)?
                        && !interface_is_service(iface)
                    {
                        return Err(invalid(format!(
                            "world `{}` cannot require types-only interface `{}`",
                            world.name, iface.name
                        )));
                    }
                }
                WorldItem::ProvideOpaque { .. }
                | WorldItem::ProvideCapability { .. }
                | WorldItem::RequireCapability { .. } => {}
            }
        }
    }
    Ok(())
}

/// Command-line representable parameter types: scalars, optional scalars,
/// and lists of strings.
fn command_param_supported(ty: &TypeRef) -> bool {
    match ty {
        TypeRef::Primitive(primitive) => !matches!(primitive, Primitive::Bytes),
        TypeRef::Option(inner) => matches!(**inner, TypeRef::Primitive(p) if p != Primitive::Bytes),
        TypeRef::List(inner) => matches!(**inner, TypeRef::Primitive(Primitive::String)),
        _ => false,
    }
}

fn validate_plugin_commands(schema: &Schema) -> Result<(), Error> {
    let mut names = BTreeSet::new();
    for command in &schema.commands {
        if !names.insert(command.name.as_str()) {
            return Err(invalid(format!("duplicate command `{}`", command.name)));
        }
        let mut params = BTreeSet::new();
        for param in &command.params {
            if !params.insert(param.name.as_str()) {
                return Err(invalid(format!(
                    "duplicate parameter `{}` in command `{}`",
                    param.name, command.name
                )));
            }
            if !command_param_supported(&param.ty) {
                return Err(invalid(format!(
                    "parameter `{}` of command `{}` must be a scalar, optional scalar, or list<string>",
                    param.name, command.name
                )));
            }
        }
    }
    Ok(())
}

fn literal_matches(literal: &Literal, ty: &TypeRef) -> bool {
    match (literal, ty) {
        (_, TypeRef::Option(inner)) => literal_matches(literal, inner),
        (Literal::String(value), TypeRef::Primitive(Primitive::Char)) => value.chars().count() == 1,
        (Literal::String(_), TypeRef::Primitive(Primitive::String | Primitive::Uuid))
        | (Literal::Bool(_), TypeRef::Primitive(Primitive::Bool))
        | (
            Literal::Int(_),
            TypeRef::Primitive(
                Primitive::U8
                | Primitive::U16
                | Primitive::U32
                | Primitive::U64
                | Primitive::I8
                | Primitive::I16
                | Primitive::I32
                | Primitive::I64,
            ),
        ) => true,
        (Literal::List(items), TypeRef::List(inner)) => {
            items.iter().all(|item| literal_matches(item, inner))
        }
        _ => false,
    }
}

fn validate_contributions(
    schema: &Schema,
    imports: &BTreeMap<String, Schema>,
) -> Result<(), Error> {
    for contribution in &schema.contributions {
        let reference = &contribution.extension_point;
        let target = match &reference.alias {
            None => Some(schema),
            Some(alias) => {
                if !schema.imports.iter().any(|import| import.alias == *alias) {
                    return Err(invalid(format!(
                        "contribution references unknown import alias `{alias}`"
                    )));
                }
                imports.get(alias)
            }
        };
        let mut seen = BTreeSet::new();
        for (field, _) in &contribution.values {
            if !seen.insert(field.as_str()) {
                return Err(invalid(format!(
                    "contribution to `{}` repeats field `{field}`",
                    reference.name
                )));
            }
        }
        let Some(target) = target else {
            continue;
        };
        let point = target
            .extension_points
            .iter()
            .find(|point| point.name == reference.name && point.version == reference.version)
            .ok_or_else(|| {
                invalid(format!(
                    "contribution references unknown extension point `{}@{}`",
                    reference.name, reference.version
                ))
            })?;
        for (field, value) in &contribution.values {
            let declared = point
                .data
                .iter()
                .find(|declared| declared.name == *field)
                .ok_or_else(|| {
                    invalid(format!(
                        "extension point `{}` has no data field `{field}`",
                        point.name
                    ))
                })?;
            if !literal_matches(value, &declared.ty) {
                return Err(invalid(format!(
                    "contribution field `{field}` does not match its declared type in `{}`",
                    point.name
                )));
            }
        }
        for declared in &point.data {
            if !matches!(declared.ty, TypeRef::Option(_))
                && !contribution
                    .values
                    .iter()
                    .any(|(field, _)| *field == declared.name)
            {
                return Err(invalid(format!(
                    "contribution to `{}` is missing required field `{}`",
                    point.name, declared.name
                )));
            }
        }
    }
    Ok(())
}

/// Build a dependency graph over an interface's user-defined types and
/// reject any genuine structural cycle. Cycles through `Option<T>`,
/// `list<T>`, or `map<_, T>` *value* position are allowed because the
/// generated Rust (`Option<_>` / `Vec<_>` / `BTreeMap<_, _>`) provides
/// heap indirection and compiles.
fn check_acyclic(iface: &Interface) -> Result<(), Error> {
    // `BTreeMap` for deterministic iteration order so cycle errors
    // report reproducibly across runs and rustc versions.
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for item in &iface.items {
        match item {
            InterfaceItem::Record(RecordDef { name, fields, .. }) => {
                let mut deps = Vec::new();
                for f in fields {
                    collect_required_named(&f.ty, &mut deps);
                }
                edges.insert(name.clone(), deps);
            }
            InterfaceItem::Variant(VariantDef { name, cases, .. }) => {
                let mut deps = Vec::new();
                for case in cases {
                    for f in &case.payload {
                        collect_required_named(&f.ty, &mut deps);
                    }
                }
                edges.insert(name.clone(), deps);
            }
            _ => {}
        }
    }

    // DFS cycle detection with path tracking for good error messages.
    let mut color: BTreeMap<&str, Color> = BTreeMap::new();
    for key in edges.keys() {
        color.insert(key.as_str(), Color::White);
    }
    for start in edges.keys() {
        if color[start.as_str()] == Color::White {
            let mut stack: Vec<&str> = Vec::new();
            dfs(start.as_str(), &edges, &mut color, &mut stack, &iface.name)?;
        }
    }
    Ok(())
}

/// Walk a type, pushing onto `deps` only the `Named` refs whose presence
/// constitutes a structural cycle edge. `Option`, `List`, and `Map`
/// value-position break the edge because Rust's lowering provides
/// heap indirection, as do handles and asynchronous values. Map keys DO
/// keep the edge because a `BTreeMap<K, _>` requires `K: Ord + Clone` —
/// but since we only allow primitive keys, in practice map keys never pull
/// a user type into the graph.
fn collect_required_named(ty: &TypeRef, deps: &mut Vec<String>) {
    match ty {
        TypeRef::Named(name) => deps.push(name.clone()),
        TypeRef::Result(a, b) => {
            collect_required_named(a, deps);
            collect_required_named(b, deps);
        }
        TypeRef::Tuple(items) => {
            for item in items {
                collect_required_named(item, deps);
            }
        }
        TypeRef::Qualified { .. }
        | TypeRef::Primitive(_)
        | TypeRef::Unit
        | TypeRef::Option(_)
        | TypeRef::List(_)
        | TypeRef::Map(_, _)
        | TypeRef::Stream(_)
        | TypeRef::Future(_)
        | TypeRef::Own(_)
        | TypeRef::Borrow(_) => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

fn dfs<'a>(
    node: &'a str,
    edges: &'a BTreeMap<String, Vec<String>>,
    color: &mut BTreeMap<&'a str, Color>,
    stack: &mut Vec<&'a str>,
    iface_name: &str,
) -> Result<(), Error> {
    color.insert(node, Color::Gray);
    stack.push(node);
    if let Some(deps) = edges.get(node) {
        for dep in deps {
            // Only traverse known nodes in this graph.
            let Some(dep_key) = edges.keys().find(|k| k.as_str() == dep) else {
                continue;
            };
            let dep_str = dep_key.as_str();
            match color.get(dep_str).copied().unwrap_or(Color::White) {
                Color::White => dfs(dep_str, edges, color, stack, iface_name)?,
                Color::Gray => {
                    // Cycle: walk stack back to dep.
                    let mut path: Vec<&str> = Vec::new();
                    let mut collecting = false;
                    for frame in stack.iter() {
                        if *frame == dep_str {
                            collecting = true;
                        }
                        if collecting {
                            path.push(*frame);
                        }
                    }
                    path.push(dep_str);
                    let rendered = path.join(" -> ");
                    return Err(Error::Validate {
                        message: format!(
                            "type cycle in interface `{iface_name}`: {rendered} \
                             (break the cycle with `?`, `list<T>`, or `map<_, T>` value)"
                        ),
                    });
                }
                Color::Black => {}
            }
        }
    }
    stack.pop();
    color.insert(node, Color::Black);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{Error, compile};

    #[test]
    fn accepts_valid_schema() {
        let src = "plugin p version 1;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i {\n\
                     record r { id: uuid }\n\
                     query q() -> r;\n\
                   }";
        let _ = compile(src).expect("valid");
    }

    #[test]
    fn rejects_service_interface_without_capability() {
        let src = "plugin p version 1;\n\
                   interface i { query q() -> unit; }";
        let err = compile(src).unwrap_err();
        assert!(
            matches!(&err, Error::Validate { message } if message.contains("must declare `@capability")),
            "got: {err:?}"
        );
    }

    #[test]
    fn access_needs_operations_and_never_changes_contract_identity() {
        let src = "plugin p version 1;\n\
                   @access(observe) interface i { record e { id: uuid } events e; }";
        let err = compile(src).unwrap_err();
        assert!(
            matches!(&err, Error::Validate { message } if message.contains("declares `@access` but has no query or command")),
            "got: {err:?}"
        );
        let plain = "plugin p version 1; capability R = p.r;\n\
                     @capability(R) interface i { query q() -> u32; query w() -> u32; }";
        let marked = plain
            .replace("@capability(R)", "@capability(R) @access(observe)")
            .replace("query w()", "@access(owner-local) query w()");
        let identity = |source: &str| {
            let schema = compile(source).expect("valid");
            crate::canonical::interface_contract(
                &schema,
                &schema.interfaces[0],
                &crate::codegen_rust::ImportMap::new(),
            )
        };
        assert_eq!(identity(plain), identity(&marked));
    }

    #[test]
    fn rejects_mixed_query_command_interface() {
        let src = "plugin p version 1;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i { query q() -> unit; command c() -> unit; }";
        let err = compile(src).unwrap_err();
        assert!(
            matches!(&err, Error::Validate { message } if message.contains("mixes queries and commands")),
            "got: {err:?}"
        );
    }

    #[test]
    fn rejects_events_mixed_with_service_operations() {
        let src = "plugin p version 1;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i { record e { id: uuid } query q() -> unit; events e; }";
        let err = compile(src).unwrap_err();
        assert!(
            matches!(&err, Error::Validate { message } if message.contains("mixes events with service operations")),
            "got: {err:?}"
        );
    }

    #[test]
    fn accepts_distinct_explicit_versions_of_service_only_interface() {
        let src = "plugin p version 1;\n\
                   capability RUN = p.run;\n\
                   @capability(RUN) @interface-version(1)\n\
                   interface actions { command run() -> unit; }\n\
                   @capability(RUN) @interface-version(2)\n\
                   interface actions { command run() -> unit; command stop() -> unit; }";
        compile(src).expect("versioned service family");
    }

    #[test]
    fn rejects_ambiguous_or_type_defining_versioned_interface_families() {
        let missing_version = "plugin p version 1;\n\
            capability RUN = p.run;\n\
            @capability(RUN) interface actions { command run() -> unit; }\n\
            @capability(RUN) @interface-version(2) interface actions { command run() -> unit; }";
        assert!(compile(missing_version).is_err());

        let repeated_version = "plugin p version 1;\n\
            capability RUN = p.run;\n\
            @capability(RUN) @interface-version(1) interface actions { command run() -> unit; }\n\
            @capability(RUN) @interface-version(1) interface actions { command run() -> unit; }";
        assert!(compile(repeated_version).is_err());

        let types = "plugin p version 1;\n\
            @interface-version(1) interface types { record value { id: uuid } }\n\
            @interface-version(2) interface types { record value { id: uuid } }";
        assert!(compile(types).is_err());
    }

    #[test]
    fn rejects_duplicate_type() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { a: u32 }\n\
                     record r { b: u32 }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_unknown_type_reference() {
        let src = "plugin p version 1;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i {\n\
                     query q() -> missing;\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_duplicate_capability_constants() {
        let src = "plugin p version 1;\n\
                   capability FOO = bmux.foo.read;\n\
                   capability FOO = bmux.foo.write;\n\
                   interface i { query q() -> unit; }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_duplicate_capability_ids() {
        let src = "plugin p version 1;\n\
                   capability FOO_READ = bmux.foo.read;\n\
                   capability BAR_READ = bmux.foo.read;\n\
                   interface i { query q() -> unit; }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_invalid_capability_constant_name() {
        let src = "plugin p version 1;\n\
                   capability foo-read = bmux.foo.read;\n\
                   interface i { query q() -> unit; }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_duplicate_events_declaration() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record e { kind: u32 }\n\
                     events e;\n\
                     events e;\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_direct_self_cycle_in_record() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record node { child: node }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(
            matches!(&err, Error::Validate { message } if message.contains("cycle")),
            "got: {err:?}"
        );
    }

    #[test]
    fn accepts_self_cycle_through_option() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record node { child: node? }\n\
                   }";
        let _ = compile(src).expect("self-cycle via Option<T> should be allowed");
    }

    #[test]
    fn accepts_self_cycle_through_list() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record tree { children: list<tree> }\n\
                   }";
        let _ = compile(src).expect("self-cycle via list<T> should be allowed");
    }

    #[test]
    fn rejects_mutual_record_cycle() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record a { b: b }\n\
                     record b { a: a }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_invalid_map_key() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { m: map<bool, u32> }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn accepts_map_with_string_key() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     record r { m: map<string, u32> }\n\
                   }";
        let _ = compile(src).expect("map<string, _> is allowed");
    }

    #[test]
    fn rejects_multiple_default_enum_cases() {
        let src = "plugin p version 1;\n\
                   interface i {\n\
                     enum e { @default a, @default b }\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_unknown_import_alias_in_qualified_ref() {
        let src = "plugin p version 1;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i {\n\
                     query q() -> windows.pane-state;\n\
                   }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn accepts_qualified_ref_with_declared_import() {
        // validate() alone (no imports resolved) permits the qualified
        // ref because the alias is declared; validate_with_imports would
        // further resolve the imported type.
        let src = "plugin p version 1;\n\
                   import windows = bmux.tabs;\n\
                   capability I_READ = p.i.read;\n\
                   @capability(I_READ)\n\
                   interface i {\n\
                     query q() -> windows.pane-state;\n\
                   }";
        let _ = compile(src).expect("qualified ref with declared import is allowed");
    }

    #[test]
    fn accepts_qualified_ref_to_same_schema_interface() {
        let src = "plugin p version 1;\n\
                   capability STATE_READ = p.state.read;\n\
                   interface shared { record row { id: uuid } }\n\
                   @capability(STATE_READ)\n\
                   interface state { query q() -> shared.row; }";
        let _ = compile(src).expect("qualified ref to same-schema interface is allowed");
    }

    #[test]
    fn rejects_unknown_type_in_same_schema_interface_ref() {
        let src = "plugin p version 1;\n\
                   capability STATE_READ = p.state.read;\n\
                   interface shared { record row { id: uuid } }\n\
                   @capability(STATE_READ)\n\
                   interface state { query q() -> shared.missing; }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }

    #[test]
    fn rejects_duplicate_import_alias() {
        let src = "plugin p version 1;\n\
                   import a = p1.one;\n\
                   import a = p2.two;\n\
                   interface i { record r { id: uuid } }";
        let err = compile(src).unwrap_err();
        assert!(matches!(err, Error::Validate { .. }));
    }
}
