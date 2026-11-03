//! Path-tracking serde adapter over [`ConfigValue`] that keeps only structural
//! diagnostics. Unexpected values, custom messages and offending variant names are
//! discarded the moment serde reports them; only keys, indices, and the target
//! type's own expectations survive.
use crate::{ConfigValue, Document, Error, ErrorKind, PathSegment, ValuePath};
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, Expected, MapAccess, SeqAccess,
    Unexpected, VariantAccess, Visitor,
};
use std::cell::Cell;
use std::sync::OnceLock;

/// Structural serde failure category. Debug prints only the category name.
///
/// Fields hold target-type descriptions (serde "expected" texts and field or
/// variant name lists) and, for [`DecodeDetail::UnknownField`], the rejected key.
/// Keys are explicit trusted access like [`crate::Source::layer`]; configuration
/// values, serde's "unexpected" descriptions, offending variant names and custom
/// error messages are never retained.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeDetail {
    /// A table key is not accepted by the target type.
    UnknownField {
        /// The rejected key.
        field: String,
        /// Field names the target type accepts.
        expected: Vec<&'static str>,
    },
    /// A required field is absent from the table at the failure path.
    MissingField {
        /// Name of the absent field.
        field: String,
    },
    /// The value has the wrong variant (for example a string where an integer is
    /// expected).
    InvalidType {
        /// The target type's description of what it accepts.
        expected: String,
    },
    /// The value has the right variant but is rejected (for example out of range).
    InvalidValue {
        /// The target type's description of what it accepts.
        expected: String,
    },
    /// An array or table has an unacceptable number of entries.
    InvalidLength {
        /// The target type's description of the accepted length.
        expected: String,
    },
    /// An enum variant name is not recognized; the offending name is discarded.
    /// A variant spelled as a table key (`{ variant = ... }`) remains visible as
    /// the final key of [`DecodeFailure::path`], like any other key.
    UnknownVariant {
        /// Variant names the target type accepts.
        expected: Vec<&'static str>,
    },
    /// Any other failure, including custom `Deserialize` messages, which are discarded.
    Other,
}
impl std::fmt::Debug for DecodeDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.category())
    }
}
impl DecodeDetail {
    fn category(&self) -> &'static str {
        match self {
            Self::UnknownField { .. } => "UnknownField",
            Self::MissingField { .. } => "MissingField",
            Self::InvalidType { .. } => "InvalidType",
            Self::InvalidValue { .. } => "InvalidValue",
            Self::InvalidLength { .. } => "InvalidLength",
            Self::UnknownVariant { .. } => "UnknownVariant",
            Self::Other => "Other",
        }
    }
}
/// Located decode failure from [`Document::decode_located`], boxed because it is
/// returned only on the cold failure path.
///
/// `error.kind` is [`ErrorKind::Decode`], or [`ErrorKind::Limit`] for excessive
/// depth; `error.source` is the origin of the node at `path` or its nearest recorded
/// ancestor. `path` is best effort: serde adapters that buffer content (for
/// example `flatten` or untagged enums) report the buffering node. Debug and
/// Display redact the path and detail fields, which are explicit trusted access.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodeFailure {
    /// Secret-safe category and origin of the failing node.
    pub error: Error,
    /// Root-relative path of the failing node; empty means root.
    pub path: ValuePath,
    /// Structural serde category without configuration values.
    pub detail: DecodeDetail,
}
impl std::fmt::Debug for DecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeFailure")
            .field("error", &self.error)
            .field("detail", &self.detail)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.error, self.detail)
    }
}
impl std::error::Error for DecodeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(crate) fn decode<T: DeserializeOwned>(
    document: &Document,
) -> std::result::Result<T, Box<DecodeFailure>> {
    let fail = |kind, path: ValuePath, detail| {
        Box::new(DecodeFailure {
            error: Error {
                source: document.origin(&path),
                ..Error::new(kind)
            },
            path,
            detail,
        })
    };
    // Same precondition order as `Document::decode`: depth, then datetime validity.
    if crate::value::validate_value(&document.value, 128, usize::MAX).is_err() {
        return Err(fail(ErrorKind::Limit, Vec::new(), DecodeDetail::Other));
    }
    if let Some(mut path) = invalid_datetime(&document.value) {
        path.reverse();
        return Err(fail(ErrorKind::Decode, path, DecodeDetail::Other));
    }
    T::deserialize(Node::Value(&document.value)).map_err(|error| {
        let mut path = error.reversed;
        path.reverse();
        fail(ErrorKind::Decode, path, error.detail)
    })
}
/// Reversed path of the first unparsable datetime; recursion is bounded by the
/// preceding depth validation.
fn invalid_datetime(value: &ConfigValue) -> Option<ValuePath> {
    match value {
        ConfigValue::Datetime(text) => text
            .parse::<toml::value::Datetime>()
            .is_err()
            .then(Vec::new),
        ConfigValue::Array(values) => values.iter().enumerate().find_map(|(index, value)| {
            invalid_datetime(value).map(|mut path| {
                path.push(PathSegment::Index(index));
                path
            })
        }),
        ConfigValue::Table(values) => values.iter().find_map(|(key, value)| {
            invalid_datetime(value).map(|mut path| {
                path.push(PathSegment::Key(key.clone()));
                path
            })
        }),
        _ => None,
    }
}

/// Internal serde error; the path is accumulated leaf-first while unwinding.
struct LocatedError {
    detail: DecodeDetail,
    reversed: ValuePath,
}
impl LocatedError {
    fn new(detail: DecodeDetail) -> Self {
        Self {
            detail,
            reversed: Vec::new(),
        }
    }
    fn within(mut self, segment: PathSegment) -> Self {
        self.reversed.push(segment);
        self
    }
}
impl std::fmt::Debug for LocatedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocatedError(<redacted>)")
    }
}
impl std::fmt::Display for LocatedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("configuration decode failure")
    }
}
impl std::error::Error for LocatedError {}
impl de::Error for LocatedError {
    fn custom<T: std::fmt::Display>(_message: T) -> Self {
        Self::new(DecodeDetail::Other)
    }
    fn invalid_type(_unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::new(DecodeDetail::InvalidType {
            expected: expected.to_string(),
        })
    }
    fn invalid_value(_unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::new(DecodeDetail::InvalidValue {
            expected: expected.to_string(),
        })
    }
    fn invalid_length(_len: usize, expected: &dyn Expected) -> Self {
        Self::new(DecodeDetail::InvalidLength {
            expected: expected.to_string(),
        })
    }
    fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
        Self::new(DecodeDetail::UnknownVariant {
            expected: expected.to_vec(),
        })
    }
    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        Self::new(DecodeDetail::UnknownField {
            field: field.to_owned(),
            expected: expected.to_vec(),
        })
    }
    fn missing_field(field: &'static str) -> Self {
        Self::new(DecodeDetail::MissingField {
            field: field.to_owned(),
        })
    }
}
type Out<T> = std::result::Result<T, LocatedError>;

/// Struct name `toml::value::Datetime` requests, discovered once by probing its
/// `Deserialize` impl so the private marker is not hard-coded here.
fn datetime_struct_name() -> Option<&'static str> {
    struct Probe(Cell<Option<&'static str>>);
    impl<'de> de::Deserializer<'de> for &Probe {
        type Error = LocatedError;
        fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Out<V::Value> {
            Err(LocatedError::new(DecodeDetail::Other))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            name: &'static str,
            _fields: &'static [&'static str],
            _visitor: V,
        ) -> Out<V::Value> {
            self.0.set(Some(name));
            Err(LocatedError::new(DecodeDetail::Other))
        }
        serde::forward_to_deserialize_any! {
            bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string unit seq
            bytes byte_buf map unit_struct tuple_struct newtype_struct option enum
            tuple ignored_any identifier
        }
    }
    static NAME: OnceLock<Option<&'static str>> = OnceLock::new();
    *NAME.get_or_init(|| {
        let probe = Probe(Cell::new(None));
        let _ = <toml::value::Datetime as de::Deserialize>::deserialize(&probe);
        probe.0.get()
    })
}

/// Borrowed node mirroring `toml::Value`'s deserializer so successful results
/// match [`Document::decode`]; keys deserialize like string values.
#[derive(Clone, Copy)]
enum Node<'de> {
    Value(&'de ConfigValue),
    Key(&'de str),
}
impl<'de> de::Deserializer<'de> for Node<'de> {
    type Error = LocatedError;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Out<V::Value> {
        let value = match self {
            Self::Key(key) => return visitor.visit_string(key.to_owned()),
            Self::Value(value) => value,
        };
        match value {
            ConfigValue::Boolean(v) => visitor.visit_bool(*v),
            ConfigValue::Integer(v) => visitor.visit_i64(*v),
            ConfigValue::Float(v) => visitor.visit_f64(*v),
            ConfigValue::String(v) => visitor.visit_string(v.clone()),
            ConfigValue::Datetime(v) => visitor.visit_string(parse_datetime(v)?.to_string()),
            ConfigValue::Array(values) => {
                let mut access = Elements(values.iter().enumerate());
                let out = visitor.visit_seq(&mut access)?;
                if access.0.len() == 0 {
                    Ok(out)
                } else {
                    Err(de::Error::invalid_length(
                        values.len(),
                        &"fewer elements in array",
                    ))
                }
            }
            ConfigValue::Table(values) => {
                let mut access = Entries {
                    iter: values.iter(),
                    pending: None,
                };
                let out = visitor.visit_map(&mut access)?;
                if access.iter.len() == 0 {
                    Ok(out)
                } else {
                    Err(de::Error::invalid_length(
                        values.len(),
                        &"fewer elements in map",
                    ))
                }
            }
        }
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Out<V::Value> {
        match self {
            Self::Key(variant) => visitor.visit_enum(UnitEnum(variant)),
            Self::Value(ConfigValue::String(variant)) => visitor.visit_enum(UnitEnum(variant)),
            Self::Value(ConfigValue::Table(table)) => {
                let mut entries = table.iter();
                match (entries.next(), entries.next()) {
                    (Some((key, value)), None) => visitor.visit_enum(TableEnum { key, value }),
                    _ => Err(LocatedError::new(DecodeDetail::InvalidLength {
                        expected: "a table with exactly 1 entry".into(),
                    })),
                }
            }
            Self::Value(_) => Err(de::Error::invalid_type(
                Unexpected::UnitVariant,
                &"string only",
            )),
        }
    }
    // `None` is represented by an absent field, so present values are `Some`.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Out<V::Value> {
        visitor.visit_some(self)
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Out<V::Value> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Out<V::Value> {
        match self {
            Self::Value(ConfigValue::Datetime(text)) if datetime_struct_name() == Some(name) => {
                de::Deserializer::deserialize_struct(
                    toml::Value::Datetime(parse_datetime(text)?),
                    name,
                    fields,
                    visitor,
                )
                .map_err(|_| LocatedError::new(DecodeDetail::Other))
            }
            node => node.deserialize_any(visitor),
        }
    }
    serde::forward_to_deserialize_any! {
        bool u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 char str string unit seq
        bytes byte_buf map unit_struct tuple_struct
        tuple ignored_any identifier
    }
}
fn parse_datetime(text: &str) -> Out<toml::value::Datetime> {
    text.parse()
        .map_err(|_| LocatedError::new(DecodeDetail::Other))
}

struct Elements<'de>(std::iter::Enumerate<std::slice::Iter<'de, ConfigValue>>);
impl<'de> SeqAccess<'de> for Elements<'de> {
    type Error = LocatedError;
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Out<Option<T::Value>> {
        let Some((index, value)) = self.0.next() else {
            return Ok(None);
        };
        seed.deserialize(Node::Value(value))
            .map(Some)
            .map_err(|error| error.within(PathSegment::Index(index)))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct Entries<'de> {
    iter: std::collections::btree_map::Iter<'de, String, ConfigValue>,
    pending: Option<(&'de String, &'de ConfigValue)>,
}
impl<'de> MapAccess<'de> for Entries<'de> {
    type Error = LocatedError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Out<Option<K::Value>> {
        let Some((key, value)) = self.iter.next() else {
            return Ok(None);
        };
        self.pending = Some((key, value));
        seed.deserialize(Node::Key(key))
            .map(Some)
            .map_err(|error| error.within(PathSegment::Key(key.clone())))
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Out<V::Value> {
        let (key, value) = self
            .pending
            .take()
            .ok_or_else(|| LocatedError::new(DecodeDetail::Other))?;
        seed.deserialize(Node::Value(value))
            .map_err(|error| error.within(PathSegment::Key(key.clone())))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Enum spelled as a bare string: only unit variants are accepted.
struct UnitEnum<'de>(&'de str);
impl<'de> EnumAccess<'de> for UnitEnum<'de> {
    type Error = LocatedError;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Out<(V::Value, Self)> {
        Ok((seed.deserialize(Node::Key(self.0))?, self))
    }
}
impl<'de> VariantAccess<'de> for UnitEnum<'de> {
    type Error = LocatedError;
    fn unit_variant(self) -> Out<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Out<T::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"newtype variant",
        ))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Out<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"tuple variant",
        ))
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Out<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"struct variant",
        ))
    }
}

/// Enum spelled as a single-entry table `{ Variant = content }`; failures inside
/// the content (and an unknown variant key) are located at the variant key.
struct TableEnum<'de> {
    key: &'de String,
    value: &'de ConfigValue,
}
impl TableEnum<'_> {
    fn locate(&self, error: LocatedError) -> LocatedError {
        error.within(PathSegment::Key(self.key.clone()))
    }
}
impl<'de> EnumAccess<'de> for TableEnum<'de> {
    type Error = LocatedError;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Out<(V::Value, Self)> {
        match seed.deserialize(Node::Key(self.key)) {
            Ok(variant) => Ok((variant, self)),
            Err(error) => Err(self.locate(error)),
        }
    }
}
impl<'de> VariantAccess<'de> for TableEnum<'de> {
    type Error = LocatedError;
    fn unit_variant(self) -> Out<()> {
        match self.value {
            ConfigValue::Array(values) if values.is_empty() => Ok(()),
            ConfigValue::Table(values) if values.is_empty() => Ok(()),
            _ => Err(self.locate(de::Error::invalid_type(
                Unexpected::Other("non-empty content"),
                &"an empty table or array",
            ))),
        }
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Out<T::Value> {
        seed.deserialize(Node::Value(self.value))
            .map_err(|error| self.locate(error))
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Out<V::Value> {
        let result = match self.value {
            ConfigValue::Array(values) if values.len() == len => {
                de::Deserializer::deserialize_seq(Node::Value(self.value), visitor)
            }
            // Tables keyed "0", "1", ... in iteration order, matching `toml::Value`.
            ConfigValue::Table(values)
                if values.len() == len
                    && values
                        .keys()
                        .enumerate()
                        .all(|(index, key)| key.parse::<usize>() == Ok(index)) =>
            {
                visitor.visit_seq(&mut TableElements(values.iter()))
            }
            _ => Err(de::Error::invalid_length(len, &"a matching tuple length")),
        };
        result.map_err(|error| self.locate(error))
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Out<V::Value> {
        de::Deserializer::deserialize_struct(Node::Value(self.value), "", fields, visitor)
            .map_err(|error| self.locate(error))
    }
}

/// Table values presented as a tuple sequence, located by their keys.
struct TableElements<'de>(std::collections::btree_map::Iter<'de, String, ConfigValue>);
impl<'de> SeqAccess<'de> for TableElements<'de> {
    type Error = LocatedError;
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Out<Option<T::Value>> {
        let Some((key, value)) = self.0.next() else {
            return Ok(None);
        };
        seed.deserialize(Node::Value(value))
            .map(Some)
            .map_err(|error| error.within(PathSegment::Key(key.clone())))
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}
