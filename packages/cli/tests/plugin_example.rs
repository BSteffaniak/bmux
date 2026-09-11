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
            .any(|manifest| manifest.id.as_str() == "bmux.tabs")
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
fn bundled_recording_manifest_owns_recording_cli_namespace() {
    let recording = bundled_manifest("bmux.recording");
    assert!(recording.owns_namespaces.contains("recording"));
    assert!(
        recording
            .owns_paths
            .iter()
            .any(|path| path.0 == vec!["server".to_string(), "recording".to_string()])
    );
    assert!(
        recording
            .owns_paths
            .iter()
            .any(|path| path.0 == vec!["playbook".to_string(), "from-recording".to_string()])
    );

    let commands = recording
        .commands
        .iter()
        .map(|command| (command.name.as_str(), command.expose_in_cli))
        .collect::<Vec<_>>();
    assert!(commands.contains(&("recording-cut", true)));
    assert!(commands.contains(&("recording-path", true)));
    assert!(commands.contains(&("recording-export", true)));
    assert!(commands.contains(&("server-recording", true)));
    assert!(commands.contains(&("playbook-from-recording", true)));
}

#[test]
fn bundled_plugin_cli_manifest_does_not_proxy_recording_commands() {
    let plugin_cli = bundled_manifest("bmux.plugin_cli");
    let commands = plugin_cli
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();

    assert!(
        !commands
            .iter()
            .any(|command| command.starts_with("recording-"))
    );
    assert!(
        !plugin_cli
            .owns_paths
            .iter()
            .any(|path| path.0.first().is_some_and(|segment| segment == "recording"))
    );
}

#[test]
fn bundled_tabs_manifest_requires_generic_runtime_capabilities() {
    let windows = bundled_manifest("bmux.tabs");

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
        service.interface_id == "session-policy-state"
            && service.kind == bmux_plugin_sdk::ServiceKind::Query
    }));
}

#[test]
fn bundled_tabs_manifest_exposes_tab_command_service_interface() {
    let windows = bundled_manifest("bmux.tabs");

    assert!(windows.services.iter().any(|service| {
        service.interface_id == "tabs-commands"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command
    }));
}

#[test]
fn bundled_tabs_manifest_matches_pragmatic_command_surface() {
    let windows = bundled_manifest("bmux.tabs");
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
        ("new-tab", vec!["new-tab"], vec![vec!["tab", "new"]]),
        ("list-tabs", vec!["list-tabs"], vec![vec!["tab", "list"]]),
        ("kill-tab", vec!["kill-tab"], vec![vec!["tab", "kill"]]),
        (
            "kill-all-tabs",
            vec!["kill-all-tabs"],
            vec![vec!["tab", "kill-all"]],
        ),
        (
            "switch-tab",
            vec!["switch-tab"],
            vec![vec!["tab", "switch"]],
        ),
        ("next-tab", vec!["next-tab"], vec![vec!["tab", "next"]]),
        ("prev-tab", vec!["prev-tab"], vec![vec!["tab", "prev"]]),
        ("last-tab", vec!["last-tab"], vec![vec!["tab", "last"]]),
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
        Some("plugin:bmux.tabs:new-tab")
    );
    assert_eq!(
        runtime_keybindings.get("n").copied(),
        Some("plugin:bmux.tabs:next-tab")
    );
    assert_eq!(
        runtime_keybindings.get("p").copied(),
        Some("plugin:bmux.tabs:prev-tab")
    );
    assert_eq!(
        runtime_keybindings.get("w").copied(),
        Some("plugin:bmux.tabs:last-tab")
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
