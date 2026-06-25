/// Encoding mode used by serde adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodingMode {
    /// Stable field-name and variant-name based encoding.
    Stable,
    /// Stable encoding with per-value type tags.
    TypedStable,
    /// Positional field and variant-index based encoding.
    Positional,
}
