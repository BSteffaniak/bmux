use bmux_config_composition::*;
fn deep(depth: usize) -> ConfigValue {
    let mut value = ConfigValue::Integer(1);
    for _ in 0..depth {
        value = ConfigValue::Array(vec![value]);
    }
    value
}
#[test]
fn checked_construction_rejects_deep_owned_trees_without_recursive_drop() {
    let error = Document::try_new(deep(10_000), SourceId(7), usize::MAX).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Limit);
    assert_eq!(error.source, Some(SourceId(7)));
    assert!(Document::try_new(deep(128), SourceId(0), 1000).is_ok());
}
#[test]
fn trusted_constructor_records_iteratively_and_explicit_disposal_is_stack_safe() {
    // Provenance paths themselves consume quadratic space for a chain; this
    // trusted constructor intentionally leaves resource bounds to its caller.
    let doc = Document::new(deep(1024), SourceId(1));
    assert_eq!(doc.provenance.len(), 1025);
    discard_value(doc.value);
}
#[test]
fn decode_encode_and_merge_validate_before_recursive_clone_or_conversion() {
    let value = deep(10_000);
    assert_eq!(
        TomlCodec::encode(&value).unwrap_err().kind,
        ErrorKind::Limit
    );
    assert_eq!(
        TomlCodec::to_serde::<Vec<i64>>(&value).unwrap_err().kind,
        ErrorKind::Limit
    );
    let incoming = Document {
        value,
        provenance: Default::default(),
    };
    let mut target = Document::new(ConfigValue::Integer(0), SourceId(0));
    assert_eq!(
        target
            .merge(incoming, &MergePolicy::default())
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    assert_eq!(target.value, ConfigValue::Integer(0));
}
#[test]
fn toml_deep_tables_are_rejected_before_recursive_variant_conversion() {
    let text = format!("[{}]\nx=1", vec!["a"; 512].join("."));
    assert!(matches!(
        TomlCodec::parse(&text).unwrap_err().kind,
        ErrorKind::Limit | ErrorKind::Parse
    ));
}
struct DeepCodec;
impl Codec for DeepCodec {
    fn parse(&self, _: &str) -> Result<ConfigValue> {
        Ok(deep(10_000))
    }
}
#[test]
fn custom_codec_deep_result_is_rejected_and_dismantled_iteratively() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), "ignored").unwrap();
    let context = LoadContext::new(root.path().to_owned(), Default::default()).unwrap();
    let loader = Loader::builder()
        .layer(
            "file",
            Layer::File {
                path: "file".into(),
                requirement: Requirement::Required,
            },
        )
        .build()
        .unwrap();
    assert_eq!(
        loader
            .capture(&context)
            .unwrap()
            .resolve(&DeepCodec)
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
}
