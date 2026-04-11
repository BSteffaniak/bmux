use bmux_plugin::{PluginManifest, discover_plugin_manifests};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should exist")
        .to_path_buf()
}

fn bundled_manifest(plugin_id: &str) -> PluginManifest {
    let plugins_root = workspace_root().join("plugins");
    let report = discover_plugin_manifests(&plugins_root).expect("manifest discovery should work");
    report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .find(|manifest| manifest.id.as_str() == plugin_id)
        .unwrap_or_else(|| panic!("{plugin_id} bundled manifest should exist"))
}

#[test]
fn bundled_plugin_manifests_include_core_shipped_plugins() {
    let plugins_root = workspace_root().join("plugins");
    let report = discover_plugin_manifests(&plugins_root).expect("manifest discovery should work");
    let manifests = report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .collect::<Vec<_>>();

    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.id.as_str() == "bmux.windows")
    );
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.id.as_str() == "bmux.cluster")
    );
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.id.as_str() == "bmux.permissions")
    );
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.id.as_str() == "bmux.plugin_cli")
    );
}

#[test]
fn bundled_cluster_manifest_matches_pragmatic_command_surface() {
    let cluster = bundled_manifest("bmux.cluster");
    let commands = cluster
        .commands
        .iter()
        .map(|command| {
            (
                command.name.as_str(),
                command.path.clone(),
                command.aliases.clone(),
            )
        })
        .collect::<Vec<_>>();

    let expected = [
        (
            "cluster-up",
            vec!["cluster-up"],
            vec![vec!["cluster", "up"]],
        ),
        (
            "cluster-status",
            vec!["cluster-status"],
            vec![vec!["cluster", "status"]],
        ),
        (
            "cluster-doctor",
            vec!["cluster-doctor"],
            vec![vec!["cluster", "doctor"]],
        ),
        (
            "cluster-hosts",
            vec!["cluster-hosts"],
            vec![vec!["cluster", "hosts"]],
        ),
        (
            "cluster-pane-new",
            vec!["cluster-pane-new"],
            vec![vec!["cluster", "pane", "new"]],
        ),
        (
            "cluster-pane-move",
            vec!["cluster-pane-move"],
            vec![vec!["cluster", "pane", "move"]],
        ),
        (
            "cluster-pane-retry",
            vec!["cluster-pane-retry"],
            vec![vec!["cluster", "pane", "retry"]],
        ),
    ];

    for (name, path, aliases) in expected {
        let entry = commands
            .iter()
            .find(|(command_name, _, _)| *command_name == name)
            .unwrap_or_else(|| panic!("missing cluster command {name}"));
        let expected_path = path.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(entry.1, expected_path, "{name} path mismatch");
        let expected_aliases = aliases
            .iter()
            .map(|alias| alias.iter().map(ToString::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(entry.2, expected_aliases, "{name} aliases mismatch");
    }
}

#[test]
fn bundled_plugin_cli_manifest_includes_recording_cut_and_path() {
    let plugin_cli = bundled_manifest("bmux.plugin_cli");
    let commands = plugin_cli
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();

    assert!(commands.contains(&"recording-cut"));
    assert!(commands.contains(&"recording-path"));
}

#[test]
fn bundled_windows_manifest_requires_generic_runtime_capabilities() {
    let windows = bundled_manifest("bmux.windows");

    let required = windows
        .required_capabilities
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert!(required.contains(&"bmux.commands".to_string()));
    assert!(required.contains(&"bmux.contexts.read".to_string()));
    assert!(required.contains(&"bmux.contexts.write".to_string()));
    assert!(required.contains(&"bmux.clients.read".to_string()));
}

#[test]
fn bundled_permissions_manifest_requires_generic_runtime_capabilities() {
    let permissions = bundled_manifest("bmux.permissions");

    let required = permissions
        .required_capabilities
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert!(required.contains(&"bmux.commands".to_string()));
    assert!(required.contains(&"bmux.sessions.read".to_string()));
    assert!(required.contains(&"bmux.clients.read".to_string()));
    assert!(required.contains(&"bmux.storage".to_string()));
}

#[test]
fn bundled_permissions_manifest_exposes_policy_service_interface() {
    let permissions = bundled_manifest("bmux.permissions");

    assert!(permissions.services.iter().any(|service| {
        service.interface_id == "session-policy-query/v1"
            && service.kind == bmux_plugin_sdk::ServiceKind::Query
    }));
}

#[test]
fn bundled_windows_manifest_exposes_window_command_service_interface() {
    let windows = bundled_manifest("bmux.windows");

    assert!(windows.services.iter().any(|service| {
        service.interface_id == "window-command/v1"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command
    }));
}

#[test]
fn bundled_windows_manifest_matches_pragmatic_command_surface() {
    let windows = bundled_manifest("bmux.windows");
    let commands = windows
        .commands
        .iter()
        .map(|command| {
            (
                command.name.as_str(),
                command.path.clone(),
                command.aliases.clone(),
            )
        })
        .collect::<Vec<_>>();

    let expected = [
        (
            "new-window",
            vec!["new-window"],
            vec![vec!["window", "new"]],
        ),
        (
            "list-windows",
            vec!["list-windows"],
            vec![vec!["window", "list"]],
        ),
        (
            "kill-window",
            vec!["kill-window"],
            vec![vec!["window", "kill"]],
        ),
        (
            "kill-all-windows",
            vec!["kill-all-windows"],
            vec![vec!["window", "kill-all"]],
        ),
        (
            "switch-window",
            vec!["switch-window"],
            vec![vec!["window", "switch"]],
        ),
        (
            "next-window",
            vec!["next-window"],
            vec![vec!["window", "next"]],
        ),
        (
            "prev-window",
            vec!["prev-window"],
            vec![vec!["window", "prev"]],
        ),
        (
            "last-window",
            vec!["last-window"],
            vec![vec!["window", "last"]],
        ),
    ];

    for (name, path, aliases) in expected {
        let entry = commands
            .iter()
            .find(|(command_name, _, _)| *command_name == name)
            .unwrap_or_else(|| panic!("missing windows command {name}"));
        let expected_path = path.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(entry.1, expected_path, "{name} path mismatch");
        let expected_aliases = aliases
            .iter()
            .map(|alias| alias.iter().map(ToString::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(entry.2, expected_aliases, "{name} aliases mismatch");
    }

    let runtime_keybindings = windows
        .keybindings
        .runtime
        .iter()
        .map(|(key, action)| (key.as_str(), action.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        runtime_keybindings.get("c").copied(),
        Some("plugin:bmux.windows:new-window")
    );
    assert_eq!(
        runtime_keybindings.get("n").copied(),
        Some("plugin:bmux.windows:next-window")
    );
    assert_eq!(
        runtime_keybindings.get("p").copied(),
        Some("plugin:bmux.windows:prev-window")
    );
    assert_eq!(
        runtime_keybindings.get("w").copied(),
        Some("plugin:bmux.windows:last-window")
    );
}

#[test]
fn bundled_permissions_manifest_matches_pragmatic_command_surface() {
    let permissions = bundled_manifest("bmux.permissions");
    let commands = permissions
        .commands
        .iter()
        .map(|command| {
            (
                command.name.as_str(),
                command.path.clone(),
                command.aliases.clone(),
            )
        })
        .collect::<Vec<_>>();

    let expected = [
        (
            "permissions",
            vec!["permissions"],
            vec![vec!["session", "permissions"]],
        ),
        (
            "permissions-current",
            vec!["permissions-current"],
            vec![vec!["session", "permissions-current"]],
        ),
        ("grant", vec!["grant"], vec![vec!["session", "grant"]]),
        ("revoke", vec!["revoke"], vec![vec!["session", "revoke"]]),
    ];

    for (name, path, aliases) in expected {
        let entry = commands
            .iter()
            .find(|(command_name, _, _)| *command_name == name)
            .unwrap_or_else(|| panic!("missing permissions command {name}"));
        let expected_path = path.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(entry.1, expected_path, "{name} path mismatch");
        let expected_aliases = aliases
            .iter()
            .map(|alias| alias.iter().map(ToString::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(entry.2, expected_aliases, "{name} aliases mismatch");
    }
}
