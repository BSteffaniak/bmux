use crate::mode::EncodingMode;
use crate::tag::TypeTag;
use crate::{error::Error, varint};
use serde::ser::{self, Serialize};

/// A binary serializer for the bmux wire protocol.
///
/// By default, structs and struct variants are encoded as field-name maps and
/// enum variants are encoded by variant name. Use [`to_positional_vec`] for the
/// positional representation that writes struct fields positionally and
/// enum variants by declaration index.
pub struct Serializer {
    output: Vec<u8>,
    mode: EncodingMode,
}

impl Serializer {
    fn new(mode: EncodingMode) -> Self {
        Serializer {
            output: Vec::new(),
            mode,
        }
    }

    fn into_vec(self) -> Vec<u8> {
        self.output
    }

    fn write_str(&mut self, value: &str) {
        varint::encode_usize(&mut self.output, value.len());
        self.output.extend_from_slice(value.as_bytes());
    }

    fn is_stable_named(&self) -> bool {
        matches!(self.mode, EncodingMode::Stable | EncodingMode::TypedStable)
    }

    fn write_tag(&mut self, tag: TypeTag) {
        if self.mode == EncodingMode::TypedStable {
            self.output.push(tag as u8);
        }
    }
}

pub(crate) fn to_vec_with_mode<T: Serialize>(
    value: &T,
    mode: EncodingMode,
) -> Result<Vec<u8>, Error> {
    let mut serializer = Serializer::new(mode);
    value.serialize(&mut serializer)?;
    Ok(serializer.into_vec())
}

impl ser::Serializer for &mut Serializer {
    type Ok = ();
    type Error = Error;

    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.write_tag(TypeTag::Bool);
        self.output.push(if v { 1 } else { 0 });
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.write_tag(TypeTag::I8);
        varint::encode_i16(&mut self.output, i16::from(v));
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.write_tag(TypeTag::I16);
        varint::encode_i16(&mut self.output, v);
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.write_tag(TypeTag::I32);
        varint::encode_i32(&mut self.output, v);
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.write_tag(TypeTag::I64);
        varint::encode_i64(&mut self.output, v);
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.write_tag(TypeTag::U8);
        self.output.push(v);
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.write_tag(TypeTag::U16);
        varint::encode_u16(&mut self.output, v);
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.write_tag(TypeTag::U32);
        varint::encode_u32(&mut self.output, v);
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.write_tag(TypeTag::U64);
        varint::encode_u64(&mut self.output, v);
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.write_tag(TypeTag::F32);
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.write_tag(TypeTag::F64);
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        self.write_tag(TypeTag::Char);
        varint::encode_u32(&mut self.output, v as u32);
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.write_tag(TypeTag::String);
        self.write_str(v);
        Ok(())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        self.write_tag(TypeTag::Bytes);
        varint::encode_usize(&mut self.output, v.len());
        self.output.extend_from_slice(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.write_tag(TypeTag::None);
        if self.mode != EncodingMode::TypedStable {
            self.output.push(0);
        }
        Ok(())
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Error> {
        self.write_tag(TypeTag::Some);
        if self.mode != EncodingMode::TypedStable {
            self.output.push(1);
        }
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        self.write_tag(TypeTag::Unit);
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        self.write_tag(TypeTag::Unit);
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        self.write_tag(TypeTag::Enum);
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                self.write_str(variant);
                if self.mode == EncodingMode::TypedStable {
                    varint::encode_usize(&mut self.output, 0);
                }
                Ok(())
            }
            EncodingMode::Positional => {
                varint::encode_u32(&mut self.output, variant_index);
                Ok(())
            }
        }
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.write_tag(TypeTag::Enum);
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                self.write_str(variant);
                if self.mode == EncodingMode::TypedStable {
                    varint::encode_usize(&mut self.output, 1);
                }
            }
            EncodingMode::Positional => varint::encode_u32(&mut self.output, variant_index),
        }
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        let len = len.ok_or(Error::Message(
            "sequence length must be known up front".to_string(),
        ))?;
        self.write_tag(TypeTag::Seq);
        varint::encode_usize(&mut self.output, len);
        Ok(self)
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Error> {
        self.write_tag(TypeTag::Seq);
        if self.mode == EncodingMode::TypedStable {
            varint::encode_usize(&mut self.output, len);
        }
        Ok(self)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        self.write_tag(TypeTag::Seq);
        if self.mode == EncodingMode::TypedStable {
            varint::encode_usize(&mut self.output, len);
        }
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        self.write_tag(TypeTag::Enum);
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                self.write_str(variant);
                if self.mode == EncodingMode::TypedStable {
                    varint::encode_usize(&mut self.output, _len);
                }
            }
            EncodingMode::Positional => varint::encode_u32(&mut self.output, variant_index),
        }
        Ok(self)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        let len = len.ok_or(Error::Message(
            "map length must be known up front".to_string(),
        ))?;
        self.write_tag(TypeTag::Map);
        varint::encode_usize(&mut self.output, len);
        Ok(self)
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        self.write_tag(TypeTag::Struct);
        if self.is_stable_named() {
            varint::encode_usize(&mut self.output, len);
        }
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        self.write_tag(TypeTag::Enum);
        match self.mode {
            EncodingMode::Stable | EncodingMode::TypedStable => {
                self.write_str(variant);
                varint::encode_usize(&mut self.output, len);
            }
            EncodingMode::Positional => varint::encode_u32(&mut self.output, variant_index),
        }
        Ok(self)
    }
}

impl ser::SerializeSeq for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTuple for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeMap for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Error> {
        key.serialize(&mut **self)
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStruct for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        if self.is_stable_named() {
            self.write_str(key);
        }
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut Serializer {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        if self.is_stable_named() {
            self.write_str(key);
        }
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
