//! `bmux_codec` — Custom binary serialization codec for the bmux IPC protocol.
//!
//! This crate provides bmux wire-format primitives and optional serde adapters.
//! With default features enabled, stable, positional, and typed-stable serde
//! APIs are exported for compatibility with existing users.

#[cfg(feature = "compression")]
pub mod compression;
mod error;
pub mod mode;
#[cfg(feature = "serde")]
mod serde_adapter;
pub mod tag;
pub mod varint;
pub mod wire;

pub use error::Error;

#[cfg(all(feature = "serde", feature = "stable"))]
pub use serde_adapter::stable::{from_bytes, to_vec};

#[cfg(all(feature = "serde", feature = "positional"))]
pub use serde_adapter::positional::{from_positional_bytes, to_positional_vec};

#[cfg(all(feature = "serde", feature = "typed-stable"))]
pub use serde_adapter::typed_stable::{from_typed_bytes, to_typed_vec};

#[cfg(feature = "serde")]
pub use serde_adapter::serde_bytes_vec;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
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
        let v: f32 = std::f32::consts::PI;
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

    #[test]
    fn stable_struct_decodes_after_field_reorder() {
        #[derive(Debug, PartialEq, Serialize)]
        struct Original {
            a: u32,
            b: String,
        }

        #[derive(Debug, PartialEq, Deserialize)]
        struct Reordered {
            b: String,
            a: u32,
        }

        let bytes = to_vec(&Original {
            a: 7,
            b: "stable".into(),
        })
        .unwrap();
        let decoded = from_bytes::<Reordered>(&bytes).unwrap();
        assert_eq!(decoded.a, 7);
        assert_eq!(decoded.b, "stable");
    }

    #[test]
    fn stable_enum_decodes_after_variant_reorder() {
        #[derive(Debug, PartialEq, Serialize)]
        enum Original {
            First,
            Second { value: u32 },
        }

        #[derive(Debug, PartialEq, Deserialize)]
        enum Reordered {
            Second { value: u32 },
            First,
        }

        let first_bytes = to_vec(&Original::First).unwrap();
        assert_eq!(
            from_bytes::<Reordered>(&first_bytes).unwrap(),
            Reordered::First
        );

        let bytes = to_vec(&Original::Second { value: 42 }).unwrap();
        let decoded = from_bytes::<Reordered>(&bytes).unwrap();
        assert_eq!(decoded, Reordered::Second { value: 42 });
    }

    #[test]
    fn positional_roundtrip_still_available() {
        let v = TestEnum::Struct {
            x: -7,
            y: "world".into(),
        };
        let bytes = to_positional_vec(&v).unwrap();
        assert_eq!(from_positional_bytes::<TestEnum>(&bytes).unwrap(), v);
        assert!(from_bytes::<TestEnum>(&bytes).is_err());
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

    #[test]
    fn typed_stable_roundtrip_complex_struct() {
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
        let bytes = to_typed_vec(&v).unwrap();
        assert_eq!(from_typed_bytes::<ComplexStruct>(&bytes).unwrap(), v);
    }

    #[test]
    fn typed_stable_supports_internally_tagged_enums() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Event {
            Status { message: String },
            Progress { percent: u8 },
        }

        for value in [
            Event::Status {
                message: "running".to_string(),
            },
            Event::Progress { percent: 42 },
        ] {
            let bytes = to_typed_vec(&value).unwrap();
            assert_eq!(from_typed_bytes::<Event>(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn typed_stable_supports_recursive_internally_tagged_enums_in_maps() {
        use std::collections::BTreeMap;

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "operation", rename_all = "snake_case")]
        enum Expression {
            Input {
                source: String,
            },
            Object {
                fields: BTreeMap<String, Self>,
            },
            Merge {
                objects: Vec<Self>,
                conflict: Conflict,
            },
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Conflict {
            KeepLast,
        }

        let value = Expression::Merge {
            objects: vec![Expression::Object {
                fields: BTreeMap::from([(
                    "value".to_string(),
                    Expression::Input {
                        source: "current".to_string(),
                    },
                )]),
            }],
            conflict: Conflict::KeepLast,
        };
        let bytes = to_typed_vec(&value).unwrap();
        assert_eq!(from_typed_bytes::<Expression>(&bytes).unwrap(), value);
    }

    #[test]
    fn typed_stable_supports_externally_tagged_enums() {
        let values = [
            TestEnum::Unit,
            TestEnum::Newtype(42),
            TestEnum::Tuple(99, "hello".into()),
            TestEnum::Struct {
                x: -7,
                y: "world".into(),
            },
        ];

        for value in values {
            let bytes = to_typed_vec(&value).unwrap();
            assert_eq!(from_typed_bytes::<TestEnum>(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn typed_stable_supports_adjacently_tagged_enums() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
        enum Event {
            Status { message: String },
            Progress(u8),
            Done,
        }

        for value in [
            Event::Status {
                message: "running".to_string(),
            },
            Event::Progress(42),
            Event::Done,
        ] {
            let bytes = to_typed_vec(&value).unwrap();
            assert_eq!(from_typed_bytes::<Event>(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn typed_stable_supports_untagged_enums() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(untagged)]
        enum Value {
            Text(String),
            Count(u64),
            Named { name: String },
        }

        for value in [
            Value::Text("hello".to_string()),
            Value::Count(42),
            Value::Named {
                name: "bmux".to_string(),
            },
        ] {
            let bytes = to_typed_vec(&value).unwrap();
            assert_eq!(from_typed_bytes::<Value>(&bytes).unwrap(), value);
        }
    }

    #[test]
    fn typed_stable_supports_nested_newtype_enum_with_struct_payload() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Outer {
            Ok(Inner),
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Inner {
            Struct { value: Nested },
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Nested {
            Unit,
        }

        let value = Outer::Ok(Inner::Struct {
            value: Nested::Unit,
        });
        let bytes = to_typed_vec(&value).unwrap();
        assert_eq!(from_typed_bytes::<Outer>(&bytes).unwrap(), value);
    }

    #[test]
    fn typed_stable_supports_nested_struct_enum_payloads_with_options() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Response {
            Ok(Payload),
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Payload {
            List { models: Vec<Model> },
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Model {
            id: String,
            pricing: Option<Pricing>,
            visibility: Visibility,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Pricing {
            currency: String,
            unit: Unit,
            input: Option<TokenPrice>,
            cached_input: Option<TokenPrice>,
            cache_write_input: Option<TokenPrice>,
            output: Option<TokenPrice>,
            source: Source,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct TokenPrice {
            micros: u64,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Unit {
            PerMillionTokens,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Source {
            PatternMatch,
        }

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Visibility {
            Visible,
        }

        let value = Response::Ok(Payload::List {
            models: vec![Model {
                id: "test".to_string(),
                pricing: Some(Pricing {
                    currency: "USD".to_string(),
                    unit: Unit::PerMillionTokens,
                    input: Some(TokenPrice { micros: 1_250_000 }),
                    cached_input: Some(TokenPrice { micros: 125_000 }),
                    cache_write_input: None,
                    output: Some(TokenPrice { micros: 10_000_000 }),
                    source: Source::PatternMatch,
                }),
                visibility: Visibility::Visible,
            }],
        });
        let bytes = to_typed_vec(&value).unwrap();
        assert_eq!(from_typed_bytes::<Response>(&bytes).unwrap(), value);
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TupleStruct(u32, String, bool);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct NewtypeStruct(String);

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct UnitStruct;

    #[test]
    fn typed_stable_supports_tuple_and_unit_struct_forms() {
        let tuple = (42_u32, "hello".to_string(), true);
        let bytes = to_typed_vec(&tuple).unwrap();
        assert_eq!(
            from_typed_bytes::<(u32, String, bool)>(&bytes).unwrap(),
            tuple
        );

        let tuple_struct = TupleStruct(7, "tuple".to_string(), false);
        let bytes = to_typed_vec(&tuple_struct).unwrap();
        assert_eq!(
            from_typed_bytes::<TupleStruct>(&bytes).unwrap(),
            tuple_struct
        );

        let newtype = NewtypeStruct("newtype".to_string());
        let bytes = to_typed_vec(&newtype).unwrap();
        assert_eq!(from_typed_bytes::<NewtypeStruct>(&bytes).unwrap(), newtype);

        let unit = UnitStruct;
        let bytes = to_typed_vec(&unit).unwrap();
        assert_eq!(from_typed_bytes::<UnitStruct>(&bytes).unwrap(), unit);
    }

    fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|value| serde_json::Value::Number(value.into())),
            any::<u64>().prop_map(|value| serde_json::Value::Number(value.into())),
            ".{0,64}".prop_map(serde_json::Value::String),
        ];

        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(serde_json::Value::Array),
                prop::collection::btree_map(".{0,32}", inner, 0..8)
                    .prop_map(|map| serde_json::Value::Object(map.into_iter().collect())),
            ]
        })
    }

    fn roundtrip_typed_stable<T>(value: &T) -> Result<T, Error>
    where
        T: Serialize,
        for<'de> T: Deserialize<'de>,
    {
        let bytes = to_typed_vec(value)?;
        from_typed_bytes(&bytes)
    }

    proptest! {
        #[test]
        fn typed_stable_roundtrips_scalars(
            boolean in any::<bool>(),
            signed in any::<i64>(),
            unsigned in any::<u64>(),
            text in ".{0,256}",
            bytes in prop::collection::vec(any::<u8>(), 0..256),
        ) {
            prop_assert_eq!(roundtrip_typed_stable(&boolean).unwrap(), boolean);
            prop_assert_eq!(roundtrip_typed_stable(&signed).unwrap(), signed);
            prop_assert_eq!(roundtrip_typed_stable(&unsigned).unwrap(), unsigned);
            prop_assert_eq!(roundtrip_typed_stable(&text).unwrap(), text);
            prop_assert_eq!(roundtrip_typed_stable(&bytes).unwrap(), bytes);
        }

        #[test]
        fn typed_stable_roundtrips_nested_dynamic_json(value in arb_json_value()) {
            let bytes = to_typed_vec(&value).unwrap();
            let decoded: serde_json::Value = from_typed_bytes(&bytes).unwrap();
            prop_assert_eq!(decoded, value);
        }

        #[test]
        fn typed_stable_roundtrips_nested_codec_value(value in arb_codec_value()) {
            let bytes = to_typed_vec(&value).unwrap();
            let decoded: CodecValue = from_typed_bytes(&bytes).unwrap();
            prop_assert_eq!(decoded, value);
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum CodecValue {
        Unit,
        Bool(bool),
        I64(i64),
        U64(u64),
        String(String),
        Bytes(#[serde(with = "crate::serde_bytes_vec")] Vec<u8>),
        List(Vec<CodecValue>),
        Map(BTreeMap<String, CodecValue>),
    }

    fn arb_codec_value() -> impl Strategy<Value = CodecValue> {
        let leaf = prop_oneof![
            Just(CodecValue::Unit),
            any::<bool>().prop_map(CodecValue::Bool),
            any::<i64>().prop_map(CodecValue::I64),
            any::<u64>().prop_map(CodecValue::U64),
            ".{0,64}".prop_map(CodecValue::String),
            prop::collection::vec(any::<u8>(), 0..64).prop_map(CodecValue::Bytes),
        ];

        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(CodecValue::List),
                prop::collection::btree_map(".{0,32}", inner, 0..8).prop_map(CodecValue::Map),
            ]
        })
    }

    #[test]
    fn typed_stable_rejects_invalid_type_tag() {
        let err = from_typed_bytes::<bool>(&[u8::MAX]).unwrap_err();
        assert!(matches!(err, Error::Message(message) if message == "invalid type tag"));
    }

    #[test]
    fn typed_stable_rejects_mismatched_type_tag() {
        let bytes = to_typed_vec(&true).unwrap();
        let err = from_typed_bytes::<String>(&bytes).unwrap_err();
        assert!(matches!(err, Error::Message(message) if message.contains("unexpected type tag")));
    }

    #[test]
    fn typed_stable_rejects_truncated_payload() {
        let mut bytes = to_typed_vec(&"hello".to_string()).unwrap();
        bytes.pop();
        let err = from_typed_bytes::<String>(&bytes).unwrap_err();
        assert!(matches!(err, Error::UnexpectedEof));
    }

    #[test]
    fn typed_stable_rejects_trailing_bytes() {
        let mut bytes = to_typed_vec(&42_u32).unwrap();
        bytes.push(0);
        let err = from_typed_bytes::<u32>(&bytes).unwrap_err();
        assert!(matches!(err, Error::TrailingBytes));
    }

    #[test]
    fn typed_stable_and_stable_wire_formats_are_isolated() {
        let typed = to_typed_vec(&42_u32).unwrap();
        assert!(from_bytes::<u32>(&typed).is_err());

        let stable = to_vec(&42_u32).unwrap();
        assert!(from_typed_bytes::<u32>(&stable).is_err());
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

    #[test]
    fn roundtrip_uuid_typed_stable() {
        let id = uuid::Uuid::new_v4();
        let bytes = to_typed_vec(&id).unwrap();
        let decoded: uuid::Uuid = from_typed_bytes(&bytes).unwrap();
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

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct WithRawBytesAdapter {
        name: String,
        #[serde(with = "crate::serde_bytes_vec")]
        payload: Vec<u8>,
        #[serde(with = "crate::serde_bytes_vec::option")]
        maybe_payload: Option<Vec<u8>>,
    }

    #[test]
    fn raw_bytes_adapter_preserves_wire_format() {
        #[derive(Serialize)]
        struct PlainBytes<'a> {
            name: &'a str,
            payload: Vec<u8>,
            maybe_payload: Option<Vec<u8>>,
        }

        let payload: Vec<u8> = (0..=255).cycle().take(1024).collect();
        let adapted = WithRawBytesAdapter {
            name: "bytes".into(),
            payload: payload.clone(),
            maybe_payload: Some(payload.clone()),
        };
        let plain = PlainBytes {
            name: "bytes",
            payload,
            maybe_payload: adapted.maybe_payload.clone(),
        };

        let adapted_bytes = to_vec(&adapted).unwrap();
        let plain_bytes = to_vec(&plain).unwrap();
        assert_eq!(adapted_bytes, plain_bytes);
        assert_eq!(
            from_bytes::<WithRawBytesAdapter>(&adapted_bytes).unwrap(),
            adapted
        );
    }

    #[test]
    fn raw_bytes_adapter_rejects_truncated_payload() {
        let value = WithRawBytesAdapter {
            name: "bytes".into(),
            payload: vec![1, 2, 3, 4],
            maybe_payload: None,
        };
        let mut bytes = to_vec(&value).unwrap();
        bytes.pop();

        assert!(from_bytes::<WithRawBytesAdapter>(&bytes).is_err());
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

    // ── Level 2A: Error path tests ───────────────────────────────────────────

    #[test]
    fn invalid_bool_byte_returns_error() {
        // A bool should be 0 or 1. Byte value 2 should fail.
        let result = from_bytes::<bool>(&[2]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_bool_byte_returns_error_high_value() {
        let result = from_bytes::<bool>(&[255]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_variant_index_returns_error() {
        #[derive(Debug, PartialEq, Deserialize)]
        enum SmallEnum {
            A,
            B,
        }
        // Variant name is unknown for a 2-variant enum.
        let bytes = to_vec(&"missing").unwrap();
        let result = from_bytes::<SmallEnum>(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_utf8_in_string_returns_error() {
        // Construct a "string" with invalid UTF-8: length=3 then 3 bytes of 0xFF
        let mut bytes = Vec::new();
        varint::encode_usize(&mut bytes, 3);
        bytes.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        let result = from_bytes::<String>(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn truncated_struct_returns_error() {
        let v = SimpleStruct {
            a: 42,
            b: "test".into(),
            c: true,
        };
        let bytes = to_vec(&v).unwrap();
        // Truncate to half the bytes
        let truncated = &bytes[..bytes.len() / 2];
        let result = from_bytes::<SimpleStruct>(truncated);
        assert!(result.is_err());
    }

    #[test]
    fn truncated_varint_returns_error() {
        // 0x80 sets continuation bit but no terminator follows
        let result = from_bytes::<u64>(&[0x80]);
        assert!(result.is_err());
    }

    #[test]
    fn truncated_varint_multi_byte_returns_error() {
        let result = from_bytes::<u64>(&[0x80, 0x80]);
        assert!(result.is_err());
    }

    #[test]
    fn empty_input_returns_error_for_non_unit() {
        let result = from_bytes::<u32>(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn empty_input_returns_error_for_string() {
        let result = from_bytes::<String>(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_char_surrogate_returns_error() {
        // U+D800 is a surrogate codepoint, not a valid char
        let mut bytes = Vec::new();
        varint::encode_u32(&mut bytes, 0xD800);
        let result = from_bytes::<char>(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_char_too_large_returns_error() {
        // 0x110000 is beyond the Unicode range
        let mut bytes = Vec::new();
        varint::encode_u32(&mut bytes, 0x11_0000);
        let result = from_bytes::<char>(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_any_returns_unsupported_error() {
        // serde_json::Value calls deserialize_any
        let bytes = to_vec(&42u32).unwrap();
        let result = from_bytes::<serde_json::Value>(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn option_invalid_tag_returns_error() {
        // Option tag should be 0 (None) or 1 (Some). Value 2 is invalid.
        let result = from_bytes::<Option<u32>>(&[2]);
        assert!(result.is_err());
    }

    // ── Level 2B: Edge case round-trips ──────────────────────────────────────

    #[test]
    fn roundtrip_btreeset() {
        use std::collections::BTreeSet;
        let mut s = BTreeSet::new();
        s.insert("alpha".to_string());
        s.insert("beta".to_string());
        s.insert("gamma".to_string());
        let bytes = to_vec(&s).unwrap();
        assert_eq!(from_bytes::<BTreeSet<String>>(&bytes).unwrap(), s);
    }

    #[test]
    fn roundtrip_btreeset_empty() {
        use std::collections::BTreeSet;
        let s: BTreeSet<u32> = BTreeSet::new();
        let bytes = to_vec(&s).unwrap();
        assert_eq!(from_bytes::<BTreeSet<u32>>(&bytes).unwrap(), s);
    }

    #[test]
    fn roundtrip_pathbuf() {
        use std::path::PathBuf;
        let p = PathBuf::from("/tmp/bmux/server.sock");
        let bytes = to_vec(&p).unwrap();
        assert_eq!(from_bytes::<PathBuf>(&bytes).unwrap(), p);
    }

    #[test]
    fn roundtrip_pathbuf_empty() {
        use std::path::PathBuf;
        let p = PathBuf::from("");
        let bytes = to_vec(&p).unwrap();
        assert_eq!(from_bytes::<PathBuf>(&bytes).unwrap(), p);
    }

    #[test]
    fn roundtrip_i8_edge_values() {
        for v in [0i8, 1, -1, i8::MIN, i8::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(
                from_bytes::<i8>(&bytes).unwrap(),
                v,
                "i8 roundtrip failed for {v}"
            );
        }
    }

    #[test]
    fn roundtrip_usize_edge_values() {
        for v in [0usize, 1, 127, 128, 65535, usize::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(
                from_bytes::<usize>(&bytes).unwrap(),
                v,
                "usize roundtrip failed for {v}"
            );
        }
    }

    #[test]
    fn roundtrip_empty_btreemap() {
        let m: BTreeMap<String, String> = BTreeMap::new();
        let bytes = to_vec(&m).unwrap();
        assert_eq!(from_bytes::<BTreeMap<String, String>>(&bytes).unwrap(), m);
    }

    #[test]
    fn roundtrip_large_vec_u8() {
        let v: Vec<u8> = vec![0xAB; 65536];
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<u8>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_deeply_nested_recursive_type() {
        // Build a tree 15 levels deep
        let mut node = TreeNode::Leaf { value: 42 };
        for _ in 0..15 {
            node = TreeNode::Branch {
                left: Box::new(node),
                right: Box::new(TreeNode::Leaf { value: 0 }),
            };
        }
        let bytes = to_vec(&node).unwrap();
        assert_eq!(from_bytes::<TreeNode>(&bytes).unwrap(), node);
    }

    #[test]
    fn roundtrip_integer_boundary_values() {
        // Test boundary values for all integer types
        for v in [u16::MIN, u16::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<u16>(&bytes).unwrap(), v);
        }
        for v in [u32::MIN, u32::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<u32>(&bytes).unwrap(), v);
        }
        for v in [u64::MIN, u64::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<u64>(&bytes).unwrap(), v);
        }
        for v in [i16::MIN, i16::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<i16>(&bytes).unwrap(), v);
        }
        for v in [i32::MIN, i32::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<i32>(&bytes).unwrap(), v);
        }
        for v in [i64::MIN, i64::MAX] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<i64>(&bytes).unwrap(), v);
        }
    }

    #[test]
    fn roundtrip_f32_special_values() {
        for v in [
            f32::MIN,
            f32::MAX,
            f32::EPSILON,
            0.0f32,
            -0.0f32,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            let bytes = to_vec(&v).unwrap();
            assert_eq!(from_bytes::<f32>(&bytes).unwrap(), v);
        }
        // NaN: can't use == for NaN, check is_nan instead
        let bytes = to_vec(&f32::NAN).unwrap();
        assert!(from_bytes::<f32>(&bytes).unwrap().is_nan());
    }

    #[test]
    fn roundtrip_nested_option() {
        let v: Option<Option<u32>> = Some(Some(42));
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Option<Option<u32>>>(&bytes).unwrap(), v);

        let v: Option<Option<u32>> = Some(None);
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Option<Option<u32>>>(&bytes).unwrap(), v);

        let v: Option<Option<u32>> = None;
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Option<Option<u32>>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_vec_of_enums() {
        let v: Vec<TestEnum> = vec![
            TestEnum::Unit,
            TestEnum::Newtype(1),
            TestEnum::Tuple(2, "x".into()),
            TestEnum::Struct {
                x: -1,
                y: "y".into(),
            },
        ];
        let bytes = to_vec(&v).unwrap();
        assert_eq!(from_bytes::<Vec<TestEnum>>(&bytes).unwrap(), v);
    }

    #[test]
    fn roundtrip_map_with_complex_values() {
        let mut m = BTreeMap::new();
        m.insert(
            "simple".to_string(),
            SimpleStruct {
                a: 1,
                b: "x".into(),
                c: true,
            },
        );
        m.insert(
            "other".to_string(),
            SimpleStruct {
                a: 2,
                b: "y".into(),
                c: false,
            },
        );
        let bytes = to_vec(&m).unwrap();
        assert_eq!(
            from_bytes::<BTreeMap<String, SimpleStruct>>(&bytes).unwrap(),
            m
        );
    }
}
