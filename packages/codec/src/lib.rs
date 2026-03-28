//! `bmux_codec` — Custom binary serialization codec for the bmux IPC protocol.
//!
//! This crate implements a serde-based binary serializer and deserializer
//! designed for the bmux wire protocol. It uses LEB128 varints for compact
//! integer encoding, length-prefixed containers, and varint enum discriminants.
//!
//! # Wire format
//!
//! | Element          | Encoding                             |
//! |-----------------|--------------------------------------|
//! | `bool`          | 1 byte (0 or 1)                      |
//! | `u8`            | 1 byte raw                           |
//! | `u16`..`u64`    | LEB128 unsigned varint               |
//! | `i8`..`i64`     | ZigZag + LEB128                      |
//! | `f32`           | 4 bytes little-endian IEEE 754       |
//! | `f64`           | 8 bytes little-endian IEEE 754       |
//! | `char`          | u32 varint (Unicode scalar)          |
//! | `String`/`str`  | varint length + UTF-8 bytes          |
//! | `Vec<u8>`/bytes | varint length + raw bytes            |
//! | `Vec<T>`        | varint length + elements             |
//! | `Option<T>`     | 1 byte tag (0=None, 1=Some) + value  |
//! | `Map<K,V>`      | varint length + key-value pairs      |
//! | struct          | fields in declaration order, no names |
//! | enum            | varint variant index + variant data   |
//! | newtype         | transparent (inner value only)        |
//! | `Box<T>`        | transparent (same as T)              |

mod de;
mod error;
mod ser;
pub mod varint;

pub use de::from_bytes;
pub use error::Error;
pub use ser::to_vec;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    // ── Basic scalar types ───────────────────────────────────────────────────

    #[test]
    fn roundtrip_bool() {
        for &v in &[true, false] {
            let bytes = to_vec(&v).unwrap();
            let decoded: bool = from_bytes(&bytes).unwrap();
            assert_eq!(decoded, v);
        }
    }

    #[test]
    fn roundtrip_integers() {
        let u8_val: u8 = 42;
        let bytes = to_vec(&u8_val).unwrap();
        assert_eq!(from_bytes::<u8>(&bytes).unwrap(), 42);

        let u16_val: u16 = 1000;
        let bytes = to_vec(&u16_val).unwrap();
        assert_eq!(from_bytes::<u16>(&bytes).unwrap(), 1000);

        let u32_val: u32 = 100_000;
        let bytes = to_vec(&u32_val).unwrap();
        assert_eq!(from_bytes::<u32>(&bytes).unwrap(), 100_000);

        let u64_val: u64 = 1_000_000_000_000;
        let bytes = to_vec(&u64_val).unwrap();
        assert_eq!(from_bytes::<u64>(&bytes).unwrap(), 1_000_000_000_000);

        let i16_val: i16 = -500;
        let bytes = to_vec(&i16_val).unwrap();
        assert_eq!(from_bytes::<i16>(&bytes).unwrap(), -500);

        let i32_val: i32 = -100_000;
        let bytes = to_vec(&i32_val).unwrap();
        assert_eq!(from_bytes::<i32>(&bytes).unwrap(), -100_000);

        let i64_val: i64 = -1_000_000_000_000;
        let bytes = to_vec(&i64_val).unwrap();
        assert_eq!(from_bytes::<i64>(&bytes).unwrap(), -1_000_000_000_000);
    }

    #[test]
    fn roundtrip_f32() {
        let v: f32 = 3.14;
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<f32>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_f64() {
        let v: f64 = std::f64::consts::PI;
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<f64>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_char() {
        for c in ['a', 'Z', '\n', '\u{1F600}', '\u{0}'] {
            let bytes = to_vec(&c).unwrap();
            assert_eq!(from_bytes::<char>(&bytes).unwrap(), c);
        }
    }

    #[test]
    fn roundtrip_string() {
        let s = "hello, bmux!".to_string();
        let bytes = to_vec(&s).unwrap();
        assert_eq!(from_bytes::<String>(&bytes).unwrap(), s);
    }

    #[test]
    fn roundtrip_empty_string() {
        let s = String::new();
        let bytes = to_vec(&s).unwrap();
        assert_eq!(from_bytes::<String>(&bytes).unwrap(), s);
    }

    // ── Option ───────────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_option_none() {
        let v: Option<u32> = None;
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Option<u32>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_option_some() {
        let v: Option<u32> = Some(42);
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Option<u32>>(&bytes).unwrap(), v);
    }

    // ── Vec ──────────────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_vec_u8() {
        let v: Vec<u8> = vec![1, 2, 3, 4, 5];
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<u8>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_vec_string() {
        let v: Vec<String> = vec!["hello".into(), "world".into()];
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<String>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_empty_vec() {
        let v: Vec<u32> = vec![];
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<u32>>(&bytes).unwrap(), v);
    }

    // ── BTreeMap ─────────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_btreemap() {
        let mut m = BTreeMap::new();
        m.insert("key1".to_string(), "val1".to_string());
        m.insert("key2".to_string(), "val2".to_string());
        let bytes = to_vec(&m).unwrap();
        assert_eq!(from_bytes::<BTreeMap<String, String>>(&bytes).unwrap(), m);
    }

    // ── Struct ───────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct SimpleStruct {
        a: u32,
        b: String,
        c: bool,
    }

    #[test]
    fn roundtrip_struct() {
        let v = SimpleStruct {
            a: 42,
            b: "test".into(),
            c: true,
        };
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<SimpleStruct>(&bytes).unwrap(), v);
    }

    // ── Newtype struct ───────────────────────────────────────────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Wrapper(u16);

    #[test]
    fn roundtrip_newtype() {
        let v = Wrapper(999);
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Wrapper>(&bytes).unwrap(), v);
    }

    // ── Enums ────────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum TestEnum {
        Unit,
        Newtype(u32),
        Tuple(u32, String),
        Struct { x: i32, y: String },
    }

    #[test]
    fn roundtrip_enum_unit() {
        let v = TestEnum::Unit;
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<TestEnum>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_enum_newtype() {
        let v = TestEnum::Newtype(42);
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<TestEnum>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_enum_tuple() {
        let v = TestEnum::Tuple(99, "hello".into());
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<TestEnum>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_enum_struct() {
        let v = TestEnum::Struct {
            x: -7,
            y: "world".into(),
        };
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<TestEnum>(&bytes).unwrap(), v);
    }

    // ── Nested / recursive types ─────────────────────────────────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum TreeNode {
        Leaf {
            value: u32,
        },
        Branch {
            left: Box<TreeNode>,
            right: Box<TreeNode>,
        },
    }

    #[test]
    fn roundtrip_recursive_enum() {
        let tree = TreeNode::Branch {
            left: Box::new(TreeNode::Leaf { value: 1 }),
            right: Box::new(TreeNode::Branch {
                left: Box::new(TreeNode::Leaf { value: 2 }),
                right: Box::new(TreeNode::Leaf { value: 3 }),
            }),
        };
        let bytes = to_vec(&tree).unwrap();
        assert_eq!(from_bytes::<TreeNode>(&bytes).unwrap(), tree);
    }

    // ── Complex struct with all field types ──────────────────────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct ComplexStruct {
        id: u64,
        name: Option<String>,
        tags: Vec<String>,
        metadata: BTreeMap<String, String>,
        active: bool,
        nested: SimpleStruct,
    }

    #[test]
    fn roundtrip_complex_struct() {
        let mut meta = BTreeMap::new();
        meta.insert("env".to_string(), "prod".to_string());
        let v = ComplexStruct {
            id: 42,
            name: Some("test-session".into()),
            tags: vec!["alpha".into(), "beta".into()],
            metadata: meta,
            active: true,
            nested: SimpleStruct {
                a: 7,
                b: "inner".into(),
                c: false,
            },
        };
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<ComplexStruct>(&bytes).unwrap(), v);
    }

    // ── UUID support ─────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_uuid() {
        let id = uuid::Uuid::new_v4();
        let bytes = to_vec(&id).unwrap();
        let decoded: uuid::Uuid = from_bytes(&bytes).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn roundtrip_uuid_nil() {
        let id = uuid::Uuid::nil();
        let bytes = to_vec(&id).unwrap();
        let decoded: uuid::Uuid = from_bytes(&bytes).unwrap();
        assert_eq!(decoded, id);
    }

    // ── Tuple types ──────────────────────────────────────────────────────────

    #[test]
    fn roundtrip_tuple() {
        let v: (u32, String, bool) = (42, "hello".into(), true);
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<(u32, String, bool)>(&bytes).unwrap(), v);
    }

    // ── Large enum with many variants (simulates Request/Response) ───────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum LargeEnum {
        V0,
        V1 { a: u32 },
        V2 { a: String, b: Vec<u8> },
        V3(u64),
        V4,
        V5 { x: Option<u32>, y: Option<String> },
        V6 { data: Vec<u8> },
        V7,
        V8 { id: u64, name: String, flags: bool },
        V9,
        V10 { items: Vec<SimpleStruct> },
    }

    #[test]
    fn roundtrip_large_enum_variants() {
        let cases = vec![
            LargeEnum::V0,
            LargeEnum::V1 { a: 100 },
            LargeEnum::V2 {
                a: "hello".into(),
                b: vec![1, 2, 3],
            },
            LargeEnum::V3(999_999),
            LargeEnum::V4,
            LargeEnum::V5 {
                x: Some(42),
                y: None,
            },
            LargeEnum::V6 {
                data: vec![0; 1024],
            },
            LargeEnum::V7,
            LargeEnum::V8 {
                id: 12345,
                name: "session".into(),
                flags: false,
            },
            LargeEnum::V9,
            LargeEnum::V10 {
                items: vec![
                    SimpleStruct {
                        a: 1,
                        b: "x".into(),
                        c: true,
                    },
                    SimpleStruct {
                        a: 2,
                        b: "y".into(),
                        c: false,
                    },
                ],
            },
        ];

        for case in cases {
            let bytes = to_vec(&case).unwrap();
            let decoded: LargeEnum = from_bytes(&bytes).unwrap();
            assert_eq!(decoded, case);
        }
    }

    // ── Serde default attribute (deserialization still works) ────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct WithDefaults {
        a: u32,
        #[serde(default)]
        b: Option<String>,
        #[serde(default)]
        c: Vec<u8>,
    }

    #[test]
    fn roundtrip_with_defaults() {
        let v = WithDefaults {
            a: 42,
            b: None,
            c: vec![],
        };
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<WithDefaults>(&bytes).unwrap(), v);
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[test]
    fn trailing_bytes_detected() {
        let bytes = to_vec(&42u32).unwrap();
        let mut extended = bytes.clone();
        extended.push(0xFF);
        assert!(from_bytes::<u32>(&extended).is_err());
    }

    #[test]
    fn empty_input_for_unit() {
        let bytes = to_vec(&()).unwrap();
        assert!(bytes.is_empty());
        from_bytes::<()>(&bytes).unwrap();
    }

    // ── Vec<u8> special behavior ─────────────────────────────────────────────
    // serde serializes Vec<u8> as a sequence of u8, not as bytes.
    // Both paths must work.

    #[test]
    fn roundtrip_vec_u8_large() {
        let v: Vec<u8> = (0..=255).collect();
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<u8>>(&bytes).unwrap(), v);
    }

    // ── Struct with serde_json::Value field (stored as bytes) ────────────────

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct WithJsonPayload {
        name: String,
        /// In practice, callers should serialize this to JSON string first
        /// and store as String, since serde_json::Value calls deserialize_any.
        data: Vec<u8>,
    }

    #[test]
    fn roundtrip_json_as_bytes() {
        let json_val = serde_json::json!({"key": "value", "num": 42});
        let json_bytes = serde_json::to_vec(&json_val).unwrap();
        let v = WithJsonPayload {
            name: "test".into(),
            data: json_bytes,
        };
        let bytes = to_vec(&v).unwrap();
        let decoded: WithJsonPayload = from_bytes(&bytes).unwrap();
        assert_eq!(decoded, v);
        // Verify we can parse the JSON back
        let parsed: serde_json::Value = serde_json::from_slice(&decoded.data).unwrap();
        assert_eq!(parsed, json_val);
    }

    // ── serde_json::Value round-trip won't work (deserialize_any) ────────────
    // This is expected: our format is non-self-describing.
    // serde_json::Value must be pre-serialized to bytes/string before encoding.

    #[test]
    fn serde_json_value_direct_fails() {
        let val = serde_json::json!({"key": "value"});
        // Serialization might work (serde_json::Value implements Serialize)
        // but deserialization will fail because it calls deserialize_any.
        let bytes = to_vec(&val);
        // It's fine if serialization succeeds or fails; the key point is
        // that deserialization of arbitrary serde_json::Value is not supported.
        if let Ok(bytes) = bytes {
            let result = from_bytes::<serde_json::Value>(&bytes);
            assert!(result.is_err());
        }
    }
}
