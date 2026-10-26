use bmux_config_composition::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

fn value(text: &str) -> ConfigValue {
    TomlCodec::parse(text).unwrap()
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Settings {
    answer: i64,
    items: Vec<String>,
}
#[test]
fn direct_serde_adapters_and_overlay_do_not_need_text() {
    let settings = Settings {
        answer: 42,
        items: vec!["secret".into()],
    };
    let mut config = TomlCodec::from_serde(&settings).unwrap();
    assert_eq!(TomlCodec::to_serde::<Settings>(&config).unwrap(), settings);
    config.overlay_replace(value("answer=7\nitems=[]"));
    assert_eq!(
        TomlCodec::to_serde::<Settings>(&config).unwrap(),
        Settings {
            answer: 7,
            items: vec![]
        }
    );
    let error = TomlCodec::from_serde(&u64::MAX).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    let error = TomlCodec::to_serde::<Settings>(&value("answer='private'\nitems=[]")).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private"));
}
#[test]
fn standard_preset_reads_user_only_and_ordered_inline_overrides_have_origins() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".config/app")).unwrap();
    std::fs::write(
        root.path().join(".config/app/config.toml"),
        "answer=2\nitems=['user']",
    )
    .unwrap();
    // A tempting project configuration must not be discovered by the preset.
    std::fs::write(root.path().join("config.toml"), "answer=999").unwrap();
    let context = LoadContext::new(
        root.path().to_owned(),
        BTreeMap::from([("HOME".into(), root.path().to_str().unwrap().into())]),
    )
    .unwrap();
    let loader = Loader::builder_for("app")
        .defaults(value("answer=1\nitems=[]"))
        .layer("invocation", Layer::Inline(value("answer=3")))
        .build()
        .unwrap();
    let captured = loader.capture(&context).unwrap();
    std::fs::remove_file(root.path().join(".config/app/config.toml")).unwrap();
    let resolved = captured.resolve(&TomlCodec).unwrap();
    assert_eq!(
        resolved.decode::<Settings>().unwrap(),
        Settings {
            answer: 3,
            items: vec!["user".into()]
        }
    );
    let origin = resolved.document.provenance[&vec![PathSegment::Key("answer".into())]];
    let source = &resolved.sources[origin.0];
    assert_eq!(source.layer, "invocation");
    assert!(source.path.is_none());
    let absent = Loader::builder_for("app")
        .build()
        .unwrap()
        .capture(&context)
        .unwrap();
    assert_eq!(absent.sources().len(), 1);
    assert!(Loader::builder_for("../bad").build().is_err());
}
#[test]
fn inline_layers_preserve_exact_order_and_exclusive_file_authority() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), "answer=2").unwrap();
    let context = LoadContext::new(root.path().to_owned(), BTreeMap::new()).unwrap();
    let loader = Loader::builder()
        .layer("before", Layer::Inline(value("answer=1\nkeep=true")))
        .layer(
            "file",
            Layer::File {
                path: "file".into(),
                requirement: Requirement::Required,
            },
        )
        .layer("after", Layer::Inline(value("answer=3")))
        .build()
        .unwrap();
    assert_eq!(
        loader
            .capture(&context)
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value,
        value("answer=3\nkeep=true")
    );
    let exclusive = Loader::builder_for("unused-home")
        .layer("inline", Layer::Inline(value("answer=1\nkeep=true")))
        .explicit_file(
            "file".into(),
            Requirement::Required,
            ExplicitMode::Exclusive,
        )
        .build()
        .unwrap();
    assert_eq!(
        exclusive
            .capture(&context)
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value,
        value("answer=2\nkeep=true")
    );
}
#[test]
fn inline_limits_apply_before_capture_and_after_composition() {
    let root = tempfile::tempdir().unwrap();
    let context = LoadContext::new(root.path().to_owned(), BTreeMap::new()).unwrap();
    let loader = Loader::builder()
        .limits(Limits {
            max_value_depth: 1,
            ..Default::default()
        })
        .layer("inline", Layer::Inline(value("[a]\nb=1")))
        .build()
        .unwrap();
    assert_eq!(loader.capture(&context).unwrap_err().kind, ErrorKind::Limit);
    let loader = Loader::builder()
        .limits(Limits {
            max_files: 1,
            ..Default::default()
        })
        .layer("one", Layer::Inline(value("a=1")))
        .layer("two", Layer::Inline(value("b=2")))
        .build()
        .unwrap();
    assert_eq!(loader.capture(&context).unwrap_err().kind, ErrorKind::Limit);
}
