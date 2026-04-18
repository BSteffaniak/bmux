//! Typed AST for a BPDL schema.
//!
//! A [`Schema`] represents a complete `.bpdl` file. It carries the plugin
//! header (`plugin <id> version <n>;`) and a list of [`Interface`] blocks.
//! Each interface defines user types (records, variants, enums) and
//! operations (queries, commands, and at most one event stream).

use crate::Span;

/// Complete parsed schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub plugin: PluginHeader,
    pub interfaces: Vec<Interface>,
}

/// The plugin header: `plugin <dotted.id> version <n>;`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHeader {
    pub plugin_id: String,
    pub version: u32,
    pub span: Span,
}

/// An `interface <name> { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub items: Vec<InterfaceItem>,
    pub span: Span,
}

/// Items allowed inside an `interface { ... }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceItem {
    Record(RecordDef),
    Variant(VariantDef),
    Enum(EnumDef),
    Query(Operation),
    Command(Operation),
    /// Declares the event type emitted by this interface's event stream.
    /// An interface has at most one event declaration.
    Events(TypeRef),
}

/// A `record <name> { field: type, ... }` definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDef {
    pub name: String,
    pub fields: Vec<Field>,
    pub span: Span,
}

/// A single field inside a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: TypeRef,
    pub span: Span,
}

/// A `variant <name> { case-a, case-b { field: type }, ... }` definition.
/// Variants are tagged unions; cases may carry payload fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantDef {
    pub name: String,
    pub cases: Vec<VariantCase>,
    pub span: Span,
}

/// A single case in a variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantCase {
    pub name: String,
    /// Empty for unit cases (`focused`), non-empty for struct-like cases
    /// (`exited { code: i32 }`).
    pub payload: Vec<Field>,
    pub span: Span,
}

/// An `enum <name> { case-a, case-b }` definition. Enums are unit-only
/// tagged sets with no payload; use `variant` if payload is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDef {
    pub name: String,
    pub cases: Vec<EnumCase>,
    pub span: Span,
}

/// A single case in an enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumCase {
    pub name: String,
    pub span: Span,
}

/// A `query` or `command` declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub name: String,
    pub params: Vec<Field>,
    pub returns: TypeRef,
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
    /// `T?` — nullable.
    Option(Box<TypeRef>),
    /// `list<T>` — variable-length sequence.
    List(Box<TypeRef>),
    /// `result<T, E>` — typed success/error.
    Result(Box<TypeRef>, Box<TypeRef>),
    /// `unit` — empty payload / void.
    Unit,
}

/// Primitive scalar types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            _ => return None,
        })
    }
}
