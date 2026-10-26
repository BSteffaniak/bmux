use bmux_config_composition::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn value(text: &str) -> ConfigValue {
    TomlCodec::parse(text).unwrap()
}
fn context(root: &Path) -> LoadContext {
    LoadContext::new(root.to_owned(), BTreeMap::new()).unwrap()
}
fn exact(path: &str, requirement: Requirement) -> Layer {
    Layer::File {
        path: path.into(),
        requirement,
    }
}
fn key(key: &str) -> PathSegment {
    PathSegment::Key(key.into())
}
fn origin(doc: &Document, path: ValuePath) -> usize {
    doc.provenance[&path].0
}
fn document(text: &str, source: usize) -> Document {
    Document::new(value(text), SourceId(source))
}

#[test]
fn recursive_tables_replace_arrays_and_keep_unaffected_origins() {
    let mut doc = document("items=[1,2]\n[db]\nhost='old'\nport=1", 1);
    doc.merge(
        document("items=[3]\n[db]\nport=2", 2),
        &MergePolicy::default(),
    )
    .unwrap();
    assert_eq!(doc.value, value("items=[3]\n[db]\nhost='old'\nport=2"));
    assert_eq!(origin(&doc, vec![key("db"), key("host")]), 1);
    assert_eq!(origin(&doc, vec![key("db"), key("port")]), 2);
    assert_eq!(origin(&doc, vec![key("db")]), 2);
    assert_eq!(origin(&doc, vec![key("items"), PathSegment::Index(0)]), 2);
    assert!(
        !doc.provenance
            .contains_key(&vec![key("items"), PathSegment::Index(1)])
    );
}
#[test]
fn ancestor_replacement_erases_stale_provenance_and_new_descendants_restore_it() {
    let mut doc = document("[a.b]\nsecret='old'", 1);
    doc.merge(document("a=7", 2), &MergePolicy::default())
        .unwrap();
    assert_eq!(origin(&doc, vec![key("a")]), 2);
    assert!(!doc.provenance.contains_key(&vec![key("a"), key("b")]));
    doc.merge(document("[a]\nx='new'", 3), &MergePolicy::default())
        .unwrap();
    assert_eq!(origin(&doc, vec![key("a"), key("x")]), 3);
}
#[test]
fn appended_nested_arrays_keep_correct_indices_and_sources() {
    let mut doc = document("a=[{x=1}]", 1);
    let policy = MergePolicy {
        arrays: ArrayMerge::Append,
        ..Default::default()
    };
    doc.merge(document("a=[{x=2}, {x=3}]", 2), &policy).unwrap();
    assert_eq!(doc.value, value("a=[{x=1}, {x=2}, {x=3}]"));
    assert_eq!(
        origin(&doc, vec![key("a"), PathSegment::Index(0), key("x")]),
        1
    );
    assert_eq!(
        origin(&doc, vec![key("a"), PathSegment::Index(2), key("x")]),
        2
    );
}
#[test]
fn exact_path_override_replaces_one_table_without_affecting_another() {
    let mut doc = document("[a]\nx=1\ny=2\n[b]\nx=1\ny=2", 1);
    let mut policy = MergePolicy::default();
    policy.overrides.insert(
        vec![key("a")],
        MergeRule {
            tables: TableMerge::Replace,
            ..Default::default()
        },
    );
    doc.merge(document("[a]\nx=9\n[b]\nx=9", 2), &policy)
        .unwrap();
    assert_eq!(doc.value, value("[a]\nx=9\n[b]\nx=9\ny=2"));
    assert!(!doc.provenance.contains_key(&vec![key("a"), key("y")]));
}
#[test]
fn type_mismatch_rejection_is_transactional() {
    let mut doc = document("a=1\nz=2", 1);
    let before = doc.clone();
    let error = doc
        .merge(
            document("a=9\nz='private'", 2),
            &MergePolicy {
                type_mismatch: TypeMismatch::Reject,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::TypeMismatch);
    assert_eq!(doc.value, before.value);
    assert_eq!(doc.provenance, before.provenance);
}
#[derive(Debug, Deserialize, PartialEq)]
struct Settings {
    answer: i64,
    items: Vec<String>,
}
#[test]
fn ordered_layers_defaults_and_explicit_authority() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("one"), "answer=1").unwrap();
    fs::write(root.path().join("two"), "answer=2").unwrap();
    let make = || {
        Loader::builder()
            .defaults(value("answer=0\nitems=['default']"))
            .layer("first", exact("one", Requirement::Required))
    };
    let additive = make()
        .explicit_file("two".into(), Requirement::Required, ExplicitMode::Additive)
        .build()
        .unwrap();
    let got: Settings = additive
        .capture(&context(root.path()))
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(
        got,
        Settings {
            answer: 2,
            items: vec!["default".into()]
        }
    );
    let exclusive = make()
        .layer("bad", exact("missing", Requirement::Required))
        .explicit_file("two".into(), Requirement::Required, ExplicitMode::Exclusive)
        .build()
        .unwrap();
    let captured = exclusive.capture(&context(root.path())).unwrap();
    assert_eq!(captured.sources().len(), 2);
    assert_eq!(captured.sources()[1].layer, "explicit");
    assert_eq!(
        captured.resolve(&TomlCodec).unwrap().document.value,
        value("answer=2\nitems=['default']")
    );
}
#[test]
fn capture_is_owned_and_resolve_does_not_reread_or_reparse_during_capture() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config");
    fs::write(&path, "answer=1").unwrap();
    let loader = Loader::builder()
        .layer("file", exact("config", Requirement::Required))
        .build()
        .unwrap();
    let captured = loader.capture(&context(root.path())).unwrap();
    fs::write(&path, "broken=[").unwrap();
    assert_eq!(
        captured.resolve(&TomlCodec).unwrap().document.value,
        value("answer=1")
    );
    fs::remove_file(&path).unwrap();
    assert_eq!(
        captured.resolve(&TomlCodec).unwrap().document.value,
        value("answer=1")
    );
    fs::write(&path, "broken=[").unwrap();
    let broken = loader.capture(&context(root.path())).unwrap();
    assert_eq!(
        broken.resolve(&TomlCodec).unwrap_err().kind,
        ErrorKind::Parse
    );
}
#[test]
fn optional_missing_does_not_mask_corruption_or_missing_required() {
    let root = tempfile::tempdir().unwrap();
    let ctx = context(root.path());
    let loader = |requirement| {
        Loader::builder()
            .layer("file", exact("config", requirement))
            .build()
            .unwrap()
    };
    assert!(loader(Requirement::Optional).capture(&ctx).is_ok());
    assert_eq!(
        loader(Requirement::Required)
            .capture(&ctx)
            .unwrap_err()
            .kind,
        ErrorKind::Missing
    );
    fs::write(root.path().join("config"), [0xff]).unwrap();
    assert_eq!(
        loader(Requirement::Optional)
            .capture(&ctx)
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap_err()
            .kind,
        ErrorKind::Parse
    );
}
#[test]
fn xdg_convention_and_home_fallback_use_only_supplied_context() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg/app");
    let home = root.path().join(".config/app");
    fs::create_dir_all(&xdg).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(xdg.join("config.toml"), "answer=1").unwrap();
    fs::write(home.join("config.toml"), "answer=2").unwrap();
    let loader = Loader::builder()
        .layer(
            "user",
            Layer::UserXdg {
                application: "app".into(),
                file_name: "config.toml".into(),
            },
        )
        .build()
        .unwrap();
    let mut env = BTreeMap::from([
        ("HOME".into(), root.path().to_str().unwrap().into()),
        (
            "XDG_CONFIG_HOME".into(),
            root.path().join("xdg").to_str().unwrap().into(),
        ),
    ]);
    let run = |env| {
        loader
            .capture(&LoadContext::new(root.path().to_owned(), env).unwrap())
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value
    };
    assert_eq!(run(env.clone()), value("answer=1"));
    env.insert("XDG_CONFIG_HOME".into(), "relative-is-ignored".into());
    assert_eq!(run(env), value("answer=2"));
    assert_eq!(
        loader.capture(&context(root.path())).unwrap_err().kind,
        ErrorKind::InvalidContext
    );
    assert!(LoadContext::new("relative".into(), BTreeMap::new()).is_err());
}
#[test]
fn ancestors_apply_farthest_first_with_inclusive_boundary_and_depth() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("a/b");
    fs::create_dir_all(&child).unwrap();
    fs::write(root.path().join("cfg"), "root=true\nanswer=0").unwrap();
    fs::write(root.path().join("a/cfg"), "middle=true\nanswer=1").unwrap();
    fs::write(child.join("cfg"), "answer=2").unwrap();
    let run = |max_depth| {
        Loader::builder()
            .layer(
                "ancestor",
                Layer::Ancestors {
                    start: None,
                    file_name: "cfg".into(),
                    boundary: Some(root.path().to_owned()),
                    max_depth,
                },
            )
            .build()
            .unwrap()
            .capture(&context(&child))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
    };
    assert_eq!(
        run(10).document.value,
        value("root=true\nmiddle=true\nanswer=2")
    );
    assert_eq!(run(0).document.value, value("answer=2"));
    assert_eq!(run(1).document.value, value("middle=true\nanswer=2"));
    assert_eq!(run(10).sources.len(), 4);
}
#[test]
fn descendants_and_fragments_have_deterministic_bounded_order() {
    let root = tempfile::tempdir().unwrap();
    for directory in ["b", "a/deep", "fragments"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for (path, text) in [
        ("cfg", "order=['root']"),
        ("a/cfg", "order=['a']"),
        ("a/deep/cfg", "order=['deep']"),
        ("b/cfg", "order=['b']"),
        ("fragments/z.toml", "order=['z']"),
        ("fragments/a.toml", "order=['fragment-a']"),
        ("fragments/no.txt", "not toml"),
    ] {
        fs::write(root.path().join(path), text).unwrap();
    }
    let loader = Loader::builder()
        .merge_policy(MergePolicy {
            arrays: ArrayMerge::Append,
            ..Default::default()
        })
        .layer(
            "tree",
            Layer::Descendants {
                root: ".".into(),
                file_name: "cfg".into(),
                max_depth: 1,
            },
        )
        .layer(
            "fragments",
            Layer::Fragments {
                directory: "fragments".into(),
                extension: "toml".into(),
                requirement: Requirement::Required,
            },
        )
        .build()
        .unwrap();
    let resolved = loader
        .capture(&context(root.path()))
        .unwrap()
        .resolve(&TomlCodec)
        .unwrap();
    assert_eq!(
        resolved.document.value,
        value("order=['root','a','b','fragment-a','z']")
    );
}
#[test]
fn bounds_apply_to_file_count_individual_and_total_bytes_and_entries() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.toml"), "v=1").unwrap();
    fs::write(root.path().join("b.toml"), "v=2").unwrap();
    let base = Limits::default();
    for limits in [
        Limits {
            max_files: 1,
            ..base
        },
        Limits {
            max_file_bytes: 2,
            ..base
        },
        Limits {
            max_total_bytes: 5,
            ..base
        },
        Limits {
            max_entries: 1,
            ..base
        },
    ] {
        let loader = Loader::builder()
            .limits(limits)
            .layer(
                "dir",
                Layer::Fragments {
                    directory: ".".into(),
                    extension: "toml".into(),
                    requirement: Requirement::Required,
                },
            )
            .build()
            .unwrap();
        assert_eq!(
            loader.capture(&context(root.path())).unwrap_err().kind,
            ErrorKind::Limit
        );
    }
}
#[test]
fn resolved_value_limits_apply_to_defaults_inputs_and_composed_arrays() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a"), "v=[1,2]").unwrap();
    fs::write(root.path().join("b"), "v=[3,4]").unwrap();
    let loader = Loader::builder()
        .limits(Limits {
            max_value_nodes: 5,
            ..Default::default()
        })
        .merge_policy(MergePolicy {
            arrays: ArrayMerge::Append,
            ..Default::default()
        })
        .layer("a", exact("a", Requirement::Required))
        .layer("b", exact("b", Requirement::Required))
        .build()
        .unwrap();
    assert_eq!(
        loader
            .capture(&context(root.path()))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    assert!(
        Loader::builder()
            .limits(Limits {
                max_value_depth: 1,
                ..Default::default()
            })
            .defaults(value("[a]\nb=1"))
            .build()
            .is_err()
    );
    let loader = Loader::builder()
        .limits(Limits {
            max_value_depth: 1,
            ..Default::default()
        })
        .layer("a", exact("a", Requirement::Required))
        .build()
        .unwrap();
    assert_eq!(
        loader
            .capture(&context(root.path()))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
}
#[test]
fn duplicate_path_policy_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a"), "items=[1]").unwrap();
    let run = |policy| {
        Loader::builder()
            .duplicates(policy)
            .merge_policy(MergePolicy {
                arrays: ArrayMerge::Append,
                ..Default::default()
            })
            .layer("one", exact("a", Requirement::Required))
            .layer("two", exact("./a", Requirement::Required))
            .build()
            .unwrap()
            .capture(&context(root.path()))
    };
    assert_eq!(
        run(DuplicatePolicy::Ignore)
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value,
        value("items=[1]")
    );
    assert_eq!(
        run(DuplicatePolicy::Allow)
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value,
        value("items=[1,1]")
    );
    assert_eq!(
        run(DuplicatePolicy::Reject).unwrap_err().kind,
        ErrorKind::Duplicate
    );
}
#[test]
fn invalid_policy_cannot_escape_convention_directories() {
    for file_name in ["../escape", "/absolute", "", ".", ".."] {
        assert!(
            Loader::builder()
                .layer(
                    "bad",
                    Layer::UserXdg {
                        application: "app".into(),
                        file_name: file_name.into()
                    }
                )
                .build()
                .is_err()
        );
    }
    assert!(
        Loader::builder()
            .layer("same", exact("a", Requirement::Optional))
            .layer("same", exact("b", Requirement::Optional))
            .build()
            .is_err()
    );
    assert!(
        Loader::builder()
            .layer(
                "deep",
                Layer::Descendants {
                    root: ".".into(),
                    file_name: "a".into(),
                    max_depth: usize::MAX
                }
            )
            .build()
            .is_err()
    );
}
#[cfg(unix)]
#[test]
fn nix_style_symlinks_follow_by_default_reject_when_requested_and_obey_boundary() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(store.path().join("immutable"), "answer=42").unwrap();
    symlink(store.path().join("immutable"), root.path().join("cfg")).unwrap();
    let make = || Loader::builder().layer("user", exact("cfg", Requirement::Required));
    assert_eq!(
        make()
            .build()
            .unwrap()
            .capture(&context(root.path()))
            .unwrap()
            .resolve(&TomlCodec)
            .unwrap()
            .document
            .value,
        value("answer=42")
    );
    assert_eq!(
        make()
            .symlinks(SymlinkPolicy::Reject)
            .build()
            .unwrap()
            .capture(&context(root.path()))
            .unwrap_err()
            .kind,
        ErrorKind::Symlink
    );
    assert_eq!(
        make()
            .boundary(BoundaryPolicy::Within(root.path().to_owned()))
            .build()
            .unwrap()
            .capture(&context(root.path()))
            .unwrap_err()
            .kind,
        ErrorKind::Boundary
    );
    fs::create_dir(root.path().join("real")).unwrap();
    fs::write(root.path().join("real/a"), "x=1").unwrap();
    symlink("real", root.path().join("alias")).unwrap();
    let loader = Loader::builder()
        .symlinks(SymlinkPolicy::Reject)
        .layer("a", exact("alias/a", Requirement::Required))
        .build()
        .unwrap();
    assert_eq!(
        loader.capture(&context(root.path())).unwrap_err().kind,
        ErrorKind::Symlink
    );
}
#[cfg(unix)]
#[test]
fn directory_symlink_cycles_are_not_traversed() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("cfg"), "x=1").unwrap();
    symlink(root.path(), root.path().join("cycle")).unwrap();
    let loader = Loader::builder()
        .layer(
            "tree",
            Layer::Descendants {
                root: ".".into(),
                file_name: "cfg".into(),
                max_depth: 128,
            },
        )
        .build()
        .unwrap();
    assert_eq!(
        loader
            .capture(&context(root.path()))
            .unwrap()
            .sources()
            .len(),
        2
    );
}
#[cfg(unix)]
#[test]
fn fifo_read_is_nonblocking_and_rejected_as_nonregular() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let fifo = root.path().join("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL terminated and mkfifo retains no pointers.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let loader = Loader::builder()
        .layer("fifo", exact("fifo", Requirement::Required))
        .build()
        .unwrap();
    assert_eq!(
        loader.capture(&context(root.path())).unwrap_err().kind,
        ErrorKind::NotRegular
    );
    let directory = Loader::builder()
        .layer("dir", exact(".", Requirement::Required))
        .build()
        .unwrap();
    assert_eq!(
        directory.capture(&context(root.path())).unwrap_err().kind,
        ErrorKind::NotRegular
    );
}
#[test]
fn diagnostics_and_debug_never_include_values_paths_environment_or_keys() {
    let root = tempfile::tempdir().unwrap();
    let secret = "password-SENSITIVE";
    let path = root.path().join(secret);
    fs::write(&path, format!("{secret} = [\"{secret}\", broken")).unwrap();
    let ctx = LoadContext::new(
        root.path().to_owned(),
        BTreeMap::from([("TOKEN".into(), secret.into())]),
    )
    .unwrap();
    let loader = Loader::builder()
        .layer(
            secret,
            Layer::File {
                path,
                requirement: Requirement::Required,
            },
        )
        .build()
        .unwrap();
    let captured = loader.capture(&ctx).unwrap();
    let error = captured.resolve(&TomlCodec).unwrap_err();
    let strings = [
        format!("{captured:?}"),
        format!("{ctx:?}"),
        format!("{:?}", captured.sources()),
        format!("{error:?} {error}"),
        format!("{:?}", document(&format!("'{secret}'='{secret}'"), 1)),
    ];
    for text in strings {
        assert!(!text.contains(secret));
        assert!(!text.contains(root.path().to_str().unwrap()));
    }
    assert_eq!(error.source, Some(SourceId(1)));
    let doc = document(&format!("answer='{secret}'"), 1);
    let error = doc.decode::<Settings>().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    assert!(!format!("{error:?}").contains(secret));
}
#[test]
fn toml_codec_preserves_datetime_and_roundtrips_legacy_documents() {
    let input = value("time=1979-05-27T07:32:00Z\na=[1,2]\n[table]\nx=true");
    assert_eq!(
        TomlCodec::parse(&TomlCodec::encode(&input).unwrap()).unwrap(),
        input
    );
    assert!(matches!(input, ConfigValue::Table(_)));
    assert_eq!(
        Document::new(
            ConfigValue::Datetime("private-invalid-date".into()),
            SourceId(0)
        )
        .decode::<String>()
        .unwrap_err()
        .kind,
        ErrorKind::Decode
    );
}
fn profiles() -> BTreeMap<String, Profile<Vec<&'static str>>> {
    BTreeMap::from([
        (
            "base".into(),
            Profile {
                parents: vec![],
                value: vec!["base"],
            },
        ),
        (
            "left".into(),
            Profile {
                parents: vec!["base".into()],
                value: vec!["left"],
            },
        ),
        (
            "right".into(),
            Profile {
                parents: vec!["base".into()],
                value: vec!["right"],
            },
        ),
        (
            "child".into(),
            Profile {
                parents: vec!["left".into(), "right".into()],
                value: vec!["child"],
            },
        ),
    ])
}
#[test]
fn typed_profiles_resolve_ordered_diamond_branches_without_guessing_syntax() {
    let merged = resolve_profile(
        "child",
        &profiles(),
        ProfileLimits::default(),
        |old, new| {
            old.extend(new);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(merged, ["base", "left", "base", "right", "child"]);
}
#[test]
fn profiles_reject_unknown_cycles_and_bounded_expansion() {
    let mut profiles = profiles();
    let merge = |_: &mut Vec<&str>, _: Vec<&str>| Ok(());
    assert_eq!(
        resolve_profile("missing", &profiles, ProfileLimits::default(), merge)
            .unwrap_err()
            .kind,
        ErrorKind::UnknownProfile
    );
    for limits in [
        ProfileLimits {
            max_depth: 2,
            max_expansions: 100,
        },
        ProfileLimits {
            max_depth: 10,
            max_expansions: 3,
        },
    ] {
        assert_eq!(
            resolve_profile("child", &profiles, limits, merge)
                .unwrap_err()
                .kind,
            ErrorKind::Limit
        );
    }
    profiles
        .get_mut("base")
        .unwrap()
        .parents
        .push("child".into());
    assert_eq!(
        resolve_profile("child", &profiles, ProfileLimits::default(), merge)
            .unwrap_err()
            .kind,
        ErrorKind::ProfileCycle
    );
}
#[test]
fn profiles_compose_documents_with_original_leaf_provenance() {
    let profiles = BTreeMap::from([
        (
            "base".into(),
            Profile {
                parents: vec![],
                value: document("a=1\nb=2", 1),
            },
        ),
        (
            "child".into(),
            Profile {
                parents: vec!["base".into()],
                value: document("b=3", 2),
            },
        ),
    ]);
    let merged = resolve_profile("child", &profiles, ProfileLimits::default(), |old, new| {
        old.merge(new, &MergePolicy::default())
    })
    .unwrap();
    assert_eq!(merged.value, value("a=1\nb=3"));
    assert_eq!(origin(&merged, vec![key("a")]), 1);
    assert_eq!(origin(&merged, vec![key("b")]), 2);
}
