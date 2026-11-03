use bmux_config_composition::*;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn value(text: &str) -> ConfigValue {
    TomlCodec::parse(text).unwrap()
}
fn key(key: &str) -> PathSegment {
    PathSegment::Key(key.into())
}
fn context(root: &Path) -> LoadContext {
    LoadContext::new(root.to_owned(), BTreeMap::new()).unwrap()
}
fn capture_texts(layers: &[(&str, &str)]) -> Captured {
    let root = tempfile::tempdir().unwrap();
    layers
        .iter()
        .fold(Loader::builder(), |builder, (name, text)| {
            builder.text(*name, Some(text))
        })
        .build()
        .unwrap()
        .capture(&context(root.path()))
        .unwrap()
}
fn source_named(captured: &Captured, layer: &str) -> SourceId {
    captured
        .sources()
        .iter()
        .find(|source| source.layer == layer)
        .unwrap()
        .id
}
fn resolve_failure<T: serde::de::DeserializeOwned + std::fmt::Debug>(
    layers: &[(&str, &str)],
) -> (Captured, DecodeFailure) {
    let captured = capture_texts(layers);
    let failure = captured
        .resolve(&TomlCodec)
        .unwrap()
        .decode_located::<T>()
        .unwrap_err();
    (captured, *failure)
}
/// Every rendering of the failure, including field contents, must omit `secret`.
fn assert_redacted(failure: &DecodeFailure, secret: &str) {
    let rendered = format!(
        "{failure:?} {failure} {:?} {} {:?}",
        failure.error,
        failure.error,
        failure.path.len()
    );
    assert!(!rendered.contains(secret), "{rendered}");
    let fields = match &failure.detail {
        DecodeDetail::UnknownField { field, expected } => format!("{field} {expected:?}"),
        DecodeDetail::MissingField { field } => field.clone(),
        DecodeDetail::InvalidType { expected }
        | DecodeDetail::InvalidValue { expected }
        | DecodeDetail::InvalidLength { expected } => expected.clone(),
        DecodeDetail::UnknownVariant { expected } => format!("{expected:?}"),
        _ => String::new(),
    };
    assert!(!fields.contains(secret), "{fields}");
    for segment in &failure.path {
        if let PathSegment::Key(key) = segment {
            assert!(!key.contains(secret));
        }
    }
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Server {
    port: u16,
    #[serde(default)]
    host: Option<String>,
}
#[derive(Deserialize, Debug, PartialEq)]
struct Root {
    server: Server,
}

#[test]
fn parse_errors_report_line_column_and_capturing_source() {
    let captured = capture_texts(&[
        ("first", "fine = 1\n"),
        ("second", "ok = true\n[table]\npassword = = 'hunter2'\n"),
    ]);
    let error = captured.documents(&TomlCodec).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Parse);
    assert_eq!(error.source, Some(source_named(&captured, "second")));
    let location = error.location.unwrap();
    assert_eq!((location.line, location.column), (3, 12));
    assert_eq!(
        location.byte_offset,
        "ok = true\n[table]\npassword = ".len()
    );
    let rendered = format!("{error} {error:?}");
    assert!(rendered.contains("line 3, column 12"), "{rendered}");
    assert!(!rendered.contains("hunter2") && !rendered.contains("password"));
    assert_eq!(captured.resolve(&TomlCodec).unwrap_err(), error);

    let broken = capture_texts(&[("first", "x = [\n"), ("second", "y = 1")]);
    let error = broken.resolve(&TomlCodec).unwrap_err();
    assert_eq!(error.source, Some(source_named(&broken, "first")));
    assert!(error.location.is_some());
}
#[test]
fn invalid_utf8_reports_first_invalid_byte() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.toml"), b"a = 1\nb = \"\xc3\xa9\xff\"\n").unwrap();
    let captured = Loader::builder()
        .text("inline", Some("ok = 1"))
        .file("file", Some(Path::new("a.toml")), Requirement::Required)
        .build()
        .unwrap()
        .capture(&context(root.path()))
        .unwrap();
    let error = captured.documents(&TomlCodec).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Parse);
    assert_eq!(error.source, Some(source_named(&captured, "file")));
    assert_eq!(
        error.location,
        Some(TextLocation {
            line: 2,
            column: 7,
            byte_offset: 13
        })
    );
}
#[test]
fn text_locations_count_characters_and_clamp_offsets() {
    let text = "a\né = 1\n";
    let location = TextLocation::from_offset(text, 5);
    assert_eq!(
        (location.line, location.column, location.byte_offset),
        (2, 3, 5)
    );
    // Inside the two-byte character moves back to its first byte.
    assert_eq!(TextLocation::from_offset(text, 3).byte_offset, 2);
    let end = TextLocation::from_offset(text, 999);
    assert_eq!((end.line, end.column, end.byte_offset), (3, 1, text.len()));
    assert_eq!(TomlCodec::parse("a = 1\n").unwrap(), value("a = 1"));
}
#[test]
fn unknown_field_reports_nested_path_and_overriding_source() {
    let (captured, failure) = resolve_failure::<Root>(&[
        ("base", "[server]\nport = 1"),
        ("over", "[server]\nbogus = 'topsecret'"),
    ]);
    assert_eq!(failure.error.kind, ErrorKind::Decode);
    assert_eq!(failure.error.source, Some(source_named(&captured, "over")));
    assert_eq!(failure.path, vec![key("server"), key("bogus")]);
    assert_eq!(
        failure.detail,
        DecodeDetail::UnknownField {
            field: "bogus".into(),
            expected: vec!["port", "host"],
        }
    );
    assert_redacted(&failure, "topsecret");
    assert_eq!(format!("{:?}", failure.detail), "UnknownField");
    assert!(!format!("{failure:?} {failure}").contains("bogus"));
}
#[test]
fn missing_field_reports_containing_table() {
    let (captured, failure) =
        resolve_failure::<Root>(&[("base", "top = 1"), ("over", "[server]\nhost = 'h'")]);
    assert_eq!(failure.path, vec![key("server")]);
    assert_eq!(failure.error.source, Some(source_named(&captured, "over")));
    assert_eq!(
        failure.detail,
        DecodeDetail::MissingField {
            field: "port".into()
        }
    );
    let (captured, failure) = resolve_failure::<Root>(&[("base", "top = 1")]);
    assert!(failure.path.is_empty());
    // The root table's origin is the last document that merged into it.
    assert_eq!(failure.error.source, Some(source_named(&captured, "base")));
    assert_eq!(
        failure.detail,
        DecodeDetail::MissingField {
            field: "server".into()
        }
    );
}
#[test]
fn invalid_type_and_value_never_retain_the_value() {
    let (captured, failure) = resolve_failure::<Root>(&[
        ("base", "[server]\nport = 1"),
        ("over", "[server]\nport = 'hunter2-secret'"),
    ]);
    assert_eq!(failure.path, vec![key("server"), key("port")]);
    assert_eq!(failure.error.source, Some(source_named(&captured, "over")));
    assert_eq!(
        failure.detail,
        DecodeDetail::InvalidType {
            expected: "u16".into()
        }
    );
    assert_redacted(&failure, "hunter2");

    let (_, failure) = resolve_failure::<Root>(&[("base", "[server]\nport = 987654")]);
    assert!(matches!(failure.detail, DecodeDetail::InvalidValue { .. }));
    assert_redacted(&failure, "987654");

    #[derive(Deserialize, Debug)]
    struct Custom {
        #[serde(deserialize_with = "reject")]
        #[allow(dead_code)]
        token: String,
    }
    fn reject<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<String, D::Error> {
        let text = String::deserialize(deserializer)?;
        Err(serde::de::Error::custom(format!("bad token {text}")))
    }
    let (_, failure) = resolve_failure::<Custom>(&[("base", "token = 'classified'")]);
    assert_eq!(failure.detail, DecodeDetail::Other);
    assert_eq!(failure.path, vec![key("token")]);
    assert_redacted(&failure, "classified");
}
#[derive(Deserialize, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Fast,
    Slow,
}
#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Shape {
    Circle { radius: f64 },
    Square(f64),
}
#[derive(Deserialize, Debug, PartialEq)]
struct Item {
    mode: Mode,
}
#[derive(Deserialize, Debug, PartialEq)]
struct Modes {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    shape: Option<Shape>,
}
#[test]
fn unknown_variants_and_enum_content_are_located_without_names() {
    let (captured, failure) = resolve_failure::<Modes>(&[
        ("base", "items = [{ mode = 'fast' }]"),
        (
            "over",
            "items = [{ mode = 'slow' }, { mode = 'turbo-secret' }]",
        ),
    ]);
    assert_eq!(
        failure.path,
        vec![key("items"), PathSegment::Index(1), key("mode")]
    );
    assert_eq!(failure.error.source, Some(source_named(&captured, "over")));
    assert_eq!(
        failure.detail,
        DecodeDetail::UnknownVariant {
            expected: vec!["fast", "slow"]
        }
    );
    assert_redacted(&failure, "turbo");

    let (_, failure) = resolve_failure::<Modes>(&[("base", "shape.hexagon-secret = 1")]);
    assert_eq!(failure.path, vec![key("shape"), key("hexagon-secret")]);
    assert!(matches!(
        failure.detail,
        DecodeDetail::UnknownVariant { .. }
    ));

    let (_, failure) = resolve_failure::<Modes>(&[("base", "shape.circle.radius = 'wide-secret'")]);
    assert_eq!(
        failure.path,
        vec![key("shape"), key("circle"), key("radius")]
    );
    assert_eq!(
        failure.detail,
        DecodeDetail::InvalidType {
            expected: "f64".into()
        }
    );
    assert_redacted(&failure, "wide-secret");
}
#[test]
fn origin_returns_nearest_recorded_ancestor() {
    let mut document = Document::new(value("[a]\nb = 1\nitems = [{ x = 1 }]"), SourceId(1));
    document
        .merge(
            Document::new(value("[a]\nitems = [{ x = 2 }]"), SourceId(2)),
            &MergePolicy::default(),
        )
        .unwrap();
    // Recursive table merges attribute containers to the latest contributor.
    assert_eq!(document.origin(&[]), Some(SourceId(2)));
    assert_eq!(document.origin(&[key("a"), key("b")]), Some(SourceId(1)));
    let x = [key("a"), key("items"), PathSegment::Index(0), key("x")];
    assert_eq!(document.origin(&x), Some(SourceId(2)));
    let missing = [key("a"), key("items"), PathSegment::Index(7), key("y")];
    assert_eq!(document.origin(&missing), Some(SourceId(2)));
    assert_eq!(document.origin(&[key("nope")]), Some(SourceId(2)));
    document.provenance.remove(x.as_slice());
    assert_eq!(document.origin(&x), Some(SourceId(2)));
    document.provenance.retain(|path, _| path.len() < 2);
    assert_eq!(document.origin(&x), Some(SourceId(2)));
    document
        .provenance
        .insert(vec![key("a"), key("items")], SourceId(9));
    assert_eq!(document.origin(&x), Some(SourceId(9)));
    document.provenance.clear();
    assert_eq!(document.origin(&x), None);
}
#[derive(Deserialize, Debug, PartialEq)]
struct Everything {
    name: String,
    count: u8,
    ratio: f32,
    enabled: bool,
    optional: Option<i64>,
    absent: Option<i64>,
    list: Vec<u32>,
    tuple: (i64, String),
    map: HashMap<String, Mode>,
    keyed: BTreeMap<Mode, bool>,
    when: toml::value::Datetime,
    when_text: String,
    shapes: Vec<Shape>,
    unit: Mode,
    nested: Root,
    #[serde(flatten)]
    rest: BTreeMap<String, ConfigValueShadow>,
}
#[derive(Deserialize, Debug, PartialEq)]
#[serde(untagged)]
enum ConfigValueShadow {
    Int(i64),
    Text(String),
    List(Vec<i64>),
}
#[test]
fn decode_located_matches_decode_on_success_and_failure_kinds() {
    let document = Document::new(
        value(
            "name = 'n'\ncount = 7\nratio = 0.5\nenabled = true\noptional = 3\n\
             list = [1, 2]\ntuple = [1, 'two']\nmap = { a = 'fast' }\n\
             keyed = { slow = true }\nwhen = 1979-05-27T07:32:00Z\n\
             when_text = 1979-05-27\nshapes = [{ circle = { radius = 1.5 } }, { square = 2.0 }]\n\
             unit = 'slow'\nextra = 5\nmore = [1]\n[nested.server]\nport = 80\n",
        ),
        SourceId(3),
    );
    let expected = document.decode::<Everything>().unwrap();
    assert_eq!(document.decode_located::<Everything>().unwrap(), expected);
    assert_eq!(expected.when_text, "1979-05-27");
    assert_eq!(expected.rest.len(), 2);
    assert_eq!(
        document.decode_located::<ConfigValueShadowRoot>().unwrap(),
        document.decode::<ConfigValueShadowRoot>().unwrap()
    );

    let mut deep = ConfigValue::Integer(1);
    for _ in 0..200 {
        deep = ConfigValue::Array(vec![deep]);
    }
    let deep = Document::new(deep, SourceId(4));
    assert_eq!(
        deep.decode::<serde::de::IgnoredAny>().unwrap_err().kind,
        ErrorKind::Limit
    );
    let failure = deep.decode_located::<serde::de::IgnoredAny>().unwrap_err();
    assert_eq!(failure.error.kind, ErrorKind::Limit);
    assert_eq!(failure.error.source, Some(SourceId(4)));

    let bad = Document::new(
        ConfigValue::Table(BTreeMap::from([(
            "ignored".into(),
            ConfigValue::Datetime("not-a-date".into()),
        )])),
        SourceId(5),
    );
    assert_eq!(
        bad.decode::<serde::de::IgnoredAny>().unwrap_err().kind,
        ErrorKind::Decode
    );
    let failure = bad.decode_located::<serde::de::IgnoredAny>().unwrap_err();
    assert_eq!(failure.error.kind, ErrorKind::Decode);
    assert_eq!(failure.path, vec![key("ignored")]);
    assert_redacted(&failure, "not-a-date");
}
#[derive(Deserialize, Debug, PartialEq)]
struct ConfigValueShadowRoot {
    #[serde(flatten)]
    all: BTreeMap<String, toml::Value>,
}
