use bmux_plugin::{PluginManifest, discover_plugin_manifests};
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root should exist")
        .to_path_buf()
}

#[test]
fn bundled_plugin_manifests_include_windows_and_permissions() {
    let bundled_root = workspace_root().join("plugins").join("bundled");
    let report = discover_plugin_manifests(&bundled_root).expect("manifest discovery should work");
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
            .any(|manifest| manifest.id.as_str() == "bmux.permissions")
    );
}

#[test]
fn bundled_windows_manifest_requires_generic_runtime_capabilities() {
    let bundled_root = workspace_root().join("plugins").join("bundled");
    let report = discover_plugin_manifests(&bundled_root).expect("manifest discovery should work");
    let windows = report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .find(|manifest| manifest.id.as_str() == "bmux.windows")
        .expect("windows bundled manifest should exist");

    let required = windows
        .required_capabilities
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert!(required.contains(&"bmux.commands".to_string()));
    assert!(required.contains(&"bmux.sessions.read".to_string()));
    assert!(required.contains(&"bmux.sessions.write".to_string()));
    assert!(required.contains(&"bmux.panes.read".to_string()));
    assert!(required.contains(&"bmux.clients.read".to_string()));
}

#[test]
fn bundled_permissions_manifest_requires_generic_runtime_capabilities() {
    let bundled_root = workspace_root().join("plugins").join("bundled");
    let report = discover_plugin_manifests(&bundled_root).expect("manifest discovery should work");
    let permissions = report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .find(|manifest| manifest.id.as_str() == "bmux.permissions")
        .expect("permissions bundled manifest should exist");

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
    let bundled_root = workspace_root().join("plugins").join("bundled");
    let report = discover_plugin_manifests(&bundled_root).expect("manifest discovery should work");
    let permissions = report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .find(|manifest| manifest.id.as_str() == "bmux.permissions")
        .expect("permissions bundled manifest should exist");

    assert!(permissions.services.iter().any(|service| {
        service.interface_id == "session-policy-query/v1"
            && service.kind == bmux_plugin::ServiceKind::Query
    }));
}

#[test]
fn bundled_windows_manifest_exposes_window_command_service_interface() {
    let bundled_root = workspace_root().join("plugins").join("bundled");
    let report = discover_plugin_manifests(&bundled_root).expect("manifest discovery should work");
    let windows = report
        .manifest_paths
        .iter()
        .map(|path| PluginManifest::from_path(path).expect("manifest should parse"))
        .find(|manifest| manifest.id.as_str() == "bmux.windows")
        .expect("windows bundled manifest should exist");

    assert!(windows.services.iter().any(|service| {
        service.interface_id == "window-command/v1"
            && service.kind == bmux_plugin::ServiceKind::Command
    }));
}
