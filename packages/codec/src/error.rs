use std::fmt;

/// Errors that can occur during serialization or deserialization.
#[derive(Debug)]
pub enum Error {
    /// A custom error message from serde.
    Message(String),
    /// Unexpected end of input during deserialization.
    UnexpectedEof,
    /// Trailing bytes after deserialization completed.
    TrailingBytes,
    /// A varint exceeded the maximum representable value.
    VarintOverflow,
    /// A sequence or map length exceeded the allowed maximum.
    LengthOverflow,
    /// Invalid UTF-8 in a string.
    InvalidUtf8,
    /// An invalid enum variant index was encountered.
    InvalidVariant,
    /// Invalid boolean value (not 0 or 1).
    InvalidBool,
    /// Invalid char value.
    InvalidChar,
    /// Type not supported by this format.
    UnsupportedType(&'static str),
    /// Compression failed.
    CompressionFailed(String),
    /// Decompression failed.
    DecompressionFailed(String),
    /// Compression algorithm is not available in this build.
    CompressionAlgorithmUnavailable(&'static str),
    /// Unknown compression algorithm wire id.
    UnknownCompressionAlgorithm(u8),
    /// Decompressed payload length did not match the expected length.
    DecompressedLengthMismatch { expected: usize, actual: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Message(msg) => write!(f, "{msg}"),
            Error::UnexpectedEof => write!(f, "unexpected end of input"),
            Error::TrailingBytes => write!(f, "trailing bytes after deserialization"),
            Error::VarintOverflow => write!(f, "varint overflow"),
            Error::LengthOverflow => write!(f, "length overflow"),
            Error::InvalidUtf8 => write!(f, "invalid UTF-8 in string"),
            Error::InvalidVariant => write!(f, "invalid enum variant index"),
            Error::InvalidBool => write!(f, "invalid boolean value"),
            Error::InvalidChar => write!(f, "invalid char value"),
            Error::UnsupportedType(ty) => write!(f, "unsupported type: {ty}"),
            Error::CompressionFailed(msg) => write!(f, "compression failed: {msg}"),
            Error::DecompressionFailed(msg) => write!(f, "decompression failed: {msg}"),
            Error::CompressionAlgorithmUnavailable(algorithm) => {
                write!(f, "compression algorithm unavailable: {algorithm}")
            }
            Error::UnknownCompressionAlgorithm(id) => {
                write!(f, "unknown compression algorithm wire id: {id}")
            }
            Error::DecompressedLengthMismatch { expected, actual } => write!(
                f,
                "decompressed length mismatch: expected {expected} bytes, got {actual} bytes"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(feature = "serde")]
impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

#[cfg(feature = "serde")]
impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}
