use crate::{Error, ErrorKind, Result};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

#[derive(Clone, PartialEq)]
/// Owned format-neutral configuration tree. Debug is redacted. Manually built trees are trusted: bound depth before recursive clone, overlay or ordinary drop; checked construction uses Document::try_new.
pub enum ConfigValue {
    /// Owned UTF-8 string.
    String(String),
    /// Signed 64-bit integer.
    Integer(i64),
    /// 64-bit floating-point number.
    Float(f64),
    /// Boolean flag.
    Boolean(bool),
    /// Codec-specific datetime representation; TOML adapters validate this text.
    Datetime(String),
    /// Ordered sequence; default merge replaces the entire sequence.
    Array(Vec<Self>),
    /// Keyed table; default merge recursively overlays entries.
    Table(BTreeMap<String, Self>),
}
impl ConfigValue {
    /// Infallible recursive-table overlay; arrays, scalars and mismatched types replace.
    /// Empty incoming tables preserve existing table entries.
    pub fn overlay_replace(&mut self, incoming: Self) {
        merge_recursive_replace(self, incoming);
    }
}
impl std::fmt::Debug for ConfigValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigValue(<redacted>)")
    }
}
/// Infallible legacy composition: recursively merge tables; replace all other
/// values, including arrays and mismatched types. Incoming empty tables preserve
/// existing tables. No provenance or application-specific syntax is involved.
pub fn merge_recursive_replace(target: &mut ConfigValue, incoming: ConfigValue) {
    match (target, incoming) {
        (ConfigValue::Table(target), ConfigValue::Table(incoming)) => {
            for (key, value) in incoming {
                if let Some(previous) = target.get_mut(&key) {
                    merge_recursive_replace(previous, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, incoming) => *target = incoming,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// Caller-assignable origin identity; loader defaults always use zero.
pub struct SourceId(pub usize);
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
/// Typed provenance position; Debug redacts key names.
pub enum PathSegment {
    /// Table entry identified by its exact key.
    Key(String),
    /// Array element identified by its zero-based position.
    Index(usize),
}
impl std::fmt::Debug for PathSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<path segment>")
    }
}
/// Root-relative sequence of table keys and array indices; empty means root.
pub type ValuePath = Vec<PathSegment>;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Table overlay policy.
pub enum TableMerge {
    #[default]
    /// Recursively overlay keys while retaining unmentioned entries.
    Recursive,
    /// Replace the entire prior value or collection.
    Replace,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Array overlay policy.
pub enum ArrayMerge {
    #[default]
    /// Replace the entire prior value or collection.
    Replace,
    /// Append incoming elements and shift their provenance indices.
    Append,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Policy for differing configuration variants.
pub enum TypeMismatch {
    #[default]
    /// Replace the entire prior value or collection.
    Replace,
    /// Reject matching objects instead of silently accepting them.
    Reject,
}
#[derive(Clone, Copy, Debug, Default)]
/// Scalar policies selected at one exact path.
pub struct MergeRule {
    /// How table-on-table overlays compose.
    pub tables: TableMerge,
    /// How array-on-array overlays compose.
    pub arrays: ArrayMerge,
    /// How differing value variants compose.
    pub type_mismatch: TypeMismatch,
}
#[derive(Clone, Debug, Default)]
/// Global merge defaults plus exact-path rule overrides.
pub struct MergePolicy {
    /// How table-on-table overlays compose.
    pub tables: TableMerge,
    /// How array-on-array overlays compose.
    pub arrays: ArrayMerge,
    /// How differing value variants compose.
    pub type_mismatch: TypeMismatch,
    /// Exact-path rules; unspecified paths use the global policies.
    pub overrides: BTreeMap<ValuePath, MergeRule>,
}
impl MergePolicy {
    fn rule(&self, path: &ValuePath) -> MergeRule {
        self.overrides.get(path).copied().unwrap_or(MergeRule {
            tables: self.tables,
            arrays: self.arrays,
            type_mismatch: self.type_mismatch,
        })
    }
}
#[derive(Clone)]
/// Value plus origin for each node; Debug redacts keys and values.
pub struct Document {
    /// Explicit trusted-access configuration contents.
    pub value: ConfigValue,
    /// Origin of each container and leaf, including array indices.
    pub provenance: BTreeMap<ValuePath, SourceId>,
}
impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Document(<redacted>)")
    }
}
impl Document {
    /// Construct a trusted in-memory document. Provenance recording is iterative,
    /// but callers must bound depth before recursive Clone/merge/drop operations.
    /// Prefer `try_new` for externally constructed or otherwise untrusted values.
    pub fn new(value: ConfigValue, source: SourceId) -> Self {
        let mut provenance = BTreeMap::new();
        record(&value, &mut Vec::new(), source, &mut provenance);
        Self { value, provenance }
    }
    /// Checked construction with a fixed safe depth ceiling of 128 and caller
    /// node bound. Rejected owned trees are dismantled iteratively.
    pub fn try_new(value: ConfigValue, source: SourceId, max_nodes: usize) -> Result<Self> {
        if let Err(error) = validate_value(&value, 128, max_nodes) {
            discard_value(value);
            return Err(error.at(source));
        }
        Ok(Self::new(value, source))
    }
    /// Transactional, including on nested type mismatch. Caller-constructed provenance
    /// maps are preserved; use `new` to ensure every node has an origin.
    pub fn merge(&mut self, incoming: Self, policy: &MergePolicy) -> Result<()> {
        if let Err(error) = validate_value(&self.value, 128, usize::MAX) {
            discard_value(incoming.value);
            return Err(error);
        }
        if let Err(error) = validate_value(&incoming.value, 128, usize::MAX) {
            discard_value(incoming.value);
            return Err(error);
        }
        let mut next = self.clone();
        merge_value(
            &mut next.value,
            incoming.value,
            &mut Vec::new(),
            &mut next.provenance,
            &incoming.provenance,
            policy,
        )?;
        *self = next;
        Ok(())
    }
    /// Decode without text roundtripping; returns Decode for serde failures or Limit for excessive depth.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        to_toml(&self.value)?
            .try_into()
            .map_err(|_| Error::new(ErrorKind::Decode))
    }
}
fn record(
    value: &ConfigValue,
    path: &mut ValuePath,
    source: SourceId,
    out: &mut BTreeMap<ValuePath, SourceId>,
) {
    let mut stack = vec![(value, path.clone())];
    while let Some((value, current)) = stack.pop() {
        out.insert(current.clone(), source);
        match value {
            ConfigValue::Table(table) => {
                for (key, value) in table {
                    let mut child = current.clone();
                    child.push(PathSegment::Key(key.clone()));
                    stack.push((value, child));
                }
            }
            ConfigValue::Array(array) => {
                for (index, value) in array.iter().enumerate() {
                    let mut child = current.clone();
                    child.push(PathSegment::Index(index));
                    stack.push((value, child));
                }
            }
            _ => {}
        }
    }
}
fn merge_value(
    old: &mut ConfigValue,
    new: ConfigValue,
    path: &mut ValuePath,
    origins: &mut BTreeMap<ValuePath, SourceId>,
    incoming: &BTreeMap<ValuePath, SourceId>,
    policy: &MergePolicy,
) -> Result<()> {
    let rule = policy.rule(path);
    if std::mem::discriminant(old) != std::mem::discriminant(&new)
        && rule.type_mismatch == TypeMismatch::Reject
    {
        return Err(Error::new(ErrorKind::TypeMismatch));
    }
    match (old, new) {
        (ConfigValue::Table(old), ConfigValue::Table(new))
            if rule.tables == TableMerge::Recursive =>
        {
            if let Some(source) = incoming.get(path) {
                origins.insert(path.clone(), *source);
            }
            for (key, value) in new {
                path.push(PathSegment::Key(key.clone()));
                if let Some(previous) = old.get_mut(&key) {
                    merge_value(previous, value, path, origins, incoming, policy)?;
                } else {
                    replace_origins(path, origins, incoming);
                    old.insert(key, value);
                }
                path.pop();
            }
        }
        (ConfigValue::Array(old), ConfigValue::Array(new)) if rule.arrays == ArrayMerge::Append => {
            let offset = old.len();
            if let Some(source) = incoming.get(path) {
                origins.insert(path.clone(), *source);
            }
            for (key, source) in incoming {
                if key.starts_with(path) && key.len() > path.len() {
                    let mut shifted = key.clone();
                    if let PathSegment::Index(index) = &mut shifted[path.len()] {
                        *index += offset;
                    }
                    origins.insert(shifted, *source);
                }
            }
            old.extend(new);
        }
        (old, new) => {
            replace_origins(path, origins, incoming);
            *old = new;
        }
    }
    Ok(())
}
fn replace_origins(
    path: &ValuePath,
    origins: &mut BTreeMap<ValuePath, SourceId>,
    incoming: &BTreeMap<ValuePath, SourceId>,
) {
    origins.retain(|key, _| !key.starts_with(path));
    origins.extend(
        incoming
            .iter()
            .filter(|(key, _)| key.starts_with(path))
            .map(|(key, value)| (key.clone(), *value)),
    );
}
/// Codec implementations must return structured errors, not raw parser diagnostics.
pub trait Codec {
    /// Parse one UTF-8 document; returns Parse or Limit without parser messages.
    fn parse(&self, text: &str) -> Result<ConfigValue>;
}
#[derive(Clone, Copy, Debug)]
/// TOML codec and direct serde adapters with redacted errors.
pub struct TomlCodec;
impl TomlCodec {
    /// Convert serde data without encoding/parsing text. TOML's data model applies:
    /// unsupported values (for example None or out-of-range integers) return Decode.
    /// Raw serializer diagnostics are discarded, including custom error messages.
    pub fn from_serde<T: serde::Serialize + ?Sized>(value: &T) -> Result<ConfigValue> {
        toml::Value::try_from(value)
            .map_err(|_| Error::new(ErrorKind::Decode))
            .and_then(checked_from_toml)
    }
    /// Decode directly through the TOML serde adapter without a text roundtrip.
    /// Generic serde consumers may erase format-specific datetime identity;
    /// use explicit ConfigValue variants when preserving that identity is required.
    pub fn to_serde<T: DeserializeOwned>(value: &ConfigValue) -> Result<T> {
        to_toml(value)?
            .try_into()
            .map_err(|_| Error::new(ErrorKind::Decode))
    }
    /// Parse one UTF-8 document; returns Parse or Limit without parser messages.
    pub fn parse(text: &str) -> Result<ConfigValue> {
        <Self as Codec>::parse(&Self, text)
    }
    /// Encode trusted data; returns Decode for unsupported data and Limit for excessive depth.
    pub fn encode(value: &ConfigValue) -> Result<String> {
        toml::to_string(&to_toml(value)?).map_err(|_| Error::new(ErrorKind::Decode))
    }
}
impl Codec for TomlCodec {
    fn parse(&self, text: &str) -> Result<ConfigValue> {
        toml::from_str::<toml::Value>(text)
            .map_err(|_| Error::new(ErrorKind::Parse))
            .and_then(checked_from_toml)
    }
}
/// Dismantle a potentially deeply nested owned value without recursive drop.
/// Useful when rejecting values constructed by an untrusted custom codec.
pub fn discard_value(value: ConfigValue) {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            ConfigValue::Array(values) => stack.extend(values),
            ConfigValue::Table(values) => stack.extend(values.into_values()),
            _ => {}
        }
    }
}
fn checked_from_toml(value: toml::Value) -> Result<ConfigValue> {
    let mut stack = vec![(&value, 0usize)];
    let mut too_deep = false;
    while let Some((value, depth)) = stack.pop() {
        if depth > 128 {
            too_deep = true;
            break;
        }
        match value {
            toml::Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)))
            }
            toml::Value::Table(values) => {
                stack.extend(values.values().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    if too_deep {
        let mut stack = vec![value];
        while let Some(value) = stack.pop() {
            match value {
                toml::Value::Array(values) => stack.extend(values),
                toml::Value::Table(values) => {
                    stack.extend(values.into_iter().map(|(_, value)| value))
                }
                _ => {}
            }
        }
        Err(Error::new(ErrorKind::Limit))
    } else {
        Ok(from_toml(value))
    }
}
fn from_toml(value: toml::Value) -> ConfigValue {
    match value {
        toml::Value::String(v) => ConfigValue::String(v),
        toml::Value::Integer(v) => ConfigValue::Integer(v),
        toml::Value::Float(v) => ConfigValue::Float(v),
        toml::Value::Boolean(v) => ConfigValue::Boolean(v),
        toml::Value::Datetime(v) => ConfigValue::Datetime(v.to_string()),
        toml::Value::Array(v) => ConfigValue::Array(v.into_iter().map(from_toml).collect()),
        toml::Value::Table(v) => {
            ConfigValue::Table(v.into_iter().map(|(k, v)| (k, from_toml(v))).collect())
        }
    }
}
fn to_toml(value: &ConfigValue) -> Result<toml::Value> {
    validate_value(value, 128, usize::MAX)?;
    to_toml_bounded(value)
}
fn to_toml_bounded(value: &ConfigValue) -> Result<toml::Value> {
    Ok(match value {
        ConfigValue::String(v) => toml::Value::String(v.clone()),
        ConfigValue::Integer(v) => toml::Value::Integer(*v),
        ConfigValue::Float(v) => toml::Value::Float(*v),
        ConfigValue::Boolean(v) => toml::Value::Boolean(*v),
        ConfigValue::Datetime(v) => {
            toml::Value::Datetime(v.parse().map_err(|_| Error::new(ErrorKind::Decode))?)
        }
        ConfigValue::Array(v) => {
            toml::Value::Array(v.iter().map(to_toml_bounded).collect::<Result<_>>()?)
        }
        ConfigValue::Table(v) => toml::Value::Table(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), to_toml_bounded(v)?)))
                .collect::<Result<_>>()?,
        ),
    })
}
/// Iterative validation avoids recursive traversal of an untrusted codec result.
pub(crate) fn validate_value(
    value: &ConfigValue,
    max_depth: usize,
    max_nodes: usize,
) -> Result<()> {
    let mut stack = vec![(value, 0)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.checked_add(1).ok_or(Error::new(ErrorKind::Limit))?;
        if depth > max_depth || nodes > max_nodes {
            return Err(Error::new(ErrorKind::Limit));
        }
        match value {
            ConfigValue::Array(v) => stack.extend(v.iter().map(|v| (v, depth + 1))),
            ConfigValue::Table(v) => stack.extend(v.values().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}
