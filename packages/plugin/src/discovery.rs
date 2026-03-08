use crate::{PluginManifest, PluginRegistry, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "plugin.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDiscoveryReport {
    pub searched_dir: PathBuf,
    pub manifest_paths: Vec<PathBuf>,
}

/// # Errors
///
/// Returns an error when the plugin directory cannot be read.
pub fn discover_plugin_manifests(plugins_dir: &Path) -> Result<PluginDiscoveryReport> {
    if !plugins_dir.exists() {
        return Ok(PluginDiscoveryReport {
            searched_dir: plugins_dir.to_path_buf(),
            manifest_paths: Vec::new(),
        });
    }

    let mut manifests = BTreeSet::new();
    for entry in fs::read_dir(plugins_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let nested_manifest = path.join(DEFAULT_PLUGIN_MANIFEST_FILE);
            if nested_manifest.is_file() {
                manifests.insert(nested_manifest);
            }
            continue;
        }

        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == DEFAULT_PLUGIN_MANIFEST_FILE)
            || path.extension().and_then(|ext| ext.to_str()) == Some("toml")
        {
            manifests.insert(path);
        }
    }

    Ok(PluginDiscoveryReport {
        searched_dir: plugins_dir.to_path_buf(),
        manifest_paths: manifests.into_iter().collect(),
    })
}

/// # Errors
///
/// Returns an error when manifest discovery, parsing, or registration fails.
pub fn discover_registered_plugins(plugins_dir: &Path) -> Result<PluginRegistry> {
    let report = discover_plugin_manifests(plugins_dir)?;
    let mut registry = PluginRegistry::new();
    for manifest_path in report.manifest_paths {
        let manifest = PluginManifest::from_path(&manifest_path)?;
        registry.register_manifest(&manifest_path, manifest)?;
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_PLUGIN_MANIFEST_FILE, discover_plugin_manifests, discover_registered_plugins,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-plugin-discovery-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    #[test]
    fn discovers_top_level_and_nested_plugin_manifests() {
        let dir = temp_dir();
        let nested = dir.join("git-status");
        fs::create_dir_all(&nested).expect("nested dir should exist");
        fs::write(dir.join("top-level.toml"), "id = 'top.level'\nname = 'Top'\nversion='0.1.0'\nentry='plugin.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n")
            .expect("manifest should be written");
        fs::write(nested.join(DEFAULT_PLUGIN_MANIFEST_FILE), "id = 'nested.plugin'\nname = 'Nested'\nversion='0.1.0'\nentry='plugin.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n")
            .expect("nested manifest should be written");

        let report = discover_plugin_manifests(&dir).expect("discovery should work");
        assert_eq!(report.manifest_paths.len(), 2);
    }

    #[test]
    fn registers_discovered_plugins() {
        let dir = temp_dir();
        let plugin_dir = dir.join("git-status");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("libgit_status.dylib"), []).expect("entry should be written");
        fs::write(
            plugin_dir.join(DEFAULT_PLUGIN_MANIFEST_FILE),
            "id = 'git.status'\nname = 'Git Status'\nversion='0.1.0'\nentry='libgit_status.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
        )
        .expect("manifest should be written");

        let registry = discover_registered_plugins(&dir).expect("registry should build");
        assert_eq!(registry.iter().count(), 1);
    }
}
