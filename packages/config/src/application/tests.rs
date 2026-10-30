use super::*;

fn family(input: &str, path: &str) -> ConfigFamily {
    ConfigFamily {
        documents: vec![ConfigDocument {
            source: ConfigSource {
                path: path.into(),
                required: true,
            },
            value: Some(toml::from_str(input).unwrap()),
            local: false,
            inline_origin: None,
        }],
    }
}

fn resolve(bterm: &str, bmux: &str) -> ApplicationConfiguration {
    resolve_application_configuration(
        &family(bterm, "/application/bterm.toml"),
        &family(bmux, "/host/bmux.toml"),
        None,
        None,
    )
    .unwrap()
}

#[test]
fn sparse_families_apply_defaults_once_and_preserve_explicit_values() {
    let result = resolve(
        r#"
        [general]
        default_shell = "custom-shell"
        scrollback_limit = 4321
        [bterm]
        enabled = false
        count = 0
        label = ""
        list = []
        [bterm.nested]
        retained = true
    "#,
        r#"
        [general]
        scrollback_limit = 9876
        [bterm]
        enabled = true
        label = "must not win"
    "#,
    );
    assert_eq!(
        result.bmux.general.default_shell.as_deref(),
        Some("custom-shell")
    );
    assert_eq!(result.bmux.general.scrollback_limit, 9876);
    assert_eq!(result.application["enabled"].as_bool(), Some(false));
    assert_eq!(result.application["count"].as_integer(), Some(0));
    assert_eq!(result.application["label"].as_str(), Some(""));
    assert_eq!(
        result.application["list"].as_array().unwrap().as_slice(),
        []
    );
    assert!(result.bmux_raw.get("bterm").is_none());
    assert!(result.bmux_raw.get("appearance").is_none());
    let snapshot = toml::to_string(&result.bmux_raw).unwrap();
    let restored = BmuxConfig::from_resolved_raw_value(toml::from_str(&snapshot).unwrap()).unwrap();
    assert_eq!(
        restored.general.default_shell,
        result.bmux.general.default_shell
    );
}

#[test]
fn bmux_profile_effects_win_without_overwriting_other_application_values() {
    let result = resolve(
        r#"
        [general]
        default_shell = "application"
        scrollback_limit = 4321
        [bterm]
        keep = true
        [composition]
        active_profile = "app"
        layer_order = ["config", "profile:active"]
        [composition.profiles.base.patch.bterm]
        values = [1, 2]
        [composition.profiles.app]
        extends = ["base"]
        [composition.profiles.app.patch.bterm]
        values = []
    "#,
        r#"
        [composition]
        active_profile = "host"
        [composition.profiles.host.patch.general]
        scrollback_limit = 2222
        [composition.profiles.host.patch.bterm]
        keep = false
    "#,
    );
    assert_eq!(
        result.bmux.general.default_shell.as_deref(),
        Some("application")
    );
    assert_eq!(result.bmux.general.scrollback_limit, 2222);
    assert_eq!(result.application["keep"].as_bool(), Some(true));
    assert_eq!(
        result.application["values"].as_array().unwrap().as_slice(),
        []
    );
    assert_eq!(
        result
            .application_family
            .resolution
            .selected_profile
            .as_deref(),
        Some("app")
    );
    assert_eq!(
        result.bmux_family.resolution.selected_profile.as_deref(),
        Some("host")
    );
    assert_eq!(
        result.application_family.origins[&vec!["bterm".into(), "values".into()]],
        PathBuf::from("/application/bterm.toml")
    );
}

#[test]
fn auto_selection_inheritance_and_layer_order_share_existing_semantics() {
    let input = r#"
        [general]
        default_shell = "config"
        [[composition.auto_select]]
        profile = "child"
        [composition.profiles.parent.patch.general]
        default_shell = "parent"
        [composition.profiles.parent.patch.bterm]
        inherited = true
        [composition.profiles.child]
        extends = ["parent"]
        [composition.profiles.child.patch.bterm]
        selected = true
    "#;
    let result = resolve(input, "");
    assert_eq!(result.bmux.general.default_shell.as_deref(), Some("config"));
    assert_eq!(result.application["inherited"].as_bool(), Some(true));
    assert_eq!(
        result
            .application_family
            .resolution
            .matched_auto_select_index,
        Some(0)
    );
    let result = resolve(&format!("{input}\n"), "");
    assert_eq!(result.application["selected"].as_bool(), Some(true));
    let result = resolve(&input.replace("[[composition.auto_select]]", "[composition]\nlayer_order = ['config', 'profile:active']\n[[composition.auto_select]]"), "");
    assert_eq!(result.bmux.general.default_shell.as_deref(), Some("parent"));
}

#[test]
fn origin_paths_follow_winning_source_and_profile_patches() {
    let mut app = family(
        r#"
        [plugins]
        search_paths = ["base"]
        [composition]
        active_profile = "app"
        layer_order = ["config", "profile:active"]
        [composition.profiles.app.patch.plugins]
        search_paths = ["profile"]
    "#,
        "/first/bterm.toml",
    );
    app.documents.extend(
        family(
            r#"
        [composition.profiles.app.patch.bterm]
        asset = "relative.png"
    "#,
            "/second/overlay.toml",
        )
        .documents,
    );
    let result =
        resolve_application_configuration(&app, &ConfigFamily::default(), None, None).unwrap();
    assert_eq!(
        result.bmux.plugins.search_paths,
        vec![PathBuf::from("/first/profile")]
    );
    assert_eq!(
        result.application_family.origins[&vec!["bterm".into(), "asset".into()]],
        PathBuf::from("/second/overlay.toml")
    );
    assert_eq!(result.application["asset"].as_str(), Some("relative.png"));
    let result = resolve_application_configuration(
        &app,
        &family("[plugins]\nsearch_paths = ['host']", "/host/config.toml"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        result.bmux.plugins.search_paths,
        vec![PathBuf::from("/host/host")]
    );
}

#[test]
fn invalid_overlay_and_invalid_profiles_are_errors() {
    for input in [
        "[general]\nscrollback_limit = 'bad'",
        "[composition]\nactive_profile = 'missing'",
        "[composition]\nlayer_order = ['unknown']",
        "[composition]\nactive_profile = 'cycle'\n[composition.profiles.cycle]\nextends = ['cycle']",
    ] {
        assert!(
            resolve_application_configuration(
                &ConfigFamily::default(),
                &family(input, "/host/config.toml"),
                None,
                None
            )
            .is_err(),
            "{input}"
        );
    }
}

#[test]
fn scoped_documents_use_existing_scope_policy() {
    let mut app = family(
        r"
        [bterm]
        local = true
        [bmux.scopes.global.config.bterm]
        selected = true
    ",
        "/project/bterm.toml",
    );
    app.documents[0].local = true;
    let result = resolve_application_configuration(
        &app,
        &ConfigFamily::default(),
        None,
        Some(&ConfigScopeTarget::global()),
    )
    .unwrap();
    assert!(result.application.get("local").is_none());
    assert_eq!(result.application["selected"].as_bool(), Some(true));
    let result = resolve_application_configuration(
        &app,
        &ConfigFamily::default(),
        None,
        Some(&ConfigScopeTarget::new("pane")),
    )
    .unwrap();
    assert_eq!(result.application["local"].as_bool(), Some(true));
}

#[test]
fn forced_profile_applies_to_each_family() {
    let app = family(
        "[composition.profiles.shared.patch.bterm]\nenabled = false",
        "/app/bterm.toml",
    );
    let host = family(
        "[composition.profiles.shared.patch.general]\ndefault_shell = 'forced'",
        "/host/bmux.toml",
    );
    let result = resolve_application_configuration(&app, &host, Some("shared"), None).unwrap();
    assert_eq!(result.bmux.general.default_shell.as_deref(), Some("forced"));
    assert_eq!(result.application["enabled"].as_bool(), Some(false));
}

#[test]
fn sparse_resolution_matches_standalone_for_single_family() {
    for input in [
        "",
        "[general]\ndefault_shell = 'single'\n[web]\nport = 0\nallowed_hosts = []",
        "[composition]\nactive_profile = 'vim'",
        "[composition]\nactive_profile = 'custom'\nlayer_order = ['config', 'profile:active']\n[composition.profiles.custom.patch.web]\nport = 0",
    ] {
        let raw = toml::from_str(input).unwrap();
        let standalone = BmuxConfig::load_from_raw_value_with_resolution(Some(raw), None)
            .unwrap()
            .0;
        let composed = resolve(input, "");
        assert_eq!(
            toml::Value::try_from(standalone).unwrap(),
            toml::Value::try_from(composed.bmux).unwrap(),
            "{input}"
        );
    }
}

#[test]
fn multiple_sources_deep_merge_and_replace_arrays_without_defaults() {
    let mut app = family(
        r#"
        [bterm]
        items = [1, 2]
        [bterm.nested]
        keep = true
        replaced = true
        [general]
        default_shell = "retained"
    "#,
        "/base/bterm.toml",
    );
    app.documents.extend(
        family(
            r"
        [bterm]
        items = []
        [bterm.nested]
        replaced = false
        count = 0
    ",
            "/overlay/bterm.toml",
        )
        .documents,
    );
    let result =
        resolve_application_configuration(&app, &ConfigFamily::default(), None, None).unwrap();
    assert_eq!(
        result.application["items"].as_array().unwrap().as_slice(),
        []
    );
    assert_eq!(result.application["nested"]["keep"].as_bool(), Some(true));
    assert_eq!(
        result.application["nested"]["replaced"].as_bool(),
        Some(false)
    );
    assert_eq!(result.application["nested"]["count"].as_integer(), Some(0));
    assert_eq!(
        result.bmux.general.default_shell.as_deref(),
        Some("retained")
    );
    assert_eq!(
        result.application_family.origins[&vec!["bterm".into(), "items".into()]],
        PathBuf::from("/overlay/bterm.toml")
    );
    assert_eq!(
        result.application_family.origins[&vec!["bterm".into(), "nested".into(), "keep".into()]],
        PathBuf::from("/base/bterm.toml")
    );
}

#[test]
fn discarded_invalid_bmux_profile_cannot_hide_an_application_field() {
    let app = family("[general]\ndefault_shell = 'retained'", "/app/bterm.toml");
    let host = family(
        r#"
        [composition]
        layer_order = ["config"]
        [composition.profiles.unused.patch.general]
        scrollback_limit = "not selected"
        [bterm]
        arbitrary = 123
    "#,
        "/host/bmux.toml",
    );
    let result = resolve_application_configuration(&app, &host, None, None).unwrap();
    assert_eq!(
        result.bmux.general.default_shell.as_deref(),
        Some("retained")
    );
    assert!(result.application.as_table().unwrap().is_empty());
}

#[test]
fn capture_freezes_documents_and_host_discovery_keeps_layer_order() {
    let _lock = crate::tests::env_lock().lock().expect("env lock");
    let root = std::env::temp_dir().join(format!(
        "bmux-application-capture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let paths = ConfigPaths::new(
        root.join("config"),
        root.join("runtime"),
        root.join("data"),
        root.join("state"),
    );
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    let state = paths.state_config_file();
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let env = root.join("environment.toml");
    let cli = root.join("cli.toml");
    std::fs::write(
        paths.config_file(),
        "[general]\ndefault_shell = 'primary'\n[web]\nport = 1000",
    )
    .unwrap();
    std::fs::write(&state, "[web]\nport = 2000").unwrap();
    std::fs::write(&env, "[web]\nport = 3000").unwrap();
    std::fs::write(&cli, "[web]\nport = 0").unwrap();
    let family = capture_host_configuration(
        &paths,
        &ConfigLoadOverrides {
            env_config_path: Some(env),
            env_config_toml: None,
            cli_config_path: Some(cli.clone()),
            ..ConfigLoadOverrides::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(family.documents.len(), 4);
    std::fs::write(&cli, "this is now malformed TOML").unwrap();
    let result =
        resolve_application_configuration(&ConfigFamily::default(), &family, None, None).unwrap();
    assert_eq!(result.bmux.web.port, 0);
    assert_eq!(
        result.bmux.general.default_shell.as_deref(),
        Some("primary")
    );
    assert_eq!(
        result.bmux_family.origins[&vec!["web".into(), "port".into()]],
        cli
    );
    assert!(
        ConfigFamily::capture(&[ConfigSource {
            path: cli,
            required: true
        }])
        .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_sources_ignore_process_overrides_and_record_missing_inputs() {
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            // SAFETY: this guard is dropped before the shared env lock.
            unsafe {
                if let Some(previous) = &self.0 {
                    std::env::set_var(super::super::BMUX_CONFIG_ENV, previous);
                } else {
                    std::env::remove_var(super::super::BMUX_CONFIG_ENV);
                }
            }
        }
    }
    let _lock = crate::tests::env_lock().lock().expect("env lock");
    let previous = std::env::var_os(super::super::BMUX_CONFIG_ENV);
    // SAFETY: configuration environment tests share the crate's env lock.
    unsafe {
        std::env::set_var(
            super::super::BMUX_CONFIG_ENV,
            "/nonexistent/ambient-env-config.toml",
        );
    }
    let _restore = RestoreEnv(previous);
    let _guard = super::super::push_process_config_overrides(ConfigLoadOverrides {
        env_config_path: Some(PathBuf::from("/nonexistent/ambient-bmux-config.toml")),
        ..ConfigLoadOverrides::default()
    });
    let source = ConfigSource {
        path: PathBuf::from("/nonexistent/optional-bterm-config.toml"),
        required: false,
    };
    let empty = ConfigFamily::capture(std::slice::from_ref(&source)).unwrap();
    assert!(empty.documents[0].value.is_none());
    let result = resolve_application_configuration(&empty, &empty, None, None).unwrap();
    assert!(result.bmux_raw.as_table().unwrap().is_empty());
    assert!(
        ConfigFamily::capture(&[ConfigSource {
            required: true,
            ..source
        }])
        .is_err()
    );
}
