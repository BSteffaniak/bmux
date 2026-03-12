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
