use bmux_config_composition::*;
use std::{collections::BTreeMap, fs, path::Path};
fn value(text: &str) -> ConfigValue {
    TomlCodec::parse(text).unwrap()
}
fn context(path: &Path) -> LoadContext {
    LoadContext::new(path.to_owned(), BTreeMap::new()).unwrap()
}
fn search(root: &Path, matches: SearchMatches, order: SearchOrder) -> Layer {
    Layer::AncestorSearch {
        start: None,
        file_name: "config.toml".into(),
        max_depth: 16,
        policy: SearchPolicy {
            matches,
            order,
            boundary: SearchBoundary::Directory(root.to_owned()),
        },
    }
}
#[test]
fn all_ancestor_orders_and_nearest_select_different_precedence() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("a/b");
    fs::create_dir_all(&child).unwrap();
    fs::write(
        root.path().join("config.toml"),
        "trace=['outer']\nwinner='outer'",
    )
    .unwrap();
    fs::write(
        root.path().join("a/config.toml"),
        "trace=['inner']\nwinner='inner'",
    )
    .unwrap();
    let run = |matches, order| {
        Loader::builder()
            .merge_policy(MergePolicy {
                arrays: ArrayMerge::Append,
                ..Default::default()
            })
            .layer("search", search(root.path(), matches, order))
            .build()
            .unwrap()
            .capture(&context(&child))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
    };
    assert_eq!(
        run(SearchMatches::All, SearchOrder::OuterFirst)
            .document
            .value,
        value("trace=['outer','inner']\nwinner='inner'")
    );
    assert_eq!(
        run(SearchMatches::All, SearchOrder::InnerFirst)
            .document
            .value,
        value("trace=['inner','outer']\nwinner='outer'")
    );
    for order in [SearchOrder::InnerFirst, SearchOrder::OuterFirst] {
        let resolved = run(SearchMatches::Nearest, order);
        assert_eq!(
            resolved.document.value,
            value("trace=['inner']\nwinner='inner'")
        );
        assert_eq!(resolved.sources.len(), 2);
    }
}
#[test]
fn nearest_corrupt_match_does_not_fall_back_and_capture_is_immutable() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(root.path().join("config.toml"), "good=true").unwrap();
    fs::write(child.join("config.toml"), "broken=[").unwrap();
    let loader = Loader::builder()
        .layer(
            "nearest",
            search(root.path(), SearchMatches::Nearest, SearchOrder::OuterFirst),
        )
        .build()
        .unwrap();
    let captured = loader.capture(&context(&child)).unwrap();
    fs::remove_file(child.join("config.toml")).unwrap();
    assert_eq!(
        captured.resolve(&TomlCodec).unwrap_err().kind,
        ErrorKind::Parse
    );
}
#[test]
fn repository_marker_is_explicit_inclusive_and_accepts_worktree_files() {
    for marker_directory in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let child = repo.join("child");
        fs::create_dir_all(&child).unwrap();
        if marker_directory {
            fs::create_dir(repo.join(".git")).unwrap();
        } else {
            fs::write(repo.join(".git"), "gitdir: not-read").unwrap();
        }
        fs::write(root.path().join("config.toml"), "escaped=true").unwrap();
        fs::write(repo.join("config.toml"), "included=true").unwrap();
        let loader = Loader::builder()
            .layer(
                "repo",
                Layer::AncestorSearch {
                    start: None,
                    file_name: "config.toml".into(),
                    max_depth: 8,
                    policy: SearchPolicy {
                        boundary: SearchBoundary::git_repository(),
                        ..Default::default()
                    },
                },
            )
            .build()
            .unwrap();
        assert_eq!(
            loader
                .capture(&context(&child))
                .unwrap()
                .resolve(&TomlCodec)
                .unwrap()
                .document
                .value,
            value("included=true")
        );
    }
}
#[test]
fn named_defaults_can_be_reordered_relative_to_files_and_sorted_fragments() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("fragments")).unwrap();
    fs::write(root.path().join("file"), "winner='file'").unwrap();
    fs::write(root.path().join("fragments/z.toml"), "winner='z'\nz=true").unwrap();
    fs::write(root.path().join("fragments/a.toml"), "winner='a'\na=true").unwrap();
    let defaults = || Layer::Value(value("winner='named-defaults'\ndefault_only=true"));
    let file = || Layer::File {
        path: "file".into(),
        requirement: Requirement::Required,
    };
    let fragments = || Layer::Fragments {
        directory: "fragments".into(),
        extension: "toml".into(),
        requirement: Requirement::Required,
    };
    let run = |layers: Vec<(&str, Layer)>| {
        let mut builder =
            Loader::builder().defaults(value("winner='convenience-defaults'\nbase=true"));
        for (name, layer) in layers {
            builder = builder.layer(name, layer);
        }
        builder
            .build()
            .unwrap()
            .capture(&context(root.path()))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
    };
    let last = run(vec![
        ("file", file()),
        ("fragments", fragments()),
        ("defaults", defaults()),
    ]);
    assert_eq!(
        last.document.value,
        value("winner='named-defaults'\nbase=true\ndefault_only=true\na=true\nz=true")
    );
    let origin = last.document.provenance[&vec![PathSegment::Key("winner".into())]];
    assert_eq!(last.sources[origin.0].layer, "defaults");
    assert!(last.sources[origin.0].path.is_none());
    let first = run(vec![
        ("defaults", defaults()),
        ("file", file()),
        ("fragments", fragments()),
    ]);
    assert_eq!(
        first.document.value,
        value("winner='z'\nbase=true\ndefault_only=true\na=true\nz=true")
    );
    let middle = run(vec![
        ("fragments", fragments()),
        ("defaults", defaults()),
        ("file", file()),
    ]);
    assert_eq!(
        middle.document.value,
        value("winner='file'\nbase=true\ndefault_only=true\na=true\nz=true")
    );
}
#[test]
fn policy_depth_marker_and_entry_limits_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(root.path().join("config.toml"), "outer=true").unwrap();
    let make = |max_depth, marker: &str| Layer::AncestorSearch {
        start: None,
        file_name: "config.toml".into(),
        max_depth,
        policy: SearchPolicy {
            boundary: SearchBoundary::Marker(marker.into()),
            ..Default::default()
        },
    };
    assert!(
        Loader::builder()
            .layer("bad", make(1, "../escape"))
            .build()
            .is_err()
    );
    assert!(
        Loader::builder()
            .layer("bad", make(129, ".git"))
            .build()
            .is_err()
    );
    let shallow = Loader::builder()
        .layer("zero", make(0, ".git"))
        .build()
        .unwrap()
        .capture(&context(&child))
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap();
    assert_eq!(shallow.document.value, value(""));
    let bounded = Loader::builder()
        .limits(Limits {
            max_entries: 1,
            ..Default::default()
        })
        .layer("bound", make(2, ".git"))
        .build()
        .unwrap();
    assert_eq!(
        bounded.capture(&context(&child)).unwrap_err().kind,
        ErrorKind::Limit
    );
}
