//! Lower the canonical contract IR to WIT (the WebAssembly component
//! interface language).
//!
//! Every BPDL package lowers to one WIT package so a Wasm component tier can
//! be generated mechanically later. BPDL stays the source of truth: WIT
//! cannot express capability ids, retained-state event delivery, secret
//! fields or exact contract identity, so those are carried as
//! documentation in the output. The lowering rules are:
//!
//! | BPDL                         | WIT                                        |
//! |------------------------------|--------------------------------------------|
//! | `plugin a.b version N`       | `package a:b@N.0.0`                        |
//! | `interface x` / `@interface-version(N)` | `interface x` / `interface x-vN` |
//! | `record`, `enum`, `flags`, `resource` | same construct                    |
//! | `variant` case `{ fields }`  | case payload record `<variant>-<case>`     |
//! | `uuid` / `bytes`             | `tuple<u64, u64>` / `list<u8>`             |
//! | `map<K, V>`                  | `list<tuple<K, V>>`                        |
//! | `unit` result / parameter    | no result / `tuple<>` elsewhere            |
//! | `query`/`command f(..) -> T` | `f: func(..) -> T`                         |
//! | `... -> R emits E`           | `f: func(..) -> tuple<stream<E>, future<R>>` |
//! | `events T` (`@state`)        | `subscribe: func() -> stream<T>`           |
//! | `world`: `provide` / `require` | `export` / `import`                      |
//! | `extension-point p version N`| `interface p-vN` with a `data` record      |
//! | plugin `command`s            | `interface plugin-commands`                |
//! | `@since`/`@unstable`/`@deprecated` | the matching WIT feature gates       |
//!
//! Opaque services, bare capabilities and contributions have no WIT form and
//! are emitted as comments.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::Error;
use crate::ir::{
    Delivery, Field, Function, Interface, Item, MethodKind, Package, PackageSet, Primitive,
    RefScope, Stability, Type, TypeDef, TypeDefKind, TypeRef, WorldItem,
};

/// WIT keywords that must be escaped with `%` when used as identifiers.
const KEYWORDS: &[&str] = &[
    "_",
    "as",
    "async",
    "bool",
    "borrow",
    "char",
    "constructor",
    "enum",
    "error-context",
    "export",
    "f32",
    "f64",
    "flags",
    "float32",
    "float64",
    "from",
    "func",
    "future",
    "import",
    "include",
    "interface",
    "list",
    "own",
    "option",
    "package",
    "record",
    "resource",
    "result",
    "s16",
    "s32",
    "s64",
    "s8",
    "static",
    "stream",
    "string",
    "tuple",
    "type",
    "u16",
    "u32",
    "u64",
    "u8",
    "use",
    "variant",
    "with",
    "world",
];

fn wit_error(message: impl Into<String>) -> Error {
    Error::Validate {
        message: message.into(),
    }
}

/// Convert a BPDL name to a WIT kebab-case label (unescaped).
///
/// Lowercases, maps `_` to `-`, collapses repeated separators and prefixes
/// fragments that would start with a digit, since WIT labels are words.
#[must_use]
pub fn label(name: &str) -> String {
    let lowered = name.to_ascii_lowercase().replace('_', "-");
    lowered
        .split('-')
        .filter(|fragment| !fragment.is_empty())
        .enumerate()
        .map(|(index, fragment)| {
            if index == 0 && fragment.starts_with(|c: char| c.is_ascii_digit()) {
                format!("n{fragment}")
            } else {
                fragment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// A label escaped for use as a WIT identifier.
fn ident(name: &str) -> String {
    let label = label(name);
    if KEYWORDS.contains(&label.as_str()) {
        format!("%{label}")
    } else {
        label
    }
}

/// WIT package name for a BPDL package: `namespace:name@N.0.0`.
///
/// The first id segment is the namespace; the rest join into the name.
#[must_use]
pub fn package_name(id: &str, version: u32) -> String {
    let mut segments = id.split('.');
    let namespace = segments.next().map(label).unwrap_or_default();
    let rest = segments.map(label).collect::<Vec<_>>().join("-");
    let name = if rest.is_empty() {
        namespace.clone()
    } else {
        rest
    };
    format!("{namespace}:{name}@{version}.0.0")
}

/// WIT interface name: `name` or `name-vN` for an explicit wire version.
#[must_use]
pub fn interface_name(iface: &Interface) -> String {
    iface.version.map_or_else(
        || label(&iface.name),
        |version| format!("{}-v{version}", label(&iface.name)),
    )
}

fn docs(out: &mut String, indent: &str, lines: &[String]) {
    for line in lines {
        if line.is_empty() {
            let _ = writeln!(out, "{indent}///");
        } else {
            let _ = writeln!(out, "{indent}/// {line}");
        }
    }
}

fn gates(out: &mut String, indent: &str, stability: &Stability) {
    if let Some(feature) = &stability.unstable {
        let _ = writeln!(out, "{indent}@unstable(feature = {})", label(feature));
    }
    if let Some(since) = stability.since {
        let _ = writeln!(out, "{indent}@since(version = {since}.0.0)");
    }
    if let Some(deprecated) = stability.deprecated {
        let _ = writeln!(out, "{indent}@deprecated(version = {deprecated}.0.0)");
    }
}

/// Lower one package to WIT source, resolving imports through `set`.
///
/// # Errors
///
/// Returns an error when a referenced type or interface cannot be resolved
/// or two BPDL names collide after WIT label conversion.
pub fn emit(set: &PackageSet, package: &Package) -> Result<String, Error> {
    let mut out = String::new();
    out.push_str("// AUTO-GENERATED FROM BPDL. DO NOT EDIT BY HAND.\n");
    docs(&mut out, "", &package.docs);
    let _ = writeln!(
        out,
        "package {};\n",
        package_name(&package.id, package.version)
    );

    let mut interface_names = BTreeSet::new();
    let mut claim = |name: String| -> Result<String, Error> {
        if interface_names.insert(name.clone()) {
            Ok(name)
        } else {
            Err(wit_error(format!(
                "package `{}` lowers two interfaces to WIT name `{name}`",
                package.id
            )))
        }
    };
    for iface in &package.interfaces {
        let name = claim(interface_name(iface))?;
        let mut header = Vec::new();
        if let Some(capability) = &iface.capability
            && let Some(id) = package.capability_id(capability)
        {
            header.push(format!("Capability: `{id}`."));
        }
        let mut lowering = InterfaceLowering::new(set, package, iface);
        lowering.interface(&mut out, &name, &header)?;
    }
    for point in &package.extension_points {
        let name = claim(format!("{}-v{}", label(&point.name), point.version))?;
        let data_name = if point.functions.iter().any(|f| label(&f.name) == "data") {
            format!("{}-data", label(&point.name))
        } else {
            "data".to_string()
        };
        let synthetic = Interface {
            name: name.clone(),
            version: None,
            capability: None,
            docs: point.docs.clone(),
            stability: point.stability.clone(),
            items: std::iter::once(Item::Type(TypeDef {
                name: data_name,
                docs: vec!["Declarative data supplied by every contribution.".to_string()],
                stability: Stability::default(),
                kind: TypeDefKind::Record {
                    fields: point.data.clone(),
                },
            }))
            .chain(point.functions.iter().cloned().map(Item::Function))
            .collect(),
        };
        let header = vec![format!("Extension point `{}`.", point.id(&package.id))];
        let mut lowering = InterfaceLowering::new(set, package, &synthetic);
        lowering.interface(&mut out, &name, &header)?;
    }
    if !package.commands.is_empty() {
        let name = claim("plugin-commands".to_string())?;
        let synthetic = commands_interface(package, &name);
        let mut lowering = InterfaceLowering::new(set, package, &synthetic);
        lowering.interface(&mut out, &name, &[])?;
    }
    for world in &package.worlds {
        emit_world(&mut out, set, package, world)?;
    }
    for contribution in &package.contributions {
        let target = &contribution.extension_point;
        let _ = writeln!(
            out,
            "// contributes to `{}{}@{}` ({} declarative field(s))",
            target
                .import
                .as_ref()
                .map(|alias| format!("{alias}."))
                .unwrap_or_default(),
            target.name,
            target.version,
            contribution.values.len()
        );
    }
    Ok(out)
}

/// Plugin commands as one synthetic interface of command functions.
fn commands_interface(package: &Package, name: &str) -> Interface {
    Interface {
        name: name.to_string(),
        version: None,
        capability: None,
        docs: vec!["Plugin commands invocable from the command line and keymaps.".to_string()],
        stability: Stability::default(),
        items: package
            .commands
            .iter()
            .map(|command| {
                let mut docs = command.docs.clone();
                if let Some(title) = &command.title {
                    docs.insert(0, title.clone());
                }
                Item::Function(Function {
                    name: command.name.clone(),
                    kind: crate::ir::FunctionKind::Command,
                    params: command.params.clone(),
                    result: Type::Result(
                        Box::new(Type::Unit),
                        Box::new(Type::Primitive(Primitive::String)),
                    ),
                    emits: None,
                    reentrant: false,
                    docs,
                    stability: command.stability.clone(),
                })
            })
            .collect(),
    }
}

fn emit_world(
    out: &mut String,
    set: &PackageSet,
    package: &Package,
    world: &crate::ir::World,
) -> Result<(), Error> {
    docs(out, "", &world.docs);
    gates(out, "", &world.stability);
    let _ = writeln!(out, "world {} {{", ident(&world.name));
    let mut lines = BTreeSet::new();
    for item in &world.items {
        let line = match item {
            WorldItem::Provide { interface, .. } => {
                format!("export {};", world_interface_path(set, package, interface)?)
            }
            WorldItem::Require { interface, .. } => {
                format!("import {};", world_interface_path(set, package, interface)?)
            }
            WorldItem::ProvideOpaque {
                name,
                version,
                kind,
                capability,
                ..
            } => format!(
                "// opaque {} service `{name}{}` guarded by `{capability}` (not modeled in BPDL)",
                kind.as_str(),
                version.map(|v| format!("/v{v}")).unwrap_or_default()
            ),
            WorldItem::ProvideCapability { capability, .. } => {
                format!("// provides capability `{capability}`")
            }
            WorldItem::RequireCapability {
                capability,
                optional,
                ..
            } => format!(
                "// requires {}capability `{capability}`",
                if *optional { "optional " } else { "" }
            ),
        };
        // WIT rejects duplicate imports; one statement per interface.
        if lines.insert(line.clone()) {
            let _ = writeln!(out, "    {line}");
        }
    }
    if !package.commands.is_empty() {
        out.push_str("    export plugin-commands;\n");
    }
    out.push_str("}\n\n");
    Ok(())
}

/// `iface` for a local interface, `ns:pkg/iface@N.0.0` for an imported one.
fn world_interface_path(
    set: &PackageSet,
    package: &Package,
    reference: &crate::ir::InterfaceRef,
) -> Result<String, Error> {
    let (target_package, local) = match &reference.import {
        None => (package, true),
        Some(alias) => (
            set.imported(package, alias).ok_or_else(|| {
                wit_error(format!(
                    "world in `{}` references unresolved import `{alias}`",
                    package.id
                ))
            })?,
            false,
        ),
    };
    let iface = target_package
        .find_interface(&reference.name, reference.version)
        .ok_or_else(|| {
            wit_error(format!(
                "world in `{}` references unknown interface `{}`",
                package.id, reference.name
            ))
        })?;
    if local {
        Ok(interface_name(iface))
    } else {
        Ok(qualified_interface(target_package, iface))
    }
}

fn qualified_interface(package: &Package, iface: &Interface) -> String {
    let name = package_name(&package.id, package.version);
    let (pkg, version) = name.split_once('@').unwrap_or((&name, ""));
    format!("{pkg}/{}@{version}", interface_name(iface))
}

/// Per-interface lowering state: local names and `use` imports.
struct InterfaceLowering<'a> {
    set: &'a PackageSet,
    package: &'a Package,
    iface: &'a Interface,
    /// WIT names already taken in this interface.
    taken: BTreeSet<String>,
    /// `(source path, type name) -> local WIT name` for `use` statements.
    uses: BTreeMap<(String, String), String>,
    /// Payload records synthesized for variant cases.
    payloads: BTreeMap<(String, String), String>,
}

impl<'a> InterfaceLowering<'a> {
    fn new(set: &'a PackageSet, package: &'a Package, iface: &'a Interface) -> Self {
        Self {
            set,
            package,
            iface,
            taken: BTreeSet::new(),
            uses: BTreeMap::new(),
            payloads: BTreeMap::new(),
        }
    }

    fn take(&mut self, name: String) -> Result<String, Error> {
        if self.taken.insert(name.clone()) {
            Ok(name)
        } else {
            Err(wit_error(format!(
                "interface `{}` lowers two items to WIT name `{name}`",
                self.iface.name
            )))
        }
    }

    /// A fresh name derived from `base`, avoiding every taken name.
    fn fresh(&mut self, base: &str) -> String {
        let mut candidate = base.to_string();
        let mut counter = 1;
        while self.taken.contains(&candidate) {
            counter += 1;
            candidate = format!("{base}-{counter}");
        }
        self.taken.insert(candidate.clone());
        candidate
    }

    fn interface(&mut self, out: &mut String, name: &str, header: &[String]) -> Result<(), Error> {
        // Reserve every local definition first so `use` aliases avoid them.
        for item in &self.iface.items {
            if let Item::Type(def) = item {
                self.take(label(&def.name))?;
            }
        }
        let mut body = String::new();
        for item in &self.iface.items {
            if let Item::Type(def) = item {
                self.type_def(&mut body, def)?;
            }
        }
        for item in &self.iface.items {
            match item {
                Item::Function(function) => self.function(&mut body, function)?,
                Item::Events(events) => {
                    let ty = self.ty(&events.ty)?;
                    let name = self.fresh("subscribe");
                    docs(&mut body, "    ", &events.docs);
                    let _ = writeln!(
                        body,
                        "    /// {} event stream.",
                        match events.delivery {
                            Delivery::Broadcast => {
                                "Broadcast: late subscribers miss earlier emissions"
                            }
                            Delivery::State => {
                                "Retained state: the latest value is replayed to new subscribers"
                            }
                        }
                    );
                    gates(&mut body, "    ", &events.stability);
                    let _ = writeln!(body, "    {}: func() -> stream<{ty}>;", ident(&name));
                }
                Item::Type(_) => {}
            }
        }
        docs(out, "", &self.iface.docs);
        for line in header {
            let _ = writeln!(out, "/// {line}");
        }
        gates(out, "", &self.iface.stability);
        let _ = writeln!(out, "interface {} {{", ident(name));
        let mut grouped: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for ((path, original), local) in &self.uses {
            let entry = if label(original) == *local {
                ident(local)
            } else {
                format!("{} as {}", ident(original), ident(local))
            };
            grouped.entry(path).or_default().push(entry);
        }
        for (path, names) in grouped {
            let _ = writeln!(out, "    use {path}.{{{}}};", names.join(", "));
        }
        out.push_str(&body);
        out.push_str("}\n\n");
        Ok(())
    }

    fn type_def(&mut self, out: &mut String, def: &TypeDef) -> Result<(), Error> {
        let name = ident(&def.name);
        docs(out, "    ", &def.docs);
        gates(out, "    ", &def.stability);
        match &def.kind {
            TypeDefKind::Record { fields } => {
                if fields.is_empty() {
                    // WIT records need a field; an empty record carries no data.
                    let _ = writeln!(out, "    type {name} = tuple<>;");
                } else {
                    let _ = writeln!(out, "    record {name} {{");
                    self.fields(out, fields)?;
                    out.push_str("    }\n");
                }
            }
            TypeDefKind::Variant { cases } => {
                let mut lines = Vec::new();
                let mut payloads = String::new();
                for case in cases {
                    let mut line = String::new();
                    docs(&mut line, "        ", &case.docs);
                    if case.fields.is_empty() {
                        let _ = write!(line, "        {},", ident(&case.name));
                    } else {
                        let payload = self.payload_name(&def.name, &case.name);
                        let _ = writeln!(
                            payloads,
                            "    /// Payload of `{}.{}`.\n    record {} {{",
                            def.name,
                            case.name,
                            ident(&payload)
                        );
                        self.fields(&mut payloads, &case.fields)?;
                        payloads.push_str("    }\n");
                        let _ = write!(line, "        {}({}),", ident(&case.name), ident(&payload));
                    }
                    lines.push(line);
                }
                let _ = writeln!(out, "    variant {name} {{\n{}\n    }}", lines.join("\n"));
                out.push_str(&payloads);
            }
            TypeDefKind::Enum { cases } => {
                let _ = writeln!(out, "    enum {name} {{");
                for case in cases {
                    docs(out, "        ", &case.docs);
                    let _ = writeln!(out, "        {},", ident(&case.name));
                }
                out.push_str("    }\n");
            }
            TypeDefKind::Flags { flags } => {
                let _ = writeln!(out, "    flags {name} {{");
                for flag in flags {
                    docs(out, "        ", &flag.docs);
                    let _ = writeln!(out, "        {},", ident(&flag.name));
                }
                out.push_str("    }\n");
            }
            TypeDefKind::Resource { methods } => {
                if methods.is_empty() {
                    let _ = writeln!(out, "    resource {name};");
                } else {
                    let _ = writeln!(out, "    resource {name} {{");
                    for method in methods {
                        docs(out, "        ", &method.docs);
                        gates(out, "        ", &method.stability);
                        let params = self.params(&method.params)?;
                        let result = match &method.result {
                            Some(result) => self.result(result)?,
                            None => String::new(),
                        };
                        match method.kind {
                            MethodKind::Constructor => {
                                let _ = writeln!(out, "        constructor({params});");
                            }
                            MethodKind::Query | MethodKind::Command => {
                                let _ = writeln!(
                                    out,
                                    "        {}: func({params}){result};",
                                    ident(&method.name)
                                );
                            }
                            MethodKind::StaticQuery | MethodKind::StaticCommand => {
                                let _ = writeln!(
                                    out,
                                    "        {}: static func({params}){result};",
                                    ident(&method.name)
                                );
                            }
                        }
                    }
                    out.push_str("    }\n");
                }
            }
        }
        Ok(())
    }

    fn payload_name(&mut self, variant: &str, case: &str) -> String {
        let key = (variant.to_string(), case.to_string());
        if let Some(existing) = self.payloads.get(&key) {
            return existing.clone();
        }
        let name = self.fresh(&format!("{}-{}", label(variant), label(case)));
        self.payloads.insert(key, name.clone());
        name
    }

    fn fields(&mut self, out: &mut String, fields: &[Field]) -> Result<(), Error> {
        for field in fields {
            docs(out, "        ", &field.docs);
            if field.secret {
                out.push_str("        /// Secret: redact from diagnostics.\n");
            }
            let _ = writeln!(
                out,
                "        {}: {},",
                ident(&field.name),
                self.ty(&field.ty)?
            );
        }
        Ok(())
    }

    fn params(&mut self, params: &[Field]) -> Result<String, Error> {
        let mut rendered = Vec::new();
        for param in params {
            rendered.push(format!("{}: {}", ident(&param.name), self.ty(&param.ty)?));
        }
        Ok(rendered.join(", "))
    }

    /// ` -> T`, or nothing for `unit`.
    fn result(&mut self, ty: &Type) -> Result<String, Error> {
        Ok(match ty {
            Type::Unit => String::new(),
            other => format!(" -> {}", self.ty(other)?),
        })
    }

    fn function(&mut self, out: &mut String, function: &Function) -> Result<(), Error> {
        let name = self.take(label(&function.name)).or_else(|_| {
            // Functions and types share one WIT namespace; BPDL allows a
            // query named after the record it returns.
            Ok::<_, Error>(self.fresh(&format!("{}-{}", label(&function.name), "fn")))
        })?;
        docs(out, "    ", &function.docs);
        if function.reentrant {
            out.push_str(
                "    /// Re-entrant: may be entered while the provider is on the call chain.\n",
            );
        }
        gates(out, "    ", &function.stability);
        let params = self.params(&function.params)?;
        let result = match &function.emits {
            Some(events) => {
                let events = self.ty(events)?;
                let terminal = match &function.result {
                    Type::Unit => "future".to_string(),
                    other => format!("future<{}>", self.ty(other)?),
                };
                format!(" -> tuple<stream<{events}>, {terminal}>")
            }
            None => self.result(&function.result)?,
        };
        let _ = writeln!(out, "    {}: func({params}){result};", ident(&name));
        Ok(())
    }

    fn ty(&mut self, ty: &Type) -> Result<String, Error> {
        Ok(match ty {
            Type::Primitive(primitive) => match primitive {
                Primitive::Bool => "bool".to_string(),
                Primitive::U8 => "u8".to_string(),
                Primitive::U16 => "u16".to_string(),
                Primitive::U32 => "u32".to_string(),
                Primitive::U64 => "u64".to_string(),
                Primitive::I8 => "s8".to_string(),
                Primitive::I16 => "s16".to_string(),
                Primitive::I32 => "s32".to_string(),
                Primitive::I64 => "s64".to_string(),
                Primitive::F32 => "f32".to_string(),
                Primitive::F64 => "f64".to_string(),
                Primitive::Char => "char".to_string(),
                Primitive::String => "string".to_string(),
                Primitive::Bytes => "list<u8>".to_string(),
                Primitive::Uuid => "tuple<u64, u64>".to_string(),
            },
            Type::Unit => "tuple<>".to_string(),
            Type::Option(inner) => format!("option<{}>", self.ty(inner)?),
            Type::List(inner) => format!("list<{}>", self.ty(inner)?),
            Type::Map(key, value) => {
                format!("list<tuple<{}, {}>>", self.ty(key)?, self.ty(value)?)
            }
            Type::Result(ok, error) => match (ok.as_ref(), error.as_ref()) {
                (Type::Unit, Type::Unit) => "result".to_string(),
                (Type::Unit, error) => format!("result<_, {}>", self.ty(error)?),
                (ok, Type::Unit) => format!("result<{}>", self.ty(ok)?),
                (ok, error) => format!("result<{}, {}>", self.ty(ok)?, self.ty(error)?),
            },
            Type::Tuple(items) => {
                let mut rendered = Vec::new();
                for item in items {
                    rendered.push(self.ty(item)?);
                }
                format!("tuple<{}>", rendered.join(", "))
            }
            Type::Stream(inner) => match inner.as_ref() {
                Type::Unit => "stream".to_string(),
                inner => format!("stream<{}>", self.ty(inner)?),
            },
            Type::Future(inner) => match inner.as_ref() {
                Type::Unit => "future".to_string(),
                inner => format!("future<{}>", self.ty(inner)?),
            },
            Type::Ref(reference) => self.reference(reference)?,
            Type::Own(reference) => format!("own<{}>", self.reference(reference)?),
            Type::Borrow(reference) => format!("borrow<{}>", self.reference(reference)?),
        })
    }

    fn reference(&mut self, reference: &TypeRef) -> Result<String, Error> {
        if reference.scope == RefScope::Local {
            if self.iface.type_def(&reference.name).is_none() {
                return Err(wit_error(format!(
                    "interface `{}` references unknown type `{}`",
                    self.iface.name, reference.name
                )));
            }
            return Ok(ident(&reference.name));
        }
        let (defining, owner, def) = self
            .set
            .resolve(self.package, self.iface, reference)
            .or_else(|| {
                // Ambiguous names have no contract identity, but the Rust
                // binding (and therefore the wire) uses the first declaring
                // interface; WIT follows the binding.
                let (defining, candidates): (&Package, Vec<&Interface>) = match &reference.scope {
                    RefScope::Interface(alias) => (
                        self.package,
                        self.package
                            .interfaces
                            .iter()
                            .filter(|candidate| candidate.name == *alias)
                            .collect(),
                    ),
                    RefScope::Import(alias) => {
                        let imported = self.set.imported(self.package, alias)?;
                        (imported, imported.interfaces.iter().collect())
                    }
                    RefScope::Local => return None,
                };
                let owner = candidates
                    .into_iter()
                    .find(|candidate| candidate.type_def(&reference.name).is_some())?;
                Some((defining, owner, owner.type_def(&reference.name)?))
            })
            .ok_or_else(|| {
                wit_error(format!(
                    "interface `{}` references unresolved type `{}`",
                    self.iface.name, reference.name
                ))
            })?;
        let path = if defining.id == self.package.id {
            interface_name(owner)
        } else {
            qualified_interface(defining, owner)
        };
        let key = (path, def.name.clone());
        if let Some(local) = self.uses.get(&key) {
            return Ok(ident(local));
        }
        let preferred = label(&def.name);
        let local = if self.taken.contains(&preferred) {
            self.fresh(&format!("{}-{preferred}", label(&owner.name)))
        } else {
            self.taken.insert(preferred.clone());
            preferred
        };
        self.uses.insert(key, local.clone());
        Ok(ident(&local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_kebab_case_words() {
        assert_eq!(label("pane_runtime"), "pane-runtime");
        assert_eq!(label("TABS_READ"), "tabs-read");
        assert_eq!(label("2d-point"), "n2d-point");
        assert_eq!(ident("stream"), "%stream");
        assert_eq!(
            package_name("bmux.pane_runtime", 1),
            "bmux:pane-runtime@1.0.0"
        );
        assert_eq!(package_name("acme.foo.bar", 2), "acme:foo-bar@2.0.0");
    }
}
