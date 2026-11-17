//! Language-neutral canonical contract IR.
//!
//! The parser produces a syntax tree ([`crate::ast`]) that keeps spans and
//! source spelling for diagnostics. After validation the compiler lowers it
//! into this IR: a span-free, serializable description of one BPDL package
//! (plugin schema) that every backend consumes — contract identity
//! ([`crate::canonical`]), WIT ([`crate::wit`]), plugin manifests
//! ([`crate::manifest`]) and, later, per-language SDK generators.
//!
//! The IR deliberately keeps how a type reference was scoped
//! ([`RefScope`]): canonical identity v2 hashes that scope, and resolution
//! across packages happens through a [`PackageSet`], mirroring how a WIT
//! `Resolve` holds every package of a contract closure.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::ast::Primitive;
use crate::ast::{self, ExtensionOperation, InterfaceItem, ResourceMethodKind, ServiceKindDecl};

/// One lowered `.bpdl` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    /// Dotted package (plugin) id, e.g. `bmux.tabs`.
    pub id: String,
    /// Schema version from `plugin <id> version <n>;`.
    pub version: u32,
    pub docs: Vec<String>,
    pub imports: Vec<Import>,
    pub capabilities: Vec<Capability>,
    pub interfaces: Vec<Interface>,
    pub worlds: Vec<World>,
    pub commands: Vec<Command>,
    pub contributions: Vec<Contribution>,
    pub extension_points: Vec<ExtensionPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub alias: String,
    /// Imported package id.
    pub package: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Constant name used by `@capability(NAME)`.
    pub name: String,
    /// Canonical capability id, e.g. `bmux.tabs.read`.
    pub id: String,
    pub docs: Vec<String>,
}

/// `@since` / `@unstable` / `@deprecated` metadata (never identity-bearing).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stability {
    pub since: Option<u32>,
    pub unstable: Option<String>,
    pub deprecated: Option<u32>,
}

impl Stability {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.since.is_none() && self.unstable.is_none() && self.deprecated.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    /// Explicit wire version from `@interface-version(N)`.
    pub version: Option<u32>,
    /// `@major(N)` for interfaces whose wire id has no `/vN` suffix.
    pub contract_major: Option<u32>,
    /// Capability constant name guarding the service operations.
    pub capability: Option<String>,
    pub docs: Vec<String>,
    pub stability: Stability,
    /// Declaration order is preserved: it can affect wire encoding.
    pub items: Vec<Item>,
}

/// Host service kind a provided interface is routed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Query,
    Command,
    Event,
}

impl ServiceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Command => "command",
            Self::Event => "event",
        }
    }
}

impl Interface {
    /// Routed wire identifier: `name` or `name/vN`.
    #[must_use]
    pub fn wire_id(&self) -> String {
        self.version.map_or_else(
            || self.name.clone(),
            |version| format!("{}/v{version}", self.name),
        )
    }

    /// Contract major version: the explicit wire version, else `@major`,
    /// else 1.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.version.or(self.contract_major).unwrap_or(1)
    }

    /// The service kind implied by the interface's operations, if any.
    /// Validation guarantees an interface never mixes kinds.
    #[must_use]
    pub fn service_kind(&self) -> Option<ServiceKind> {
        self.items.iter().find_map(|item| match item {
            Item::Function(function) => Some(match function.kind {
                FunctionKind::Query => ServiceKind::Query,
                FunctionKind::Command => ServiceKind::Command,
            }),
            Item::Events(_) => Some(ServiceKind::Event),
            Item::Type(_) => None,
        })
    }

    /// Query operations that opted into `@reentrant`.
    pub fn reentrant_operations(&self) -> impl Iterator<Item = &str> {
        self.items.iter().filter_map(|item| match item {
            Item::Function(function) if function.reentrant => Some(function.name.as_str()),
            _ => None,
        })
    }

    #[must_use]
    pub fn type_def(&self, name: &str) -> Option<&TypeDef> {
        self.items.iter().find_map(|item| match item {
            Item::Type(def) if def.name == name => Some(def),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    Type(TypeDef),
    Function(Function),
    Events(Events),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDef {
    pub name: String,
    pub docs: Vec<String>,
    pub stability: Stability,
    pub kind: TypeDefKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeDefKind {
    Record { fields: Vec<Field> },
    Variant { cases: Vec<Case> },
    Enum { cases: Vec<EnumCase> },
    Flags { flags: Vec<Flag> },
    Resource { methods: Vec<Method> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    /// Redact from diagnostics in every SDK. Not identity-bearing.
    pub secret: bool,
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub name: String,
    /// Empty for unit cases.
    pub fields: Vec<Field>,
    pub default: bool,
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumCase {
    pub name: String,
    pub default: bool,
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    pub name: String,
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodKind {
    Constructor,
    Query,
    Command,
    StaticQuery,
    StaticCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Method {
    pub name: String,
    pub kind: MethodKind,
    pub params: Vec<Field>,
    /// `None` for constructors, which return an owned handle.
    pub result: Option<Type>,
    pub docs: Vec<String>,
    pub stability: Stability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionKind {
    Query,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub kind: FunctionKind,
    pub params: Vec<Field>,
    pub result: Type,
    /// Request-scoped event frames before the single terminal result.
    pub emits: Option<Type>,
    pub reentrant: bool,
    pub docs: Vec<String>,
    pub stability: Stability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Broadcast,
    State,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Events {
    pub ty: Type,
    pub delivery: Delivery,
    pub docs: Vec<String>,
    pub stability: Stability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Type {
    Primitive(Primitive),
    Unit,
    Option(Box<Type>),
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Result(Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    Stream(Box<Type>),
    Future(Box<Type>),
    Own(TypeRef),
    Borrow(TypeRef),
    Ref(TypeRef),
}

/// A reference to a named type definition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TypeRef {
    pub scope: RefScope,
    pub name: String,
}

/// Where a referenced type is looked up, relative to the referencing
/// interface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RefScope {
    /// The referencing interface itself.
    Local,
    /// Another interface of the same package.
    Interface(String),
    /// An imported package, by its import alias.
    Import(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct World {
    pub name: String,
    pub docs: Vec<String>,
    pub stability: Stability,
    pub items: Vec<WorldItem>,
}

/// An interface named from a world: local or by import alias.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InterfaceRef {
    pub import: Option<String>,
    pub name: String,
    pub version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorldItem {
    Provide {
        interface: InterfaceRef,
        /// Required exactly when the interface declares no capability.
        capability: Option<String>,
        docs: Vec<String>,
    },
    ProvideOpaque {
        name: String,
        version: Option<u32>,
        kind: ServiceKind,
        capability: String,
        docs: Vec<String>,
    },
    ProvideCapability {
        capability: String,
        docs: Vec<String>,
    },
    Require {
        interface: InterfaceRef,
        optional: bool,
        docs: Vec<String>,
    },
    RequireCapability {
        capability: String,
        optional: bool,
        docs: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub name: String,
    pub params: Vec<Field>,
    pub title: Option<String>,
    pub docs: Vec<String>,
    pub stability: Stability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Literal {
    String(String),
    Int(u64),
    Bool(bool),
    List(Vec<Literal>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionPointRef {
    pub import: Option<String>,
    pub name: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contribution {
    pub extension_point: ExtensionPointRef,
    pub values: Vec<(String, Literal)>,
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionPoint {
    pub name: String,
    pub version: u32,
    pub docs: Vec<String>,
    pub stability: Stability,
    pub data: Vec<Field>,
    pub functions: Vec<Function>,
}

impl ExtensionPoint {
    /// Stable extension point id, e.g. `bmux.tab-bar-item/v1`.
    #[must_use]
    pub fn id(&self, package: &str) -> String {
        format!("{package}.{}/v{}", self.name, self.version)
    }
}

impl Package {
    #[must_use]
    pub fn capability_id(&self, name: &str) -> Option<&str> {
        self.capabilities
            .iter()
            .find(|capability| capability.name == name)
            .map(|capability| capability.id.as_str())
    }

    #[must_use]
    pub fn import(&self, alias: &str) -> Option<&str> {
        self.imports
            .iter()
            .find(|import| import.alias == alias)
            .map(|import| import.package.as_str())
    }

    /// Find `name[@version]` among this package's interfaces. Without a
    /// version the name must be unique.
    #[must_use]
    pub fn find_interface(&self, name: &str, version: Option<u32>) -> Option<&Interface> {
        let mut candidates = self.interfaces.iter().filter(|iface| iface.name == name);
        if let Some(version) = version {
            candidates.find(|iface| iface.version == Some(version))
        } else {
            let first = candidates.next()?;
            candidates.next().is_none().then_some(first)
        }
    }
}

/// Every package of a contract closure, keyed by package id.
///
/// Inserting two different packages under one id makes that id ambiguous:
/// lookups then fail, so identity and lowering never guess.
#[derive(Debug, Clone, Default)]
pub struct PackageSet {
    packages: BTreeMap<String, Option<Package>>,
}

impl PackageSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, package: Package) {
        match self.packages.get_mut(&package.id) {
            Some(existing) => {
                if existing.as_ref() != Some(&package) {
                    *existing = None;
                }
            }
            None => {
                self.packages.insert(package.id.clone(), Some(package));
            }
        }
    }

    /// The unique package with this id, if present and unambiguous.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Package> {
        self.packages.get(id).and_then(Option::as_ref)
    }

    /// Resolve an import alias of `from` to its package.
    #[must_use]
    pub fn imported(&self, from: &Package, alias: &str) -> Option<&Package> {
        self.get(from.import(alias)?)
    }

    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.values().filter_map(Option::as_ref)
    }

    /// Resolve a type reference made from interface `iface` of `package`.
    /// Returns the defining package, interface and definition.
    #[must_use]
    pub fn resolve<'a>(
        &'a self,
        package: &'a Package,
        iface: &'a Interface,
        reference: &TypeRef,
    ) -> Option<(&'a Package, &'a Interface, &'a TypeDef)> {
        match &reference.scope {
            RefScope::Local => Some((package, iface, iface.type_def(&reference.name)?)),
            RefScope::Interface(alias) => {
                let target = unique(
                    package
                        .interfaces
                        .iter()
                        .filter(|candidate| candidate.name == *alias),
                    &reference.name,
                )?;
                Some((package, target, target.type_def(&reference.name)?))
            }
            RefScope::Import(alias) => {
                let imported = self.imported(package, alias)?;
                let target = unique(imported.interfaces.iter(), &reference.name)?;
                Some((imported, target, target.type_def(&reference.name)?))
            }
        }
    }
}

/// The single interface among `candidates` defining `name`.
fn unique<'a>(
    candidates: impl Iterator<Item = &'a Interface>,
    name: &str,
) -> Option<&'a Interface> {
    let mut matching = candidates.filter(|iface| iface.type_def(name).is_some());
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

/// Lower a validated schema into the canonical IR.
#[must_use]
pub fn lower(schema: &ast::Schema) -> Package {
    let aliases: Vec<&str> = schema.imports.iter().map(|i| i.alias.as_str()).collect();
    let cx = Lowering { aliases: &aliases };
    Package {
        id: schema.plugin.plugin_id.clone(),
        version: schema.plugin.version,
        docs: schema.plugin.docs.clone(),
        imports: schema
            .imports
            .iter()
            .map(|import| Import {
                alias: import.alias.clone(),
                package: import.plugin_id.clone(),
            })
            .collect(),
        capabilities: schema
            .capabilities
            .iter()
            .map(|capability| Capability {
                name: capability.name.clone(),
                id: capability.id.clone(),
                docs: capability.docs.clone(),
            })
            .collect(),
        interfaces: schema
            .interfaces
            .iter()
            .map(|iface| cx.interface(iface))
            .collect(),
        worlds: schema.worlds.iter().map(world).collect(),
        commands: schema
            .commands
            .iter()
            .map(|command| Command {
                name: command.name.clone(),
                params: cx.fields(&command.params),
                title: command.title.clone(),
                docs: command.docs.clone(),
                stability: stability(&command.stability),
            })
            .collect(),
        contributions: schema
            .contributions
            .iter()
            .map(|contribution| Contribution {
                extension_point: ExtensionPointRef {
                    import: contribution.extension_point.alias.clone(),
                    name: contribution.extension_point.name.clone(),
                    version: contribution.extension_point.version,
                },
                values: contribution
                    .values
                    .iter()
                    .map(|(name, value)| (name.clone(), literal(value)))
                    .collect(),
                docs: contribution.docs.clone(),
            })
            .collect(),
        extension_points: schema
            .extension_points
            .iter()
            .map(|point| ExtensionPoint {
                name: point.name.clone(),
                version: point.version,
                docs: point.docs.clone(),
                stability: stability(&point.stability),
                data: cx.fields(&point.data),
                functions: point
                    .operations
                    .iter()
                    .map(|operation| match operation {
                        ExtensionOperation::Query(op) => cx.function(op, FunctionKind::Query),
                        ExtensionOperation::Command(op) => cx.function(op, FunctionKind::Command),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn stability(value: &ast::Stability) -> Stability {
    Stability {
        since: value.since,
        unstable: value.unstable.clone(),
        deprecated: value.deprecated,
    }
}

fn literal(value: &ast::Literal) -> Literal {
    match value {
        ast::Literal::String(value) => Literal::String(value.clone()),
        ast::Literal::Int(value) => Literal::Int(*value),
        ast::Literal::Bool(value) => Literal::Bool(*value),
        ast::Literal::List(items) => Literal::List(items.iter().map(literal).collect()),
    }
}

fn interface_ref(reference: &ast::InterfaceRef) -> InterfaceRef {
    InterfaceRef {
        import: reference.alias.clone(),
        name: reference.name.clone(),
        version: reference.version,
    }
}

const fn service_kind(kind: ServiceKindDecl) -> ServiceKind {
    match kind {
        ServiceKindDecl::Query => ServiceKind::Query,
        ServiceKindDecl::Command => ServiceKind::Command,
        ServiceKindDecl::Events => ServiceKind::Event,
    }
}

fn world(value: &ast::World) -> World {
    World {
        name: value.name.clone(),
        docs: value.docs.clone(),
        stability: stability(&value.stability),
        items: value
            .items
            .iter()
            .map(|item| match item {
                ast::WorldItem::Provide {
                    docs,
                    interface,
                    capability,
                    ..
                } => WorldItem::Provide {
                    interface: interface_ref(interface),
                    capability: capability.clone(),
                    docs: docs.clone(),
                },
                ast::WorldItem::ProvideOpaque {
                    docs,
                    name,
                    version,
                    kind,
                    capability,
                    ..
                } => WorldItem::ProvideOpaque {
                    name: name.clone(),
                    version: *version,
                    kind: service_kind(*kind),
                    capability: capability.clone(),
                    docs: docs.clone(),
                },
                ast::WorldItem::ProvideCapability {
                    docs, capability, ..
                } => WorldItem::ProvideCapability {
                    capability: capability.clone(),
                    docs: docs.clone(),
                },
                ast::WorldItem::Require {
                    docs,
                    optional,
                    interface,
                    ..
                } => WorldItem::Require {
                    interface: interface_ref(interface),
                    optional: *optional,
                    docs: docs.clone(),
                },
                ast::WorldItem::RequireCapability {
                    docs,
                    optional,
                    capability,
                    ..
                } => WorldItem::RequireCapability {
                    capability: capability.clone(),
                    optional: *optional,
                    docs: docs.clone(),
                },
            })
            .collect(),
    }
}

struct Lowering<'a> {
    aliases: &'a [&'a str],
}

impl Lowering<'_> {
    fn interface(&self, iface: &ast::Interface) -> Interface {
        Interface {
            name: iface.name.clone(),
            version: iface.interface_version,
            contract_major: iface.major,
            capability: iface.capability.clone(),
            docs: iface.docs.clone(),
            stability: stability(&iface.stability),
            items: iface.items.iter().map(|item| self.item(item)).collect(),
        }
    }

    fn item(&self, item: &InterfaceItem) -> Item {
        match item {
            InterfaceItem::Record(record) => Item::Type(TypeDef {
                name: record.name.clone(),
                docs: record.docs.clone(),
                stability: stability(&record.stability),
                kind: TypeDefKind::Record {
                    fields: self.fields(&record.fields),
                },
            }),
            InterfaceItem::Variant(variant) => Item::Type(TypeDef {
                name: variant.name.clone(),
                docs: variant.docs.clone(),
                stability: stability(&variant.stability),
                kind: TypeDefKind::Variant {
                    cases: variant
                        .cases
                        .iter()
                        .map(|case| Case {
                            name: case.name.clone(),
                            fields: self.fields(&case.payload),
                            default: case.is_default,
                            docs: case.docs.clone(),
                        })
                        .collect(),
                },
            }),
            InterfaceItem::Enum(enumeration) => Item::Type(TypeDef {
                name: enumeration.name.clone(),
                docs: enumeration.docs.clone(),
                stability: stability(&enumeration.stability),
                kind: TypeDefKind::Enum {
                    cases: enumeration
                        .cases
                        .iter()
                        .map(|case| EnumCase {
                            name: case.name.clone(),
                            default: case.is_default,
                            docs: case.docs.clone(),
                        })
                        .collect(),
                },
            }),
            InterfaceItem::Flags(flags) => Item::Type(TypeDef {
                name: flags.name.clone(),
                docs: flags.docs.clone(),
                stability: stability(&flags.stability),
                kind: TypeDefKind::Flags {
                    flags: flags
                        .flags
                        .iter()
                        .map(|flag| Flag {
                            name: flag.name.clone(),
                            docs: flag.docs.clone(),
                        })
                        .collect(),
                },
            }),
            InterfaceItem::Resource(resource) => Item::Type(TypeDef {
                name: resource.name.clone(),
                docs: resource.docs.clone(),
                stability: stability(&resource.stability),
                kind: TypeDefKind::Resource {
                    methods: resource
                        .methods
                        .iter()
                        .map(|method| Method {
                            name: method.name.clone(),
                            kind: match method.kind {
                                ResourceMethodKind::Constructor => MethodKind::Constructor,
                                ResourceMethodKind::Query => MethodKind::Query,
                                ResourceMethodKind::Command => MethodKind::Command,
                                ResourceMethodKind::StaticQuery => MethodKind::StaticQuery,
                                ResourceMethodKind::StaticCommand => MethodKind::StaticCommand,
                            },
                            params: self.fields(&method.params),
                            result: method.returns.as_ref().map(|ty| self.ty(ty)),
                            docs: method.docs.clone(),
                            stability: stability(&method.stability),
                        })
                        .collect(),
                },
            }),
            InterfaceItem::Query(op) => Item::Function(self.function(op, FunctionKind::Query)),
            InterfaceItem::Command(op) => Item::Function(self.function(op, FunctionKind::Command)),
            InterfaceItem::Events(events) => Item::Events(Events {
                ty: self.ty(&events.ty),
                delivery: match events.delivery {
                    ast::DeliveryMode::Broadcast => Delivery::Broadcast,
                    ast::DeliveryMode::State => Delivery::State,
                },
                docs: events.docs.clone(),
                stability: stability(&events.stability),
            }),
        }
    }

    fn function(&self, op: &ast::Operation, kind: FunctionKind) -> Function {
        Function {
            name: op.name.clone(),
            kind,
            params: self.fields(&op.params),
            result: self.ty(&op.returns),
            emits: op.emits.as_ref().map(|ty| self.ty(ty)),
            reentrant: op.reentrant,
            docs: op.docs.clone(),
            stability: stability(&op.stability),
        }
    }

    fn fields(&self, fields: &[ast::Field]) -> Vec<Field> {
        fields
            .iter()
            .map(|field| Field {
                name: field.name.clone(),
                ty: self.ty(&field.ty),
                secret: field.secret,
                docs: field.docs.clone(),
            })
            .collect()
    }

    fn reference(&self, ty: &ast::TypeRef) -> TypeRef {
        match ty {
            ast::TypeRef::Named(name) => TypeRef {
                scope: RefScope::Local,
                name: name.clone(),
            },
            ast::TypeRef::Qualified { alias, name } => TypeRef {
                // An import alias shadows a same-named local interface.
                scope: if self.aliases.contains(&alias.as_str()) {
                    RefScope::Import(alias.clone())
                } else {
                    RefScope::Interface(alias.clone())
                },
                name: name.clone(),
            },
            // Validation only admits named handles.
            other => TypeRef {
                scope: RefScope::Local,
                name: format!("{other:?}"),
            },
        }
    }

    fn ty(&self, ty: &ast::TypeRef) -> Type {
        let boxed = |inner: &ast::TypeRef| Box::new(self.ty(inner));
        match ty {
            ast::TypeRef::Primitive(primitive) => Type::Primitive(*primitive),
            ast::TypeRef::Unit => Type::Unit,
            ast::TypeRef::Named(_) | ast::TypeRef::Qualified { .. } => {
                Type::Ref(self.reference(ty))
            }
            ast::TypeRef::Option(inner) => Type::Option(boxed(inner)),
            ast::TypeRef::List(inner) => Type::List(boxed(inner)),
            ast::TypeRef::Map(key, value) => Type::Map(boxed(key), boxed(value)),
            ast::TypeRef::Result(ok, error) => Type::Result(boxed(ok), boxed(error)),
            ast::TypeRef::Tuple(items) => {
                Type::Tuple(items.iter().map(|item| self.ty(item)).collect())
            }
            ast::TypeRef::Stream(inner) => Type::Stream(boxed(inner)),
            ast::TypeRef::Future(inner) => Type::Future(boxed(inner)),
            ast::TypeRef::Own(inner) => Type::Own(self.reference(inner)),
            ast::TypeRef::Borrow(inner) => Type::Borrow(self.reference(inner)),
        }
    }
}
