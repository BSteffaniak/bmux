//! Compatibility characterization for the shared-engine migration.
use super::*;

#[test]
fn inline_environment_merges_without_files_and_is_secret_safe() {
    let family = ConfigFamily::capture_layers(vec![
        ("defaults".into(), ConfigLayer::Text("[general]\nscrollback_limit=11\nserver_timeout=7".into())),
        ("environment".into(), ConfigLayer::EnvironmentText { variable: "INLINE".into() }),
        ("cli".into(), ConfigLayer::Text("[general]\nserver_timeout=99".into())),
    ], std::env::temp_dir(), std::collections::BTreeMap::from([
        ("INLINE".into(), "[general]\nscrollback_limit=42\nserver_timeout=8\n[plugins.settings.\"bmux.connections\".remote]\nauto_start=false".into()),
    ])).unwrap();
    let resolved = resolve_application_configuration_with_context(
        &ConfigFamily::default(),
        &family,
        None,
        None,
        &ConfigEvaluationContext::default(),
    )
    .unwrap();
    assert_eq!(resolved.bmux.general.scrollback_limit, 42);
    assert_eq!(resolved.bmux.general.server_timeout, 99);
    assert_eq!(
        resolved.bmux.plugins.settings["bmux.connections"]["remote"]["auto_start"].as_bool(),
        Some(false)
    );
    assert_eq!(
        family.documents[1].inline_origin.as_deref(),
        Some("environment")
    );
    let overrides = ConfigLoadOverrides {
        env_config_toml: Some("private-canary".into()),
        ..ConfigLoadOverrides::default()
    };
    assert!(!format!("{overrides:?}").contains("private-canary"));
}

#[test]
fn repeated_selected_file_paths_preserve_every_document() {
    let root = std::env::temp_dir().join(format!("bmux-config-repeat-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.toml");
    std::fs::write(&path, "[general]\nscrollback_limit=42").unwrap();
    let family = ConfigFamily::capture(&[
        ConfigSource {
            path: path.clone(),
            required: false,
        },
        ConfigSource {
            path: path.clone(),
            required: true,
        },
    ])
    .unwrap();
    assert_eq!(family.documents.len(), 2);
    assert!(
        family
            .documents
            .iter()
            .all(|document| document.value.is_some())
    );
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(root).unwrap();
}

fn value(text: &str) -> toml::Value {
    toml::from_str(text).unwrap()
}

#[cfg(unix)]
#[test]
fn capture_preserves_symlink_parent_path_semantics_and_freezes_contents() {
    let root = std::env::temp_dir().join(format!(
        "bmux-engine-capture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let nested = root.join("target/child");
    std::fs::create_dir_all(&nested).unwrap();
    let lexical = root.join("config.toml");
    let physical = root.join("target/config.toml");
    std::fs::write(&lexical, "[bterm]\nanswer = 111").unwrap();
    std::fs::write(&physical, "[bterm]\nanswer = 222").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&nested, &link).unwrap();
    let selected = link.join("../config.toml");
    let captured = ConfigFamily::capture(&[ConfigSource {
        path: selected.clone(),
        required: true,
    }])
    .unwrap();
    assert_eq!(captured.documents[0].source.path, selected);
    std::fs::write(&physical, "[bterm]\nanswer = 333").unwrap();
    let resolved = resolve_application_configuration_with_context(
        &captured,
        &ConfigFamily::default(),
        None,
        None,
        &ConfigEvaluationContext::default(),
    )
    .unwrap();
    assert_eq!(resolved.application["answer"].as_integer(), Some(222));
    std::fs::remove_file(link).unwrap();
    std::fs::remove_file(lexical).unwrap();
    std::fs::remove_file(physical).unwrap();
    std::fs::remove_dir(nested).unwrap();
    std::fs::remove_dir(root.join("target")).unwrap();
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn scoped_overlay_applies_after_default_body_and_nonmatching_body_is_ignored() {
    let raw = value(
        r#"
        [bmux.config]
        default_scope = ["session"]
        [general]
        scrollback_limit = 111
        [bmux.scopes.session.config.general]
        scrollback_limit = 222
        [bmux.scopes.context.config.general]
        scrollback_limit = 333
    "#,
    );
    let session =
        scoped_local_overlay_for_target(raw.clone(), &ConfigScopeTarget::new(" SESSION "))
            .unwrap()
            .unwrap();
    assert_eq!(
        session["general"]["scrollback_limit"].as_integer(),
        Some(222)
    );
    let context = scoped_local_overlay_for_target(raw.clone(), &ConfigScopeTarget::new("context"))
        .unwrap()
        .unwrap();
    assert_eq!(
        context["general"]["scrollback_limit"].as_integer(),
        Some(333)
    );
    assert!(
        scoped_local_overlay_for_target(raw, &ConfigScopeTarget::global())
            .unwrap()
            .is_none()
    );
}

#[test]
fn first_matching_auto_rule_uses_captured_context_and_explicit_profile_wins() {
    let mut raw = value(
        r#"
        [[composition.auto_select]]
        profile = "first"
        term_prefix = "CAPTURED"
        [[composition.auto_select]]
        profile = "second"
        [composition.profiles.first.patch.general]
        scrollback_limit = 111
        [composition.profiles.second.patch.general]
        scrollback_limit = 222
    "#,
    );
    let context = ConfigEvaluationContext {
        cwd: None,
        environment: BTreeMap::from([("TERM".into(), "captured-terminal".into())]),
    };
    let (first, resolution, _) =
        resolve_composed_config_value_with_context(&raw, value(""), &context).unwrap();
    assert_eq!(resolution.selected_profile.as_deref(), Some("first"));
    assert_eq!(first["general"]["scrollback_limit"].as_integer(), Some(111));
    apply_forced_profile(&mut raw, Some(" SECOND ")).unwrap();
    let (second, resolution, _) =
        resolve_composed_config_value_with_context(&raw, value(""), &context).unwrap();
    assert_eq!(resolution.selected_profile.as_deref(), Some("second"));
    assert_eq!(
        second["general"]["scrollback_limit"].as_integer(),
        Some(222)
    );
}

#[test]
fn scalar_root_merge_adapter_does_not_require_document_table_root() {
    let mut base = toml::Value::Integer(1);
    merge_toml_value(&mut base, toml::Value::Array(vec![]));
    assert_eq!(base, toml::Value::Array(vec![]));
}

#[test]
fn recursive_merge_replaces_arrays_and_types_but_empty_tables_do_not_clear() {
    let mut base = value("a = [1, 2]\nb = 1\n[t]\nkeep = true\n[nested]\na = 1\nb = 2");
    merge_toml_value(
        &mut base,
        value("a = []\nb = { child = false }\nt = {}\n[nested]\na = 3"),
    );
    assert_eq!(
        base,
        value("a = []\nb = { child = false }\n[t]\nkeep = true\n[nested]\na = 3\nb = 2")
    );
    merge_toml_value(&mut base, value("nested = false"));
    assert_eq!(base["nested"].as_bool(), Some(false));
}

#[test]
fn profile_diamond_reapplies_shared_ancestor_and_canonicalizes_parent_ids() {
    let profiles = BTreeMap::from([
        (
            "root".into(),
            CompositionProfile {
                extends: vec![],
                patch: value("answer = 1\nlist = [1]").as_table().unwrap().clone(),
            },
        ),
        (
            "left".into(),
            CompositionProfile {
                extends: vec![" ROOT ".into()],
                patch: value("answer = 2\nlist = []").as_table().unwrap().clone(),
            },
        ),
        (
            "right".into(),
            CompositionProfile {
                extends: vec!["root".into()],
                patch: toml::Table::new(),
            },
        ),
        (
            "child".into(),
            CompositionProfile {
                extends: vec!["left".into(), "right".into()],
                patch: value("child = true").as_table().unwrap().clone(),
            },
        ),
    ]);
    let patch = resolve_profile_patch(" CHILD ", &profiles).unwrap();
    assert_eq!(patch["answer"].as_integer(), Some(1));
    assert_eq!(patch["list"].as_array().unwrap().len(), 1);
    assert_eq!(patch["child"].as_bool(), Some(true));
}

#[test]
fn composition_consumes_already_merged_sources_before_applying_profile_layers() {
    let mut raw = value(
        r#"
        [general]
        scrollback_limit = 101
        [composition]
        active_profile = "selected"
        layer_order = ["config", "profile:active", "config"]
        [composition.profiles.selected.patch.general]
        scrollback_limit = 202
        default_shell = "profile-shell"
    "#,
    );
    merge_toml_value(&mut raw, value("[general]\nscrollback_limit = 303"));
    let (resolved, resolution, layers) = resolve_composed_config_value_with_context(
        &raw,
        value(""),
        &ConfigEvaluationContext::default(),
    )
    .unwrap();
    assert_eq!(
        resolved["general"]["scrollback_limit"].as_integer(),
        Some(303)
    );
    assert_eq!(
        resolved["general"]["default_shell"].as_str(),
        Some("profile-shell")
    );
    assert_eq!(resolution.selected_profile.as_deref(), Some("selected"));
    assert_eq!(layers.len(), 3);
    assert!(resolved.get("composition").is_none());
}

#[test]
fn repeated_parent_chain_uses_cached_complete_patches_and_preserves_replacement() {
    // Expanding both edges recursively would visit more than 2^100 branches.
    // Legacy memoization resolves each patch once but still applies both edges.
    let profiles = (0..100)
        .map(|index| {
            (
                format!("p{index}"),
                CompositionProfile {
                    extends: if index == 0 {
                        vec![]
                    } else {
                        vec![format!("p{}", index - 1); 2]
                    },
                    patch: value(&format!("answer = {index}\nitems = [{index}]"))
                        .as_table()
                        .unwrap()
                        .clone(),
                },
            )
        })
        .collect();
    let resolved = resolve_profile_patch("p99", &profiles).unwrap();
    assert_eq!(resolved["answer"].as_integer(), Some(99));
    assert_eq!(
        resolved["items"],
        toml::Value::Array(vec![toml::Value::Integer(99)])
    );
}

#[test]
fn deep_valid_profile_graph_is_not_limited_by_engine_defaults() {
    let profiles = (0..160)
        .map(|index| {
            (
                format!("p{index}"),
                CompositionProfile {
                    extends: if index == 0 {
                        vec![]
                    } else {
                        vec![format!("p{}", index - 1)]
                    },
                    patch: value(&format!("answer = {index}"))
                        .as_table()
                        .unwrap()
                        .clone(),
                },
            )
        })
        .collect();
    let resolved = resolve_profile_patch("p159", &profiles).unwrap();
    assert_eq!(resolved["answer"].as_integer(), Some(159));
}

#[test]
fn merge_adapter_preserves_all_toml_scalar_types_and_datetime() {
    let input = value(
        r#"
        string = "private"
        integer = 9223372036854775807
        float = 1.25
        boolean = false
        date = 1979-05-27
        time = 07:32:00
        datetime = 1979-05-27T07:32:00.123-07:00
        nested = [{ date = 2000-01-01 }, { array = [true, 0, ""] }]
    "#,
    );
    let mut merged = value("");
    merge_toml_value(&mut merged, input.clone());
    assert_eq!(merged, input);
}

#[test]
fn unknown_profile_and_cycle_preserve_legacy_error_fields() {
    let profiles = BTreeMap::from([(
        "cycle".into(),
        CompositionProfile {
            extends: vec![" CYCLE ".into()],
            patch: toml::Table::new(),
        },
    )]);
    for (name, expected) in [
        ("missing", "composition.active_profile"),
        ("cycle", "composition.profiles"),
    ] {
        let ConfigError::InvalidValue { field, .. } =
            resolve_profile_patch(name, &profiles).unwrap_err()
        else {
            panic!("unexpected error")
        };
        assert_eq!(field, expected);
    }
}
