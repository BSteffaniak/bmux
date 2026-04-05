use crate::error::Error;
use crate::varint;
use serde::ser::{self, Serialize};

/// A binary serializer for the bmux wire protocol.
///
/// Encodes data using LEB128 varints for integers, length-prefixed containers,
/// and varint enum discriminants. Struct fields are written in declaration order
/// without field names.
pub struct Serializer {
    output: Vec<u8>,
}

impl Serializer {
    fn new() -> Self {
        Serializer { output: Vec::new() }
    }

    fn into_vec(self) -> Vec<u8> {
        self.output
    }
}

/// Serialize a value to a byte vector.
///
/// # Errors
///
/// Returns an error if the value fails to serialize.
pub fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let mut serializer = Serializer::new();
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

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.output.push(if v { 1 } else { 0 });
        Ok(())
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        // ZigZag + LEB128 for consistency (though i8 is rare)
        varint::encode_i16(&mut self.output, i16::from(v));
        Ok(())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        varint::encode_i16(&mut self.output, v);
        Ok(())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        varint::encode_i32(&mut self.output, v);
        Ok(())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        varint::encode_i64(&mut self.output, v);
        Ok(())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.output.push(v);
        Ok(())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        varint::encode_u16(&mut self.output, v);
        Ok(())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        varint::encode_u32(&mut self.output, v);
        Ok(())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        varint::encode_u64(&mut self.output, v);
        Ok(())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.output.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        // Encode as u32 (Unicode scalar value)
        varint::encode_u32(&mut self.output, v as u32);
        Ok(())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        varint::encode_usize(&mut self.output, v.len());
        self.output.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        varint::encode_usize(&mut self.output, v.len());
        self.output.extend_from_slice(v);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.output.push(0);
        Ok(())
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Error> {
        self.output.push(1);
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        varint::encode_u32(&mut self.output, variant_index);
        Ok(())
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
        _variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        varint::encode_u32(&mut self.output, variant_index);
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        let len = len.ok_or(Error::Message(
            "sequence length must be known up front".to_string(),
        ))?;
        varint::encode_usize(&mut self.output, len);
        Ok(self)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        // Tuple fields are serialized in order, no length prefix needed (known at compile time)
        Ok(self)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        varint::encode_u32(&mut self.output, variant_index);
        Ok(self)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        let len = len.ok_or(Error::Message(
            "map length must be known up front".to_string(),
        ))?;
        varint::encode_usize(&mut self.output, len);
        Ok(self)
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        // Struct fields serialized in order, no names on wire
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        varint::encode_u32(&mut self.output, variant_index);
        Ok(self)
    }
}

// ── Compound type serialization ──────────────────────────────────────────────

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
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
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
        _key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(&mut **self)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}
