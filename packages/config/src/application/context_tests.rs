use super::*;

fn document(value: &str) -> ConfigFamily {
    ConfigFamily {
        documents: vec![ConfigDocument {
            source: ConfigSource {
                path: PathBuf::from("/original/config.toml"),
                required: true,
            },
            value: Some(toml::from_str(value).unwrap()),
            local: false,
        }],
    }
}

fn context() -> ConfigEvaluationContext {
    ConfigEvaluationContext {
        cwd: Some(PathBuf::from("/captured/project")),
        environment: [
            ("HOME", "/captured/home"),
            ("TERM", "captured-terminal"),
            ("HOSTNAME", "captured-host"),
            ("BMUX_RUNTIME", "captured-runtime"),
            ("ASSET", "relative-assets"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.into()))
        .collect(),
    }
}

fn resolve(family: &ConfigFamily, context: &ConfigEvaluationContext) -> ApplicationConfiguration {
    resolve_application_configuration_with_context(
        family,
        &ConfigFamily::default(),
        None,
        None,
        context,
    )
    .unwrap()
}

#[test]
fn captured_profile_context_is_stable_and_no_process_fallback_occurs() {
    let family = document(
        r#"
        [[composition.auto_select]]
        profile = "selected"
        cwd_prefix = "/captured"
        term_prefix = "captured-"
        host = "CAPTURED-HOST"
        runtime = "CAPTURED-RUNTIME"
        [composition.profiles.selected.patch.bterm]
        selected = true
        [composition.profiles.selected.patch.general]
        default_shell = "captured-shell"
    "#,
    );
    let captured = context();
    let first = resolve(&family, &captured);
    assert_eq!(
        first.bmux.general.default_shell.as_deref(),
        Some("captured-shell")
    );
    assert_eq!(first.application["selected"].as_bool(), Some(true));
    let mut later = captured.clone();
    later.cwd = Some(PathBuf::from("/elsewhere"));
    later.environment.clear();
    assert!(
        resolve(&family, &later)
            .application
            .as_table()
            .unwrap()
            .is_empty()
    );
    assert!(
        resolve(&family, &ConfigEvaluationContext::default())
            .application
            .as_table()
            .unwrap()
            .is_empty()
    );
    let repeated = resolve(&family, &captured);
    assert_eq!(first.bmux_raw, repeated.bmux_raw);
    assert_eq!(first.application, repeated.application);
    assert_eq!(
        first.application_family.resolution,
        repeated.application_family.resolution
    );
    for name in ["TERM", "HOSTNAME", "BMUX_RUNTIME"] {
        let mut changed = captured.clone();
        changed
            .environment
            .insert(name.to_owned(), "different".into());
        assert!(
            resolve(&family, &changed)
                .application
                .as_table()
                .unwrap()
                .is_empty(),
            "{name}"
        );
    }
}

#[test]
fn captured_paths_expand_exactly_once_and_keep_origin_and_missing_tokens() {
    let family = document(
        r#"
        [plugins]
        search_paths = ["~/plugins", "${ASSET}", "$$HOME", "$MISSING", "${LITERAL}"]
        [presenter.web]
        assets = "~/browser"
        [recording]
        dir = "${ASSET}"
        [connections.targets.remote]
        identity_file = "~/key"
    "#,
    );
    let mut captured = context();
    captured
        .environment
        .insert("LITERAL".into(), "$HOME".into());
    let first = resolve(&family, &captured);
    assert_eq!(
        first.bmux.plugins.search_paths,
        vec![
            PathBuf::from("/captured/home/plugins"),
            PathBuf::from("/original/relative-assets"),
            PathBuf::from("/original/$HOME"),
            PathBuf::from("$MISSING"),
            PathBuf::from("/original/$HOME"),
        ]
    );
    assert_eq!(
        first.bmux.presenter.web.assets,
        Some(PathBuf::from("/captured/home/browser"))
    );
    assert_eq!(
        first.bmux.recording.dir,
        Some(PathBuf::from("/original/relative-assets"))
    );
    assert_eq!(
        first.bmux.connections.targets["remote"].identity_file,
        Some(PathBuf::from("/captured/home/key"))
    );
    let empty = resolve(&family, &ConfigEvaluationContext::default());
    assert_eq!(
        empty.bmux.plugins.search_paths[0],
        PathBuf::from("~/plugins")
    );
    assert_eq!(
        empty.bmux.plugins.search_paths[1],
        PathBuf::from("${ASSET}")
    );
    let repeated = resolve(&family, &captured);
    assert_eq!(first.bmux_raw, repeated.bmux_raw);
}

#[test]
fn userprofile_is_explicit_home_fallback_and_host_is_explicit_hostname_fallback() {
    let family = document(
        r#"
        [[composition.auto_select]]
        host = "fallback"
        profile = "selected"
        [composition.profiles.selected.patch.plugins]
        search_paths = ["~/plugins"]
    "#,
    );
    let context = ConfigEvaluationContext {
        cwd: None,
        environment: [
            ("USERPROFILE".into(), "/selected/home".into()),
            ("HOST".into(), "fallback".into()),
        ]
        .into(),
    };
    assert_eq!(
        resolve(&family, &context).bmux.plugins.search_paths,
        vec![PathBuf::from("/selected/home/plugins")]
    );
}

#[test]
fn explicit_errors_redact_composition_typed_values_and_validation_values() {
    for input in [
        "[general]\nscrollback_limit = 'SECRET-CANARY'",
        "[composition]\nlayer_order = ['SECRET-CANARY']",
        "[composition]\nactive_profile = 'SECRET-CANARY'",
        "[composition]\nprofiles = 'SECRET-CANARY'",
        "[composition]\nactive_profile = 'a'\n[composition.profiles.a]\nextends = ['SECRET-CANARY']",
        "[server.gateway]\nenabled = true\nlisten = 'SECRET-CANARY'",
        "[appearance]\npane_border_style = 'SECRET-CANARY'",
    ] {
        let error = resolve_application_configuration_with_context(
            &document(input),
            &ConfigFamily::default(),
            None,
            None,
            &context(),
        )
        .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET-CANARY"));
        assert!(error.to_string().contains("redacted"));
    }
}

#[test]
fn malformed_source_diagnostics_keep_only_the_source_path() {
    let root = std::env::temp_dir().join(format!(
        "bmux-config-redaction-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("source.toml");
    std::fs::write(&path, "secret = 'SECRET-CANARY' ! invalid TOML").unwrap();
    let error = ConfigFamily::capture(&[ConfigSource {
        path: path.clone(),
        required: true,
    }])
    .unwrap_err();
    let message = format!("{error:?} {error}");
    assert!(message.contains(&path.to_string_lossy().into_owned()));
    assert!(!message.contains("SECRET-CANARY"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn startup_snapshot_pins_defaults_repairs_and_keeps_unknown_nested_fields() {
    let family = document(
        r#"
        [general]
        scrollback_limit = 0
        extension = "general-extension"
        [unknown_extension]
        secret = "retained-private"
        [plugins.settings.custom]
        token = "retained-token"
        [plugins]
        search_paths = ["~/plugins"]
        [recording.export]
        fps = 0
        extension = "export-extension"
        [[behavior.scrollback.pane_rules]]
        mode = "frozen"
        match_name = "pane"
        extension = "array-element-extension"
    "#,
    );
    let result = resolve(&family, &context());
    let snapshot = result.startup_snapshot().unwrap();
    assert_eq!(
        snapshot["general"]["scrollback_limit"].as_integer(),
        Some(10_000)
    );
    assert_eq!(
        snapshot["general"]["extension"].as_str(),
        Some("general-extension")
    );
    assert_eq!(
        snapshot["unknown_extension"]["secret"].as_str(),
        Some("retained-private")
    );
    assert_eq!(
        snapshot["plugins"]["settings"]["custom"]["token"].as_str(),
        Some("retained-token")
    );
    assert_eq!(
        snapshot["plugins"]["search_paths"][0].as_str(),
        Some("/captured/home/plugins")
    );
    assert_eq!(
        snapshot["recording"]["export"]["fps"].as_integer(),
        Some(i64::from(result.bmux.recording.export.fps))
    );
    assert_eq!(
        snapshot["recording"]["export"]["extension"].as_str(),
        Some("export-extension")
    );
    assert_eq!(
        snapshot["behavior"]["scrollback"]["pane_rules"][0]["extension"].as_str(),
        Some("array-element-extension")
    );
    assert!(snapshot.get("appearance").is_some());
    assert_eq!(
        result.bmux_raw["general"]["scrollback_limit"].as_integer(),
        Some(0)
    );
    let typed: BmuxConfig = snapshot.try_into().unwrap();
    assert_eq!(
        toml::Value::try_from(typed).unwrap(),
        toml::Value::try_from(&result.bmux).unwrap()
    );
}

#[test]
fn startup_snapshot_removes_known_optional_fields_absent_after_normalization() {
    let result = resolve(
        &document("[general]\ndefault_shell = 'before'\nextension = 'retained'"),
        &context(),
    );
    let mut normalized = result;
    normalized.bmux.general.default_shell = None;
    let snapshot = normalized.startup_snapshot().unwrap();
    assert!(snapshot["general"].get("default_shell").is_none());
    assert_eq!(snapshot["general"]["extension"].as_str(), Some("retained"));
}

#[test]
fn resolution_retains_unknown_extensions_and_all_original_profile_definitions() {
    let family = document(
        r#"
        [unknown_extension]
        nested = "retained"
        [composition]
        active_profile = "selected"
        [composition.profiles.selected.patch.unknown_extension]
        enabled = false
        [composition.profiles.unselected.patch.unknown_extension]
        unselected = "retained-in-source"
    "#,
    );
    let result = resolve(&family, &context());
    assert_eq!(
        result.bmux_raw["unknown_extension"]["nested"].as_str(),
        Some("retained")
    );
    assert_eq!(
        result.bmux_raw["unknown_extension"]["enabled"].as_bool(),
        Some(false)
    );
    assert_eq!(result.sources[0], family);
    assert!(
        result.sources[0].documents[0].value.as_ref().unwrap()["composition"]["profiles"]
            .get("unselected")
            .is_some()
    );
}
