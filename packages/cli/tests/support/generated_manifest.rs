//! The manifest a workspace plugin embeds, generated from files alone.
//!
//! A plugin's `plugin.toml` holds only settings BPDL does not model; the
//! complete manifest (services, contracts and capabilities) is generated
//! from the plugin's BPDL world. Tests that inspect a plugin's contract
//! surface read this generated manifest, exactly as `schema!` and
//! `plugin_manifest!` produce it, without linking plugin code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bmux_plugin_schema::ir::{Package, PackageSet};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("bmux_cli lives under packages/cli")
        .to_path_buf()
}

/// Every BPDL package in the workspace, keyed by plugin id.
fn workspace_packages() -> BTreeMap<String, Package> {
    let root = workspace_root();
    let mut packages = BTreeMap::new();
    for group in ["packages", "plugins", "examples"] {
        let Ok(crates) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for entry in crates.filter_map(Result::ok) {
            let Ok(files) = std::fs::read_dir(entry.path().join("bpdl")) else {
                continue;
            };
            for file in files.filter_map(Result::ok) {
                let path = file.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("bpdl") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read BPDL");
                let schema = bmux_plugin_schema::compile(&source)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                let package = bmux_plugin_schema::ir::lower(&schema);
                packages.insert(package.id.clone(), package);
            }
        }
    }
    packages
}

/// The generated manifest TOML for the plugin whose settings live in
/// `plugin_dir/plugin.toml`, or the settings verbatim when the plugin has
/// no BPDL package.
#[must_use]
pub fn generated_manifest(plugin_dir: &Path) -> String {
    let settings = std::fs::read_to_string(plugin_dir.join("plugin.toml")).expect("read settings");
    let table: toml::Table = toml::from_str(&settings).expect("settings are TOML");
    let id = table
        .get("id")
        .and_then(toml::Value::as_str)
        .expect("settings name a plugin id");
    let packages = workspace_packages();
    let Some(package) = packages.get(id) else {
        return settings;
    };
    let mut set = PackageSet::new();
    for other in packages.values() {
        set.insert(other.clone());
    }
    bmux_plugin_schema::manifest::generate(&set, package, &settings)
        .unwrap_or_else(|error| panic!("{}: {error}", plugin_dir.display()))
}
