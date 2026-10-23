use super::*;
use bmux_config::ConfigDocument;

fn family(path: &str, value: &str) -> ConfigFamily {
    ConfigFamily {
        documents: vec![ConfigDocument {
            source: ConfigSource {
                path: PathBuf::from(path),
                required: true,
            },
            value: Some(toml::from_str(value).unwrap()),
            local: false,
        }],
    }
}

fn inputs() -> ConfigInputs {
    ConfigOptions {
        policy: SourcePolicy::Explicit,
        ..ConfigOptions::default()
    }
    .capture_selected(
        BtermPaths::from_root(PathBuf::from("/bterm-unit")).unwrap(),
        PathBuf::from("/captured-cwd"),
    )
    .unwrap()
}

#[test]
fn bmux_overlays_entire_bterm_family_but_not_native_settings() {
    let mut input = inputs();
    input.bterm = family(
        "/bterm.toml",
        "[general]\nscrollback_limit = 111\n[bterm.window]\ndecorations = false\n",
    );
    input.bmux = family(
        "/bmux.toml",
        "[general]\nscrollback_limit = 222\n[bterm.window]\ndecorations = true\n",
    );
    let resolved = input.resolve().unwrap();
    assert_eq!(resolved.bmux.general.scrollback_limit, 222);
    assert!(!resolved.native.window.decorations);
    assert!(!resolved.bmux_toml.contains("bterm"));
}

#[test]
fn defaults_once_and_invalid_native_settings_fail() {
    let mut input = inputs();
    assert!(input.resolve().unwrap().native.window.decorations);
    input.bterm = family("/invalid.toml", "[bterm.window]\ndecorations = 'false'\n");
    assert!(input.resolve().is_err());
    input.bterm = family("/invalid.toml", "[bterm.window]\nunknown_setting = true\n");
    assert!(input.resolve().is_err());
}

#[test]
fn effective_output_never_exposes_arbitrary_values() {
    let mut input = inputs();
    input.bterm = family(
        "/bterm.toml",
        "[arbitrary_extension]\nsecret = 'never-show-this-token'\n",
    );
    let resolved = input.resolve().unwrap();
    let output = resolved.effective_json().to_string();
    assert!(!output.contains("never-show-this-token"));
    assert!(!output.contains("arbitrary_extension"));
    assert!(output.contains("sha256"));
    assert_eq!(resolved.generation, input.resolve().unwrap().generation);
    input.bterm = family(
        "/bterm.toml",
        "[arbitrary_extension]\nsecret = 'different-token'\n",
    );
    assert_ne!(resolved.generation, input.resolve().unwrap().generation);
}

#[test]
fn family_sources_order_and_relative_paths_are_captured() {
    let paths = SourcePaths {
        base: Some("base.toml".into()),
        primary: Some("primary.toml".into()),
        state: Some("state.toml".into()),
        environment: Some("env.toml".into()),
        cli: Some("cli.toml".into()),
        local: vec!["local.toml".into()],
    };
    let sources = paths.sources(Path::new("/captured"));
    assert_eq!(
        sources
            .iter()
            .map(|source| source.path.clone())
            .collect::<Vec<_>>(),
        ["base", "primary", "state", "env", "cli", "local"]
            .map(|name| PathBuf::from(format!("/captured/{name}.toml")))
    );
    assert_eq!(
        sources
            .iter()
            .map(|source| source.required)
            .collect::<Vec<_>>(),
        [false, false, false, true, true, false]
    );
}

#[test]
fn missing_explicit_overlay_is_error_optional_primary_is_retained() {
    let root = std::env::temp_dir().join(format!("bterm-absent-config-{}", std::process::id()));
    let mut options = ConfigOptions {
        policy: SourcePolicy::Explicit,
        bterm: SourcePaths {
            primary: Some(root.join("optional.toml")),
            ..SourcePaths::default()
        },
        ..ConfigOptions::default()
    };
    let paths = BtermPaths::from_root(root.clone()).unwrap();
    let captured = options
        .clone()
        .capture_selected(paths.clone(), root.clone())
        .unwrap();
    assert!(captured.bterm.documents[0].value.is_none());
    assert!(!root.exists());
    options.bterm.cli = Some(root.join("required.toml"));
    assert!(options.capture_selected(paths, root.clone()).is_err());
    assert!(!root.exists());
}

#[test]
fn explicit_root_does_not_import_host_environment() {
    let env = BTreeMap::from([
        ("HOME".to_owned(), OsString::from("/host-home")),
        (
            "BTERM_CONFIG".to_owned(),
            OsString::from("/host-bterm-secret.toml"),
        ),
        (
            "BMUX_CONFIG".to_owned(),
            OsString::from("/host-bmux-secret.toml"),
        ),
        ("BMUX_STATE_DIR".to_owned(), OsString::from("/host-state")),
    ]);
    let root = std::env::temp_dir().join("bterm-explicit-unit-root");
    let captured = ConfigOptions::default()
        .capture_with_environment(root.clone(), env, Some(&root))
        .unwrap();
    assert_eq!(captured.options.policy, SourcePolicy::Explicit);
    assert!(
        captured
            .bmux
            .documents
            .iter()
            .all(|document| document.source.path.starts_with(&root))
    );
    assert_eq!(captured.paths.config_file(), root.join("config/bterm.toml"));
    assert!(
        captured
            .bterm
            .documents
            .iter()
            .all(|document| document.source.path.starts_with(&root))
    );
}

#[test]
fn supplied_environment_controls_paths_not_process_environment() {
    let env = BTreeMap::from([
        ("HOME".to_owned(), OsString::from("/captured-home")),
        (
            "BTERM_CONFIG_DIR".to_owned(),
            OsString::from("relative-config"),
        ),
        ("BTERM_DATA_DIR".to_owned(), OsString::from("/bterm-data")),
        ("BTERM_STATE_DIR".to_owned(), OsString::from("/bterm-state")),
        ("BTERM_LOG_DIR".to_owned(), OsString::from("/bterm-logs")),
        ("BTERM_CACHE_DIR".to_owned(), OsString::from("/bterm-cache")),
        ("BMUX_DATA_DIR".to_owned(), OsString::from("/host-data")),
    ]);
    let paths = BtermPaths::from_environment(Path::new("/captured-cwd"), &env).unwrap();
    assert_eq!(
        paths.config_file(),
        PathBuf::from("/captured-cwd/relative-config/bterm.toml")
    );
    assert_eq!(paths.data_dir, PathBuf::from("/bterm-data"));
    assert_eq!(paths.state_dir, PathBuf::from("/bterm-state"));
    assert_eq!(paths.log_dir, PathBuf::from("/bterm-logs"));
    assert_eq!(paths.cache_dir, PathBuf::from("/bterm-cache"));
    assert_eq!(
        paths.default_managed_root(),
        PathBuf::from("/bterm-state/bmux")
    );
}

#[test]
fn successive_sources_apply_base_primary_state_environment_cli_order() {
    let mut input = inputs();
    let mut family_sources = ConfigFamily::default();
    for (name, value) in [
        ("base", 101),
        ("primary", 202),
        ("state", 303),
        ("env", 404),
        ("cli", 505),
    ] {
        family_sources.documents.extend(
            family(
                &format!("/{name}.toml"),
                &format!("[general]\nscrollback_limit = {value}\n"),
            )
            .documents,
        );
        input.bterm = family_sources.clone();
        assert_eq!(
            input.resolve().unwrap().bmux.general.scrollback_limit,
            value
        );
    }
    input.bmux = family("/bmux-base.toml", "[general]\nscrollback_limit = 606\n");
    assert_eq!(input.resolve().unwrap().bmux.general.scrollback_limit, 606);
}

#[test]
fn invalid_bmux_schema_is_not_silently_defaulted() {
    let mut input = inputs();
    input.bmux = family(
        "/invalid-bmux.toml",
        "[general]\nscrollback_limit = 'invalid'\n",
    );
    assert!(input.resolve().is_err());
}

#[test]
fn default_source_location_is_not_managed_config_root() {
    let env = BTreeMap::from([("HOME".to_owned(), OsString::from("/captured-home"))]);
    let captured = ConfigOptions {
        policy: SourcePolicy::BtermOnly,
        ..ConfigOptions::default()
    }
    .capture_with_environment(PathBuf::from("/captured-cwd"), env, None)
    .unwrap();
    assert!(
        !captured
            .paths
            .config_file()
            .starts_with(captured.paths.default_managed_root())
    );
    assert!(captured.bmux.documents.is_empty());
}
