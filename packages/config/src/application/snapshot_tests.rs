use super::*;

fn snapshot_family() -> ConfigFamily {
    ConfigFamily {
        documents: vec![ConfigDocument {
            source: ConfigSource {
                path: PathBuf::from("/captured/config.toml"),
                required: true,
            },
            value: Some(
                toml::from_str(
                    r#"
                [plugins]
                search_paths = ["~/plugins", "$HOME", "$$HOME", "${MISSING}"]
                [recording]
                dir = "~/recordings"
                [unknown_extension]
                token = "private-value"
            "#,
                )
                .unwrap(),
            ),
            local: false,
            inline_origin: None,
        }],
    }
}

fn snapshot() -> toml::Value {
    let context = ConfigEvaluationContext {
        cwd: Some(PathBuf::from("/captured")),
        environment: [("HOME".into(), "/captured/$TOKEN/~/home".into())].into(),
    };
    resolve_application_configuration_with_context(
        &snapshot_family(),
        &ConfigFamily::default(),
        None,
        None,
        &context,
    )
    .unwrap()
    .startup_snapshot()
    .unwrap()
}

#[test]
fn ordinary_loader_preserves_captured_literal_tokens_in_marked_snapshot() {
    let snapshot = snapshot();
    let root = std::env::temp_dir().join(format!(
        "bmux-marked-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("snapshot.toml");
    std::fs::write(&path, toml::to_string(&snapshot).unwrap()).unwrap();
    let config = BmuxConfig::load_from_path(&path).unwrap();
    assert_eq!(
        config.plugins.search_paths,
        vec![
            PathBuf::from("/captured/$TOKEN/~/home/plugins"),
            PathBuf::from("/captured/$TOKEN/~/home"),
            PathBuf::from("/captured/$HOME"),
            PathBuf::from("${MISSING}"),
        ]
    );
    assert_eq!(
        config.recording.dir,
        Some(PathBuf::from("/captured/$TOKEN/~/home/recordings"))
    );
    let (explained, _) = BmuxConfig::load_from_path_with_explain(&path, None).unwrap();
    assert_eq!(explained.plugins.search_paths, config.plugins.search_paths);
    assert_eq!(
        snapshot["unknown_extension"]["token"].as_str(),
        Some("private-value")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn marker_is_strict_versioned_and_composition_free() {
    let key = super::super::STARTUP_SNAPSHOT_MARKER;
    for marker in [
        toml::Value::String("SECRET-CANARY".to_owned()),
        toml::from_str("version = 2\nrepresentation = 'SECRET-CANARY'").unwrap(),
        toml::from_str("version = 1\nrepresentation = 'resolved-paths'\nextra = 'SECRET-CANARY'")
            .unwrap(),
        toml::from_str("version = 1").unwrap(),
    ] {
        let mut raw = snapshot();
        raw[key] = marker;
        let error = BmuxConfig::load_from_raw_value_with_resolution(Some(raw), None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported private startup snapshot")
        );
        assert!(!format!("{error:?}").contains("SECRET-CANARY"));
    }
    let mut raw = snapshot();
    raw.as_table_mut()
        .unwrap()
        .insert("composition".into(), toml::Value::Table(toml::Table::new()));
    assert!(BmuxConfig::load_from_raw_value_with_resolution(Some(raw), None).is_err());
}

#[test]
fn marked_snapshot_still_repairs_and_validates_without_value_diagnostics() {
    let mut raw = snapshot();
    raw["general"]["scrollback_limit"] = toml::Value::Integer(0);
    let repaired = BmuxConfig::load_from_raw_value_with_resolution(Some(raw.clone()), None)
        .unwrap()
        .0;
    assert_eq!(repaired.general.scrollback_limit, 10_000);
    raw["server"]["gateway"]["enabled"] = toml::Value::Boolean(true);
    raw["server"]["gateway"]["listen"] = toml::Value::String("SECRET-CANARY".to_owned());
    let error = BmuxConfig::load_from_raw_value_with_resolution(Some(raw), None).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("SECRET-CANARY"));
}

#[test]
fn unmarked_loader_keeps_ordinary_interpolation() {
    let raw = toml::from_str("[plugins]\nsearch_paths = ['$$HOME']").unwrap();
    let config = BmuxConfig::load_from_raw_value_with_resolution(Some(raw), None)
        .unwrap()
        .0;
    assert_eq!(config.plugins.search_paths, vec![PathBuf::from("$HOME")]);
}
