//! Typed AST for a BPDL schema.
//!
//! A [`Schema`] represents a complete `.bpdl` file. It carries the plugin
//! header (`plugin <id> version <n>;`), optional [`Import`]s referencing
//! other plugin schemas, and a list of [`Interface`] blocks. Each
//! interface defines user types (records, variants, enums) and
//! operations (queries, commands, and at most one event stream).

use crate::Span;

/// Complete parsed schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub plugin: PluginHeader,
    /// `import <alias> = <plugin.id>;` directives. Resolved against a
    /// caller-provided imports table at validation and codegen time.
    pub imports: Vec<Import>,
    /// `capability <NAME> = <capability.id>;` declarations emitted as
    /// typed [`bmux_plugin_sdk::CapabilityId`] constants.
    pub capabilities: Vec<CapabilityDecl>,
    pub interfaces: Vec<Interface>,
    /// `world <name> { ... }` blocks: what this plugin provides and requires.
    pub worlds: Vec<World>,
    /// Plugin-level `command <name>(...)` declarations (CLI commands).
    pub commands: Vec<PluginCommand>,
    /// Declarative `contribute <extension-point> { ... };` data.
    pub contributions: Vec<Contribution>,
    /// Host- or plugin-declared `extension-point <name> version <n> { ... }`.
    pub extension_points: Vec<ExtensionPoint>,
}

/// `///` documentation lines attached to the following declaration.
///
/// Documentation is metadata only: it never enters contract identity.
pub type Docs = Vec<String>;

/// Feature-gate and lifecycle annotations (`@since`, `@unstable`,
/// `@deprecated`). Like documentation they are metadata only and never
/// enter contract identity; they lower to the matching WIT gates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stability {
    /// `@since(N)`: the package version that introduced the item.
    pub since: Option<u32>,
    /// `@unstable(feature-name)`: the item exists only behind a feature.
    pub unstable: Option<String>,
    /// `@deprecated(N)`: the package version that deprecated the item.
    pub deprecated: Option<u32>,
}

impl Stability {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.since.is_none() && self.unstable.is_none() && self.deprecated.is_none()
    }
}

/// The plugin header: `plugin <dotted.id> version <n>;`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHeader {
    pub docs: Docs,
    pub plugin_id: String,
    pub version: u32,
    pub span: Span,
}

/// An `import <alias> = <plugin.id>;` declaration at the top of a schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// Local alias used in qualified type references (`windows.pane-state`).
    pub alias: String,
    /// The imported plugin's `plugin <id>` value.
    pub plugin_id: String,
    pub span: Span,
}

/// A named capability constant declared at schema top level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDecl {
    pub docs: Docs,
    /// Rust constant name emitted under `pub mod capabilities`.
    pub name: String,
    /// Canonical capability id, for example `bmux.foo.read`.
    pub id: String,
    pub span: Span,
}

/// An `interface <name> { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    /// Optional wire-version suffix for the generated service identifier.
    /// `@interface-version(1) interface foo` produces `foo/v1` while
    /// retaining `foo` as the source/module name.
    pub interface_version: Option<u32>,
    /// Capability constant required to invoke this service interface.
    /// Present only for query-only or command-only interfaces.
    pub capability: Option<String>,
    pub items: Vec<InterfaceItem>,
    pub span: Span,
}

/// Items allowed inside an `interface { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceItem {
    Record(RecordDef),
    Variant(VariantDef),
    Enum(EnumDef),
    /// `flags <name> { a, b }`: a set of named booleans.
    Flags(FlagsDef),
    /// `resource <name> { ... }`: a handle type with methods.
    Resource(ResourceDef),
    Query(Operation),
    Command(Operation),
    /// Declares the event type emitted by this interface's event stream.
    /// An interface has at most one event declaration.
    Events(EventsDecl),
}

/// An event-stream declaration. The delivery mode distinguishes
/// one-shot broadcast events (default) from state channels that
/// retain the latest value and replay it to late subscribers.
///
/// - `@state events T;` → [`DeliveryMode::State`]
/// - `events T;` → [`DeliveryMode::Broadcast`] (default)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsDecl {
    pub docs: Docs,
    pub stability: Stability,
    pub ty: TypeRef,
    pub delivery: DeliveryMode,
    pub span: Span,
}

/// Delivery semantics for an interface's event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    /// Classic broadcast: emissions fan out to current subscribers.
    /// Late subscribers miss prior emissions. Suitable for transient
    /// events (bell, recording-started, per-tick updates).
    #[default]
    Broadcast,
    /// Reactive state channel: the most-recently-published value is
    /// retained and replayed synchronously to new subscribers before
    /// any live updates. Suitable for shared state (focused pane,
    /// zoom status, session list).
    State,
}

/// A `record <name> { field: type, ... }` definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDef {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

/// A single field inside a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub docs: Docs,
    /// Diagnostic-only: redact Debug without altering serialization.
    pub secret: bool,
    pub name: String,
    pub ty: TypeRef,
    pub span: Span,
}

/// A `variant <name> { case-a, case-b { field: type }, ... }` definition.
/// Variants are tagged unions; cases may carry payload fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDef {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub cases: Vec<VariantCase>,
    pub span: Span,
}

/// A single case in a variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantCase {
    pub docs: Docs,
    pub name: String,
    /// Empty for unit cases (`focused`), non-empty for struct-like cases
    /// (`exited { code: i32 }`).
    pub payload: Vec<Field>,
    /// Marked `@default`. Only legal for unit cases.
    pub is_default: bool,
    pub span: Span,
}

/// An `enum <name> { case-a, case-b }` definition. Enums are unit-only
/// tagged sets with no payload; use `variant` if payload is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDef {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub cases: Vec<EnumCase>,
    pub span: Span,
}

/// A single case in an enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumCase {
    pub docs: Docs,
    pub name: String,
    /// Marked `@default`. At most one per enum.
    pub is_default: bool,
    pub span: Span,
}

/// A `query`, `command`, or request-scoped streaming `command ... emits ...`
/// declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub params: Vec<Field>,
    pub returns: TypeRef,
    /// Event frame type emitted by a request-scoped streaming operation.
    ///
    /// This is distinct from [`InterfaceItem::Events`], which models ambient
    /// interface event streams. `emits` belongs to the invocation that started
    /// the operation and must produce zero or more typed event frames followed
    /// by exactly one final response frame.
    pub emits: Option<TypeRef>,
    /// `@reentrant` contract opt-in: the provider promises this query may be
    /// entered while another callback entry of the same plugin instance is
    /// still active on the same call chain (stateless/read-only, takes no
    /// instance-exclusive locks, never blocks on outbound calls). Only legal on
    /// non-streaming `query` operations. It changes concurrency semantics, so
    /// it is part of the canonical contract identity when set.
    pub reentrant: bool,
    pub span: Span,
}

/// A type reference. Either a primitive, a user-defined name, a container,
/// an option, or a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    /// Built-in primitive types.
    Primitive(Primitive),
    /// Reference to a record/variant/enum defined in the same interface.
    Named(String),
    /// Qualified reference to a type in an imported schema:
    /// `<alias>.<type-name>`.
    Qualified { alias: String, name: String },
    /// `T?` — nullable.
    Option(Box<TypeRef>),
    /// `list<T>` — variable-length sequence.
    List(Box<TypeRef>),
    /// `map<K, V>` — keyed collection. Lowered to `BTreeMap<K, V>` in
    /// Rust codegen to guarantee deterministic iteration order.
    Map(Box<TypeRef>, Box<TypeRef>),
    /// `result<T, E>` — typed success/error.
    Result(Box<TypeRef>, Box<TypeRef>),
    /// `unit` — empty payload / void.
    Unit,
    /// `tuple<A, B, ...>` — positional product type.
    Tuple(Vec<TypeRef>),
    /// `stream<T>` — zero or more values delivered asynchronously.
    Stream(Box<TypeRef>),
    /// `future<T>` — exactly one value delivered asynchronously.
    Future(Box<TypeRef>),
    /// `own<R>` — an owned handle to resource `R`. A bare reference to a
    /// resource type means the same thing.
    Own(Box<TypeRef>),
    /// `borrow<R>` — a handle to resource `R` borrowed for one call.
    Borrow(Box<TypeRef>),
}

/// Primitive scalar types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Primitive {
    Bool,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    String,
    Bytes,
    Uuid,
    Char,
}

impl Primitive {
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::String => "string",
            Self::Bytes => "bytes",
            Self::Uuid => "uuid",
            Self::Char => "char",
        }
    }

    #[must_use]
    pub fn from_keyword(keyword: &str) -> Option<Self> {
        Some(match keyword {
            "bool" => Self::Bool,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "f32" => Self::F32,
            "f64" => Self::F64,
            "string" => Self::String,
            "bytes" => Self::Bytes,
            "uuid" => Self::Uuid,
            "char" => Self::Char,
            _ => return None,
        })
    }

    /// Is this primitive a legal `map<K, _>` key type?
    ///
    /// Allowed: string, uuid, all integer primitives. Disallowed:
    /// `bool` (tiny domain, usually a mistake), floats (not `Ord`),
    /// `bytes` (large keys in RPC payloads is almost always wrong).
    #[must_use]
    pub const fn is_valid_map_key(self) -> bool {
        matches!(
            self,
            Self::U8
                | Self::U16
                | Self::U32
                | Self::U64
                | Self::I8
                | Self::I16
                | Self::I32
                | Self::I64
                | Self::String
                | Self::Uuid
        )
    }
}

/// A `flags <name> { a, b, c }` definition: a set of independent booleans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagsDef {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub flags: Vec<FlagDef>,
    pub span: Span,
}

/// One named flag in a [`FlagsDef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagDef {
    pub docs: Docs,
    pub name: String,
    pub span: Span,
}

/// A `resource <name> { ... }` definition. Values of a resource type are
/// handles (`own<R>` / `borrow<R>`), never copied state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDef {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub methods: Vec<ResourceMethod>,
    pub span: Span,
}

/// How a resource method is invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMethodKind {
    /// `constructor(params);` — returns a new owned handle.
    Constructor,
    /// `query name(params) -> T;` — read-only method on a borrowed handle.
    Query,
    /// `command name(params) -> T;` — mutating method on a borrowed handle.
    Command,
    /// `static query name(params) -> T;` — no receiver, read-only.
    StaticQuery,
    /// `static command name(params) -> T;` — no receiver, mutating.
    StaticCommand,
}

/// One method of a [`ResourceDef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMethod {
    pub docs: Docs,
    pub stability: Stability,
    pub kind: ResourceMethodKind,
    /// `constructor` for constructors.
    pub name: String,
    pub params: Vec<Field>,
    /// `None` for constructors.
    pub returns: Option<TypeRef>,
    pub span: Span,
}

/// A `world <name> { ... }` block. Worlds list what the plugin provides
/// (exports) and what it requires from the host or other plugins (imports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub items: Vec<WorldItem>,
    pub span: Span,
}

/// Reference to an interface from a world: `name` or `alias.name`, with an
/// optional `@N` wire version selecting one member of a versioned family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceRef {
    /// Import alias, or `None` for an interface of this schema.
    pub alias: Option<String>,
    pub name: String,
    pub version: Option<u32>,
    pub span: Span,
}

/// Service kind of an opaque (not yet BPDL-modeled) provided service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKindDecl {
    Query,
    Command,
    Events,
}

/// One entry in a [`World`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldItem {
    /// `provide <iface> [capability <id>];` — the plugin serves this
    /// interface. A capability may only be given for interfaces that do
    /// not declare one (event interfaces), and is then required.
    Provide {
        docs: Docs,
        interface: InterfaceRef,
        capability: Option<String>,
        span: Span,
    },
    /// `provide opaque <name>[@N]: <kind> capability <id>;` — a routed
    /// service whose operations are not modeled in BPDL yet. It carries no
    /// contract identity and has no WIT lowering.
    ProvideOpaque {
        docs: Docs,
        name: String,
        version: Option<u32>,
        kind: ServiceKindDecl,
        capability: String,
        span: Span,
    },
    /// `provide capability <id>;` — a capability granted by this plugin
    /// that is not tied to a service interface.
    ProvideCapability {
        docs: Docs,
        capability: String,
        span: Span,
    },
    /// `require [optional] <iface>;` — the plugin calls this interface.
    Require {
        docs: Docs,
        optional: bool,
        interface: InterfaceRef,
        span: Span,
    },
    /// `require [optional] capability <id>;` — a host capability.
    RequireCapability {
        docs: Docs,
        optional: bool,
        capability: String,
        span: Span,
    },
}

/// A plugin-level `command <name>(params) [title "..."];` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub params: Vec<Field>,
    pub title: Option<String>,
    pub span: Span,
}

/// A literal value in declarative contribution data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    String(String),
    Int(u64),
    Bool(bool),
    List(Vec<Literal>),
}

/// Reference to an extension point: `[alias.]name@version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPointRef {
    pub alias: Option<String>,
    pub name: String,
    pub version: u32,
    pub span: Span,
}

/// `contribute <extension-point>@N { field: literal, ... };` — declarative
/// data visible before activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub docs: Docs,
    pub extension_point: ExtensionPointRef,
    pub values: Vec<(String, Literal)>,
    pub span: Span,
}

/// `extension-point <name> version <n> { data { ... } interface { ... } }`.
/// Every contribution supplies the data record; contributing plugins
/// implement the interface operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPoint {
    pub docs: Docs,
    pub stability: Stability,
    pub name: String,
    pub version: u32,
    pub data: Vec<Field>,
    pub operations: Vec<ExtensionOperation>,
    pub span: Span,
}

/// An operation in an extension point's `interface { ... }` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionOperation {
    Query(Operation),
    Command(Operation),
}
