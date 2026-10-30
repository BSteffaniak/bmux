use bmux_config_composition::{
    ErrorKind, ExplicitMode, Layer, Limits, LoadContext, Loader, PathSegment, Requirement,
    TomlCodec,
};
use std::collections::BTreeMap;

#[test]
fn environment_documents_are_ordered_captured_and_attributed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("overlay"), "answer=2\nkeep=true").unwrap();
    let context = LoadContext::new(
        root.path().to_owned(),
        BTreeMap::from([
            ("APP_FILE".into(), "overlay".into()),
            ("APP_TEXT".into(), "answer=3".into()),
        ]),
    )
    .unwrap();
    let loader = Loader::builder()
        .layer(
            "file",
            Layer::EnvironmentFile {
                variable: "APP_FILE".into(),
                requirement: Requirement::Required,
            },
        )
        .layer(
            "environment",
            Layer::EnvironmentText {
                variable: "APP_TEXT".into(),
            },
        )
        .layer("cli", Layer::Text("answer=4".into()))
        .build()
        .unwrap();
    let captured = loader.capture(&context).unwrap();
    std::fs::remove_file(root.path().join("overlay")).unwrap();
    let resolved = captured.resolve(&TomlCodec).unwrap();
    let output: toml::Value = resolved.decode().unwrap();
    assert_eq!(output["answer"].as_integer(), Some(4));
    assert_eq!(output["keep"].as_bool(), Some(true));
    let origin = resolved.document.provenance[&vec![PathSegment::Key("answer".into())]];
    assert_eq!(resolved.sources[origin.0].layer, "cli");
    assert!(resolved.sources[origin.0].path.is_none());
}

#[test]
fn absent_and_empty_environment_text_are_distinct_sources() {
    let root = tempfile::tempdir().unwrap();
    let loader = Loader::builder()
        .layer(
            "env",
            Layer::EnvironmentText {
                variable: "TEXT".into(),
            },
        )
        .build()
        .unwrap();
    let absent = LoadContext::new(root.path().to_owned(), BTreeMap::new()).unwrap();
    assert_eq!(loader.capture(&absent).unwrap().sources().len(), 1);
    let empty = LoadContext::new(
        root.path().to_owned(),
        BTreeMap::from([("TEXT".into(), String::new())]),
    )
    .unwrap();
    let captured = loader.capture(&empty).unwrap();
    assert_eq!(captured.sources().len(), 2);
    assert!(captured.resolve(&TomlCodec).is_ok());
}

#[test]
fn exclusive_file_excludes_ambient_environment_documents() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("only"), "answer=7").unwrap();
    let context = LoadContext::new(
        root.path().to_owned(),
        BTreeMap::from([
            ("TEXT".into(), "private invalid [".into()),
            ("FILE".into(), "missing".into()),
        ]),
    )
    .unwrap();
    let resolved = Loader::builder()
        .layer(
            "env-text",
            Layer::EnvironmentText {
                variable: "TEXT".into(),
            },
        )
        .layer(
            "env-file",
            Layer::EnvironmentFile {
                variable: "FILE".into(),
                requirement: Requirement::Required,
            },
        )
        .explicit_file(
            "only".into(),
            Requirement::Required,
            ExplicitMode::Exclusive,
        )
        .build()
        .unwrap()
        .capture(&context)
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap();
    let output: toml::Value = resolved.decode().unwrap();
    assert_eq!(output["answer"].as_integer(), Some(7));
    assert_eq!(resolved.sources.len(), 2);
}

#[test]
fn inline_text_errors_are_redacted_and_capture_is_bounded() {
    let root = tempfile::tempdir().unwrap();
    let context = LoadContext::new(root.path().to_owned(), BTreeMap::new()).unwrap();
    let captured = Loader::builder()
        .layer("text", Layer::Text("private = [invalid".into()))
        .build()
        .unwrap()
        .capture(&context)
        .unwrap();
    let error = captured.resolve(&TomlCodec).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Parse);
    assert!(!format!("{error:?} {error} {captured:?}").contains("private"));
    let error = Loader::builder()
        .limits(Limits {
            max_file_bytes: 3,
            ..Limits::default()
        })
        .layer("text", Layer::Text("answer=4".into()))
        .build()
        .unwrap()
        .capture(&context)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Limit);
}
