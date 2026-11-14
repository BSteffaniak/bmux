use crate::{PluginManifest, PluginRegistry};
use bmux_plugin_sdk::Result;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PLUGIN_MANIFEST_FILE: &str = "plugin.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDiscoveryReport {
    pub search_root: PathBuf,
    pub manifest_paths: Vec<PathBuf>,
}

/// # Errors
///
/// Returns an error when the plugin directory cannot be read.
pub fn discover_plugin_manifests(plugins_dir: &Path) -> Result<PluginDiscoveryReport> {
    if !plugins_dir.try_exists()? {
        return Ok(PluginDiscoveryReport {
            search_root: plugins_dir.to_path_buf(),
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
        search_root: plugins_dir.to_path_buf(),
        manifest_paths: manifests.into_iter().collect(),
    })
}

/// # Errors
///
/// Returns an error when any plugin search root cannot be read.
pub fn discover_plugin_manifests_in_roots(
    plugin_roots: &[PathBuf],
) -> Result<Vec<PluginDiscoveryReport>> {
    plugin_roots
        .iter()
        .map(|root| discover_plugin_manifests(root))
        .collect()
}

/// # Errors
///
/// Returns an error when manifest discovery, parsing, or registration fails.
pub fn discover_registered_plugins(plugins_dir: &Path) -> Result<PluginRegistry> {
    let report = discover_plugin_manifests(plugins_dir)?;
    let mut registry = PluginRegistry::new();
    for manifest_path in report.manifest_paths {
        let manifest = PluginManifest::from_path(&manifest_path)?;
        registry.register_manifest_from_root(&report.search_root, &manifest_path, manifest)?;
    }
    Ok(registry)
}

/// # Errors
///
/// Returns an error when manifest discovery, parsing, or registration fails.
pub fn discover_registered_plugins_in_roots(plugin_roots: &[PathBuf]) -> Result<PluginRegistry> {
    let reports = discover_plugin_manifests_in_roots(plugin_roots)?;
    let mut registry = PluginRegistry::new();
    for report in reports {
        for manifest_path in report.manifest_paths {
            let manifest = PluginManifest::from_path(&manifest_path)?;
            registry.register_manifest_from_root(&report.search_root, &manifest_path, manifest)?;
        }
    }
    Ok(registry)
}

/// Discover inert dynamic candidates without choosing providers or touching entry files.
/// The caller may add bundled candidates before applying explicit startup selection.
///
/// # Errors
/// Returns an error for unreadable roots, corrupt manifests or conflicting inventory.
/// Such failures are never reinterpreted as an empty installation.
pub fn discover_plugin_candidates_in_roots(
    plugin_roots: &[PathBuf],
) -> Result<crate::registry::PluginCandidateInventory> {
    let reports = discover_plugin_manifests_in_roots(plugin_roots)?;
    let mut inventory = crate::registry::PluginCandidateInventory::new();
    for report in reports {
        for manifest_path in report.manifest_paths {
            let manifest = PluginManifest::from_path(&manifest_path)?;
            inventory.add_dynamic_manifest(&report.search_root, &manifest_path, manifest)?;
        }
    }
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_PLUGIN_MANIFEST_FILE, discover_plugin_manifests, discover_registered_plugins,
    };
    use std::fs;
    fn temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("bmux-plugin-discovery-")
            .tempdir()
            .expect("isolated temp dir should be created")
    }

    #[test]
    fn candidate_discovery_keeps_competing_manifests_and_deduplicates_roots() {
        let root = temp_dir();
        let dir = root.path();
        let manifest =
            "id = 'example.plugin'\nname = 'Example'\nversion = '1.0.0'\nentry = 'missing.bin'\n";
        fs::write(dir.join("one.toml"), manifest).unwrap();
        fs::write(dir.join("two.toml"), manifest).unwrap();
        let inventory =
            super::discover_plugin_candidates_in_roots(&[dir.to_path_buf(), dir.to_path_buf()])
                .unwrap();
        // Competing artifacts remain visible; discovery does not choose one or open entries.
        assert_eq!(inventory.iter().count(), 2);
    }

    #[test]
    fn discovers_top_level_and_nested_plugin_manifests() {
        let root = temp_dir();
        let dir = root.path();
        let nested = dir.join("git-status");
        fs::create_dir_all(&nested).expect("nested dir should exist");
        fs::write(dir.join("top-level.toml"), "id = 'top.level'\nname = 'Top'\nversion='0.1.0'\nentry='plugin.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n")
            .expect("manifest should be written");
        fs::write(nested.join(DEFAULT_PLUGIN_MANIFEST_FILE), "id = 'nested.plugin'\nname = 'Nested'\nversion='0.1.0'\nentry='plugin.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n")
            .expect("nested manifest should be written");

        let report = discover_plugin_manifests(dir).expect("discovery should work");
        assert_eq!(report.manifest_paths.len(), 2);
    }

    #[test]
    fn registers_discovered_plugins() {
        let root = temp_dir();
        let dir = root.path();
        let plugin_dir = dir.join("git-status");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("libgit_status.dylib"), []).expect("entry should be written");
        fs::write(
            plugin_dir.join(DEFAULT_PLUGIN_MANIFEST_FILE),
            "id = 'git.status'\nname = 'Git Status'\nversion='0.1.0'\nentry='libgit_status.dylib'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
        )
        .expect("manifest should be written");

        let registry = discover_registered_plugins(dir).expect("registry should build");
        assert_eq!(registry.iter().count(), 1);
    }
}
