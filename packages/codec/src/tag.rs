/// Type tags used by typed-stable encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TypeTag {
    /// Unit value.
    Unit = 0,
    /// Boolean value.
    Bool = 1,
    /// Signed 8-bit integer.
    I8 = 2,
    /// Signed 16-bit integer.
    I16 = 3,
    /// Signed 32-bit integer.
    I32 = 4,
    /// Signed 64-bit integer.
    I64 = 5,
    /// Unsigned 8-bit integer.
    U8 = 6,
    /// Unsigned 16-bit integer.
    U16 = 7,
    /// Unsigned 32-bit integer.
    U32 = 8,
    /// Unsigned 64-bit integer.
    U64 = 9,
    /// 32-bit float.
    F32 = 10,
    /// 64-bit float.
    F64 = 11,
    /// Unicode scalar value.
    Char = 12,
    /// UTF-8 string.
    String = 13,
    /// Raw byte buffer.
    Bytes = 14,
    /// `Option::None`.
    None = 15,
    /// `Option::Some`.
    Some = 16,
    /// Sequence-like value.
    Seq = 17,
    /// Map value.
    Map = 18,
    /// Struct value.
    Struct = 19,
    /// Enum value.
    Enum = 20,
}
