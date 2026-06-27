use crate::mode::EncodingMode;
use crate::tag::TypeTag;
use crate::{error::Error, varint};
use serde::de::{self, Deserialize, DeserializeSeed, IntoDeserializer, Visitor};

/// A binary deserializer for the bmux wire protocol.
///
/// By default, structs and struct variants are decoded as field-name maps and
/// enum variants are decoded by variant name. Use [`from_positional_bytes`] for the
/// positional representation that reads struct fields positionally and enum
/// variants by declaration index.
pub struct Deserializer<'de> {
    input: &'de [u8],
    pos: usize,
    mode: EncodingMode,
    tagged_identifier: bool,
}

impl<'de> Deserializer<'de> {
    fn new(input: &'de [u8], mode: EncodingMode) -> Self {
        Deserializer {
            input,
            pos: 0,
            mode,
            tagged_identifier: false,
        }
    }

    fn remaining(&self) -> &'de [u8] {
        &self.input[self.pos..]
    }

    fn consume(&mut self, n: usize) -> Result<&'de [u8], Error> {
        if self.pos + n > self.input.len() {
            return Err(Error::UnexpectedEof);
        }
        let slice = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, Error> {
        let bytes = self.consume(1)?;
        Ok(bytes[0])
    }

    fn read_tag(&mut self) -> Result<Option<TypeTag>, Error> {
        if self.mode != EncodingMode::TypedStable {
            return Ok(None);
        }
        let tag = match self.read_u8()? {
            0 => TypeTag::Unit,
            1 => TypeTag::Bool,
            2 => TypeTag::I8,
            3 => TypeTag::I16,
            4 => TypeTag::I32,
            5 => TypeTag::I64,
            6 => TypeTag::U8,
            7 => TypeTag::U16,
            8 => TypeTag::U32,
            9 => TypeTag::U64,
            10 => TypeTag::F32,
            11 => TypeTag::F64,
            12 => TypeTag::Char,
            13 => TypeTag::String,
            14 => TypeTag::Bytes,
            15 => TypeTag::None,
            16 => TypeTag::Some,
            17 => TypeTag::Seq,
            18 => TypeTag::Map,
            19 => TypeTag::Struct,
            20 => TypeTag::Enum,
            _ => return Err(Error::Message("invalid type tag".to_string())),
        };
        Ok(Some(tag))
    }

    fn expect_tag(&mut self, expected: TypeTag) -> Result<(), Error> {
        if let Some(actual) = self.read_tag()?
            && actual != expected
        {
            return Err(Error::Message(format!(
                "unexpected type tag: expected {expected:?}, got {actual:?}"
            )));
        }
        Ok(())
    }

    fn read_varint_u64(&mut self) -> Result<u64, Error> {
        let (value, consumed) = varint::decode_u64(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_u32(&mut self) -> Result<u32, Error> {
        let (value, consumed) = varint::decode_u32(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_u16(&mut self) -> Result<u16, Error> {
        let (value, consumed) = varint::decode_u16(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_usize(&mut self) -> Result<usize, Error> {
        let (value, consumed) =
            varint::decode_usize(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_i16(&mut self) -> Result<i16, Error> {
        let (value, consumed) = varint::decode_i16(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_i32(&mut self) -> Result<i32, Error> {
        let (value, consumed) = varint::decode_i32(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_varint_i64(&mut self) -> Result<i64, Error> {
        let (value, consumed) = varint::decode_i64(self.remaining()).ok_or(Error::UnexpectedEof)?;
        self.pos += consumed;
        Ok(value)
    }

    fn read_bytes(&mut self) -> Result<&'de [u8], Error> {
        let len = self.read_varint_usize()?;
        self.consume(len)
    }

    fn read_str(&mut self) -> Result<&'de str, Error> {
        let bytes = self.read_bytes()?;
        std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
    }
}

pub(crate) fn from_bytes_with_mode<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    mode: EncodingMode,
) -> Result<T, Error> {
    let mut deserializer = Deserializer::new(bytes, mode);
    let value = T::deserialize(&mut deserializer)?;
    if deserializer.pos != deserializer.input.len() {
        return Err(Error::TrailingBytes);
    }
    Ok(value)
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let Some(tag) = self.read_tag()? else {
            return Err(Error::UnsupportedType(
                "deserialize_any (non-self-describing format)",
            ));
        };
        match tag {
            TypeTag::Unit => visitor.visit_unit(),
            TypeTag::Bool => match self.read_u8()? {
                0 => visitor.visit_bool(false),
                1 => visitor.visit_bool(true),
                _ => Err(Error::InvalidBool),
            },
            TypeTag::I8 => {
                let v = self.read_varint_i16()?;
                visitor.visit_i8(
                    i8::try_from(v).map_err(|_| Error::Message("i8 overflow".to_string()))?,
                )
            }
            TypeTag::I16 => visitor.visit_i16(self.read_varint_i16()?),
            TypeTag::I32 => visitor.visit_i32(self.read_varint_i32()?),
            TypeTag::I64 => visitor.visit_i64(self.read_varint_i64()?),
            TypeTag::U8 => visitor.visit_u8(self.read_u8()?),
            TypeTag::U16 => visitor.visit_u16(self.read_varint_u16()?),
            TypeTag::U32 => visitor.visit_u32(self.read_varint_u32()?),
            TypeTag::U64 => visitor.visit_u64(self.read_varint_u64()?),
            TypeTag::F32 => {
                let bytes = self.consume(4)?;
                visitor.visit_f32(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            TypeTag::F64 => {
                let bytes = self.consume(8)?;
                visitor.visit_f64(f64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]))
            }
            TypeTag::Char => {
                let v = self.read_varint_u32()?;
                let c = char::from_u32(v).ok_or(Error::InvalidChar)?;
                visitor.visit_char(c)
            }
            TypeTag::String => visitor.visit_borrowed_str(self.read_str()?),
            TypeTag::Bytes => visitor.visit_borrowed_bytes(self.read_bytes()?),
            TypeTag::None => visitor.visit_none(),
            TypeTag::Some => visitor.visit_some(self),
            TypeTag::Seq => {
                let len = self.read_varint_usize()?;
                visitor.visit_seq(CountedAccess {
                    de: self,
                    remaining: len,
                    raw_keys: false,
                })
            }
            TypeTag::Map => {
                let len = self.read_varint_usize()?;
                visitor.visit_map(CountedAccess {
                    de: self,
                    remaining: len,
                    raw_keys: false,
                })
            }
            TypeTag::Struct => {
                let len = self.read_varint_usize()?;
                visitor.visit_map(CountedAccess {
                    de: self,
                    remaining: len,
                    raw_keys: true,
                })
            }
            TypeTag::Enum => {
                let variant = self.read_str()?;
                let len = self.read_varint_usize()?;
                visitor.visit_enum(InternallyTaggedEnum {
                    de: self,
                    variant,
                    remaining: len,
                })
            }
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Bool)?;
        match self.read_u8()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            _ => Err(Error::InvalidBool),
        }
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::I8)?;
        let v = self.read_varint_i16()?;
        visitor.visit_i8(i8::try_from(v).map_err(|_| Error::Message("i8 overflow".to_string()))?)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::I16)?;
        visitor.visit_i16(self.read_varint_i16()?)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::I32)?;
        visitor.visit_i32(self.read_varint_i32()?)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::I64)?;
        visitor.visit_i64(self.read_varint_i64()?)
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::U8)?;
        visitor.visit_u8(self.read_u8()?)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::U16)?;
        visitor.visit_u16(self.read_varint_u16()?)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::U32)?;
        visitor.visit_u32(self.read_varint_u32()?)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::U64)?;
        visitor.visit_u64(self.read_varint_u64()?)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::F32)?;
        let bytes = self.consume(4)?;
        visitor.visit_f32(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::F64)?;
        let bytes = self.consume(8)?;
        visitor.visit_f64(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Char)?;
        let v = self.read_varint_u32()?;
        let c = char::from_u32(v).ok_or(Error::InvalidChar)?;
        visitor.visit_char(c)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::String)?;
        visitor.visit_borrowed_str(self.read_str()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::String)?;
        visitor.visit_string(self.read_str()?.to_string())
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Bytes)?;
        visitor.visit_borrowed_bytes(self.read_bytes()?)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Bytes)?;
        visitor.visit_borrowed_bytes(self.read_bytes()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.mode == EncodingMode::TypedStable {
            return match self.read_tag()? {
                Some(TypeTag::None) => visitor.visit_none(),
                Some(TypeTag::Some) => visitor.visit_some(self),
                Some(actual) => Err(Error::Message(format!(
                    "unexpected option type tag: {actual:?}"
                ))),
                None => unreachable!(),
            };
        }
        match self.read_u8()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            _ => Err(Error::InvalidBool),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Unit)?;
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Unit)?;
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Seq)?;
        let len = self.read_varint_usize()?;
        visitor.visit_seq(CountedAccess {
            de: self,
            remaining: len,
            raw_keys: false,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Seq)?;
        let remaining = if self.mode == EncodingMode::TypedStable {
            self.read_varint_usize()?
        } else {
            len
        };
        visitor.visit_seq(CountedAccess {
            de: self,
            remaining,
            raw_keys: false,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Seq)?;
        let remaining = if self.mode == EncodingMode::TypedStable {
            self.read_varint_usize()?
        } else {
            len
        };
        visitor.visit_seq(CountedAccess {
            de: self,
            remaining,
            raw_keys: false,
        })
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Map)?;
        let len = self.read_varint_usize()?;
        visitor.visit_map(CountedAccess {
            de: self,
            remaining: len,
            raw_keys: false,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Struct)?;
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                let len = self.read_varint_usize()?;
                visitor.visit_map(CountedAccess {
                    de: self,
                    remaining: len,
                    raw_keys: true,
                })
            }
            EncodingMode::Positional => visitor.visit_seq(CountedAccess {
                de: self,
                remaining: fields.len(),
                raw_keys: false,
            }),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.expect_tag(TypeTag::Enum)?;
        visitor.visit_enum(self)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.mode {
            EncodingMode::Stable => visitor.visit_borrowed_str(self.read_str()?),
            EncodingMode::TypedStable => {
                if self.tagged_identifier {
                    self.expect_tag(TypeTag::String)?;
                }
                visitor.visit_borrowed_str(self.read_str()?)
            }
            EncodingMode::Positional => visitor.visit_u32(self.read_varint_u32()?),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
}

impl<'de> de::EnumAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Error> {
        let variant = seed.deserialize(&mut *self)?;
        Ok((variant, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        if self.mode == EncodingMode::TypedStable {
            let remaining = self.read_varint_usize()?;
            if remaining != 0 {
                return Err(Error::Message(format!(
                    "unit variant expected 0 fields, found {remaining}"
                )));
            }
        }
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        if self.mode == EncodingMode::TypedStable {
            let remaining = self.read_varint_usize()?;
            if remaining != 1 {
                return Err(Error::Message(format!(
                    "newtype variant expected 1 field, found {remaining}"
                )));
            }
        }
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, Error> {
        let remaining = if self.mode == EncodingMode::TypedStable {
            self.read_varint_usize()?
        } else {
            len
        };
        visitor.visit_seq(CountedAccess {
            de: self,
            remaining,
            raw_keys: false,
        })
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                let len = self.read_varint_usize()?;
                visitor.visit_map(CountedAccess {
                    de: self,
                    remaining: len,
                    raw_keys: true,
                })
            }
            EncodingMode::Positional => visitor.visit_seq(CountedAccess {
                de: self,
                remaining: fields.len(),
                raw_keys: false,
            }),
        }
    }
}

struct InternallyTaggedEnum<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    variant: &'de str,
    remaining: usize,
}

impl<'de, 'a> de::EnumAccess<'de> for InternallyTaggedEnum<'a, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Error> {
        let variant = seed.deserialize(self.variant.into_deserializer())?;
        Ok((variant, self))
    }
}

impl<'de, 'a> de::VariantAccess<'de> for InternallyTaggedEnum<'a, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_seq(CountedAccess {
            de: self.de,
            remaining: self.remaining,
            raw_keys: false,
        })
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_map(CountedAccess {
            de: self.de,
            remaining: self.remaining,
            raw_keys: true,
        })
    }
}

struct CountedAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    remaining: usize,
    raw_keys: bool,
}

impl<'de, 'a> de::SeqAccess<'de> for CountedAccess<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        seed.deserialize(&mut *self.de).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}

impl<'de, 'a> de::MapAccess<'de> for CountedAccess<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        if self.raw_keys {
            let key = self.de.read_str()?;
            return seed.deserialize(key.into_deserializer()).map(Some);
        }
        let previous = self.de.tagged_identifier;
        self.de.tagged_identifier = true;
        let result = seed.deserialize(&mut *self.de);
        self.de.tagged_identifier = previous;
        result.map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let previous = self.de.tagged_identifier;
        if self.raw_keys && self.de.remaining().first().copied() == Some(TypeTag::String as u8) {
            self.de.tagged_identifier = true;
        }
        let result = seed.deserialize(&mut *self.de);
        self.de.tagged_identifier = previous;
        result
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining)
    }
}
