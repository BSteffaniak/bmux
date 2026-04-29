//! Semantic validation for a parsed BPDL schema.
//!
//! Verifies that:
//! - Plugin id is non-empty.
//! - Type names are unique within an interface.
//! - Variant/enum case names are unique within their parent.
//! - Operation names (queries + commands) are unique within an interface.
//! - All `Named` type references resolve to a declared type in the same
//!   interface. Qualified (`alias.type`) references resolve against
//!   declared imports or same-schema interface names.
//! - `map<K, V>` keys are one of the allowed primitives.
//! - `@default` is used at most once per enum or variant.
//! - An interface declares at most one `events <type>`.
//! - Records/variants contain no direct structural cycles (cycles
//!   through `Option<T>`, `list<T>`, or `map<_, T>` value position are
//!   allowed because the generated Rust compiles).
//! - Import aliases are unique; declared plugin ids must match the
//!   imported schema's `plugin <id>` (when an imports table is supplied).
//! - Capability constant names and ids are unique.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Error,
    ast::{Interface, InterfaceItem, RecordDef, Schema, TypeRef, VariantDef},
};

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
        return Err(Error::Validate {
            message: "plugin id must not be empty".to_string(),
        });
    }

    // Import aliases must be unique, and if a resolved schema is
    // supplied the declared plugin_id must match.
    let mut seen_aliases: BTreeSet<&str> = BTreeSet::new();
    for imp in &schema.imports {
        if !seen_aliases.insert(imp.alias.as_str()) {
            return Err(Error::Validate {
                message: format!("duplicate import alias `{}`", imp.alias),
            });
        }
        if let Some(imported) = imports.get(&imp.alias)
            && imported.plugin.plugin_id != imp.plugin_id
        {
            return Err(Error::Validate {
                message: format!(
                    "import alias `{}` declares `{}` but imported schema is `{}`",
                    imp.alias, imp.plugin_id, imported.plugin.plugin_id
                ),
            });
        }
    }
    let declared_aliases: BTreeSet<&str> =
        schema.imports.iter().map(|i| i.alias.as_str()).collect();
    let schema_type_names = schema_type_names(schema);

    let mut capability_names: BTreeSet<&str> = BTreeSet::new();
    let mut capability_ids: BTreeSet<&str> = BTreeSet::new();
    for capability in &schema.capabilities {
        if !is_rust_const_identifier(&capability.name) {
            return Err(Error::Validate {
                message: format!(
                    "capability constant `{}` is not a valid Rust constant identifier",
                    capability.name
                ),
            });
        }
        if !capability_names.insert(capability.name.as_str()) {
            return Err(Error::Validate {
                message: format!("duplicate capability constant `{}`", capability.name),
            });
        }
        if !capability_ids.insert(capability.id.as_str()) {
            return Err(Error::Validate {
                message: format!("duplicate capability id `{}`", capability.id),
            });
        }
    }

    let declared_capabilities: BTreeSet<&str> = schema
        .capabilities
        .iter()
        .map(|c| c.name.as_str())
        .collect();

    for iface in &schema.interfaces {
        validate_interface(
            iface,
            &declared_aliases,
            imports,
            &schema_type_names,
            &declared_capabilities,
        )?;
    }
    Ok(())
}

fn schema_type_names(schema: &Schema) -> BTreeMap<String, BTreeSet<String>> {
    schema
        .interfaces
        .iter()
        .map(|iface| {
            let names = iface
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

fn validate_interface(
    iface: &Interface,
    declared_aliases: &BTreeSet<&str>,
    imports: &BTreeMap<String, Schema>,
    schema_type_names: &BTreeMap<String, BTreeSet<String>>,
    declared_capabilities: &BTreeSet<&str>,
) -> Result<(), Error> {
    let type_names = collect_and_validate_names(iface)?;
    validate_interface_shape(iface, declared_capabilities)?;
    resolve_type_references(
        iface,
        &type_names,
        declared_aliases,
        imports,
        schema_type_names,
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
        return Err(Error::Validate {
            message: format!(
                "interface `{}` mixes queries and commands; split state and command interfaces",
                iface.name
            ),
        });
    }
    if has_events && (has_queries || has_commands) {
        return Err(Error::Validate {
            message: format!(
                "interface `{}` mixes events with service operations; split event interfaces from services",
                iface.name
            ),
        });
    }

    if has_queries || has_commands {
        let Some(capability) = iface.capability.as_deref() else {
            return Err(Error::Validate {
                message: format!(
                    "service interface `{}` must declare `@capability(NAME)`",
                    iface.name
                ),
            });
        };
        if !declared_capabilities.contains(capability) {
            return Err(Error::Validate {
                message: format!(
                    "interface `{}` references unknown capability `{capability}`",
                    iface.name
                ),
            });
        }
    } else if iface.capability.is_some() {
        return Err(Error::Validate {
            message: format!(
                "interface `{}` declares `@capability` but has no query or command operations",
                iface.name
            ),
        });
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
        match item {
            InterfaceItem::Record(r) => insert_type_name(&mut type_names, &r.name, &iface.name)?,
            InterfaceItem::Variant(v) => {
                insert_type_name(&mut type_names, &v.name, &iface.name)?;
                validate_variant_cases(v)?;
            }
            InterfaceItem::Enum(e) => {
                insert_type_name(&mut type_names, &e.name, &iface.name)?;
                validate_enum_cases(e)?;
            }
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                if !op_names.insert(op.name.clone()) {
                    return Err(Error::Validate {
                        message: format!(
                            "duplicate operation `{}` in interface `{}`",
                            op.name, iface.name
                        ),
                    });
                }
            }
            InterfaceItem::Events(_) => {
                if events_declared {
                    return Err(Error::Validate {
                        message: format!(
                            "interface `{}` declares `events` more than once",
                            iface.name
                        ),
                    });
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
        Err(Error::Validate {
            message: format!("duplicate type `{name}` in interface `{iface_name}`"),
        })
    }
}

fn validate_variant_cases(v: &crate::ast::VariantDef) -> Result<(), Error> {
    let mut case_names = BTreeSet::new();
    let mut defaults = 0;
    for c in &v.cases {
        if !case_names.insert(c.name.clone()) {
            return Err(Error::Validate {
                message: format!("duplicate case `{}` in variant `{}`", c.name, v.name),
            });
        }
        if c.is_default {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(Error::Validate {
            message: format!("variant `{}` has multiple @default cases", v.name),
        });
    }
    Ok(())
}

fn validate_enum_cases(e: &crate::ast::EnumDef) -> Result<(), Error> {
    let mut case_names = BTreeSet::new();
    let mut defaults = 0;
    for c in &e.cases {
        if !case_names.insert(c.name.clone()) {
            return Err(Error::Validate {
                message: format!("duplicate case `{}` in enum `{}`", c.name, e.name),
            });
        }
        if c.is_default {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(Error::Validate {
            message: format!("enum `{}` has multiple @default cases", e.name),
        });
    }
    Ok(())
}

/// Second validation pass: resolve every `TypeRef` in the interface.
fn resolve_type_references(
    iface: &Interface,
    type_names: &BTreeSet<String>,
    declared_aliases: &BTreeSet<&str>,
    imports: &BTreeMap<String, Schema>,
    schema_type_names: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(), Error> {
    for item in &iface.items {
        match item {
            InterfaceItem::Record(r) => {
                for f in &r.fields {
                    check_type(
                        &f.ty,
                        type_names,
                        &iface.name,
                        declared_aliases,
                        imports,
                        schema_type_names,
                    )?;
                }
            }
            InterfaceItem::Variant(v) => {
                for c in &v.cases {
                    for f in &c.payload {
                        check_type(
                            &f.ty,
                            type_names,
                            &iface.name,
                            declared_aliases,
                            imports,
                            schema_type_names,
                        )?;
                    }
                }
            }
            InterfaceItem::Enum(_) => {}
            InterfaceItem::Query(op) | InterfaceItem::Command(op) => {
                for p in &op.params {
                    check_type(
                        &p.ty,
                        type_names,
                        &iface.name,
                        declared_aliases,
                        imports,
                        schema_type_names,
                    )?;
                }
                check_type(
                    &op.returns,
                    type_names,
                    &iface.name,
                    declared_aliases,
                    imports,
                    schema_type_names,
                )?;
            }
            InterfaceItem::Events(decl) => {
                check_type(
                    &decl.ty,
                    type_names,
                    &iface.name,
                    declared_aliases,
                    imports,
                    schema_type_names,
                )?;
            }
        }
    }
    Ok(())
}

fn check_type(
    ty: &TypeRef,
    known: &BTreeSet<String>,
    iface_name: &str,
    declared_aliases: &BTreeSet<&str>,
    imports: &BTreeMap<String, Schema>,
    schema_type_names: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(), Error> {
    match ty {
        TypeRef::Primitive(_) | TypeRef::Unit => Ok(()),
        TypeRef::Named(name) => {
            if known.contains(name) {
                Ok(())
            } else {
                Err(Error::Validate {
                    message: format!(
                        "unknown type `{name}` referenced in interface `{iface_name}`"
                    ),
                })
            }
        }
        TypeRef::Qualified { alias, name } => {
            if declared_aliases.contains(alias.as_str()) {
                // If the caller supplied the resolved schema, confirm the
                // type exists in one of its interfaces.
                if let Some(imported) = imports.get(alias)
                    && !imported_has_type(imported, name)
                {
                    return Err(Error::Validate {
                        message: format!(
                            "imported plugin `{}` (alias `{alias}`) has no type `{name}`",
                            imported.plugin.plugin_id
                        ),
                    });
                }
                return Ok(());
            }

            if let Some(types) = schema_type_names.get(alias) {
                if types.contains(name) {
                    return Ok(());
                }
                return Err(Error::Validate {
                    message: format!(
                        "interface `{alias}` referenced in interface `{iface_name}` has no type `{name}`"
                    ),
                });
            }

            Err(Error::Validate {
                message: format!(
                    "unknown import alias or interface `{alias}` referenced in interface `{iface_name}`"
                ),
            })
        }
        TypeRef::Option(inner) | TypeRef::List(inner) => check_type(
            inner,
            known,
            iface_name,
            declared_aliases,
            imports,
            schema_type_names,
        ),
        TypeRef::Map(key, value) => {
            check_map_key(key, iface_name)?;
            check_type(
                key,
                known,
                iface_name,
                declared_aliases,
                imports,
                schema_type_names,
            )?;
            check_type(
                value,
                known,
                iface_name,
                declared_aliases,
                imports,
                schema_type_names,
            )
        }
        TypeRef::Result(a, b) => {
            check_type(
                a,
                known,
                iface_name,
                declared_aliases,
                imports,
                schema_type_names,
            )?;
            check_type(
                b,
                known,
                iface_name,
                declared_aliases,
                imports,
                schema_type_names,
            )
        }
    }
}

/// Map keys must be one of the `Ord`-able primitives in
/// [`crate::ast::Primitive::is_valid_map_key`].
fn check_map_key(key: &TypeRef, iface_name: &str) -> Result<(), Error> {
    match key {
        TypeRef::Primitive(p) if p.is_valid_map_key() => Ok(()),
        TypeRef::Primitive(p) => Err(Error::Validate {
            message: format!(
                "map key type `{}` is not allowed in interface `{iface_name}`; \
                 map keys must be one of: string, uuid, integer primitives",
                p.keyword()
            ),
        }),
        _ => Err(Error::Validate {
            message: format!(
                "map key must be a primitive type in interface `{iface_name}`; \
                 map keys must be one of: string, uuid, integer primitives"
            ),
        }),
    }
}

fn imported_has_type(schema: &Schema, name: &str) -> bool {
    for iface in &schema.interfaces {
        for item in &iface.items {
            let defined = match item {
                InterfaceItem::Record(r) => &r.name,
                InterfaceItem::Variant(v) => &v.name,
                InterfaceItem::Enum(e) => &e.name,
                _ => continue,
            };
            if defined == name {
                return true;
            }
        }
    }
    false
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
/// heap indirection. Map keys DO keep the edge because a `BTreeMap<K, _>`
/// requires `K: Ord + Clone` — but since we only allow primitive keys,
/// in practice map keys never pull a user type into the graph.
fn collect_required_named(ty: &TypeRef, deps: &mut Vec<String>) {
    match ty {
        TypeRef::Named(name) => deps.push(name.clone()),
        TypeRef::Result(a, b) => {
            collect_required_named(a, deps);
            collect_required_named(b, deps);
        }
        TypeRef::Qualified { .. }
        | TypeRef::Primitive(_)
        | TypeRef::Unit
        | TypeRef::Option(_)
        | TypeRef::List(_)
        | TypeRef::Map(_, _) => {}
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
                   import windows = bmux.windows;\n\
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
