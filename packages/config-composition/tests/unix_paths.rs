#![cfg(unix)]
use bmux_config_composition::{
    ConfigValue, ErrorKind, Layer, LoadContext, Loader, Requirement, SymlinkPolicy, TomlCodec,
};
use std::{collections::BTreeMap, fs, os::unix::fs::symlink};

#[test]
fn parent_components_are_resolved_after_symlinks_not_lexically_folded() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("target/child")).unwrap();
    fs::write(root.path().join("config"), "answer=1").unwrap();
    fs::write(root.path().join("target/config"), "answer=2").unwrap();
    symlink("target/child", root.path().join("link")).unwrap();
    let context = LoadContext::new(root.path().to_owned(), BTreeMap::new()).unwrap();
    let make = || {
        Loader::builder().layer(
            "exact",
            Layer::File {
                path: "link/../config".into(),
                requirement: Requirement::Required,
            },
        )
    };
    let resolved = make()
        .build()
        .unwrap()
        .capture(&context)
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap();
    assert_eq!(
        resolved.document.value,
        TomlCodec::parse("answer=2").unwrap()
    );
    let error = make()
        .symlinks(SymlinkPolicy::Reject)
        .build()
        .unwrap()
        .capture(&context)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Symlink);
    let regular_parent = Loader::builder()
        .symlinks(SymlinkPolicy::Reject)
        .layer(
            "parent",
            Layer::File {
                path: "target/child/../config".into(),
                requirement: Requirement::Required,
            },
        )
        .build()
        .unwrap()
        .capture(&context)
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap();
    assert_eq!(
        regular_parent.document.value,
        ConfigValue::Table(BTreeMap::from([("answer".into(), ConfigValue::Integer(2))]))
    );
}
