//! Private durable remote preferences; secrets remain in the existing pairing stores.
use crate::storage::PairingStore;
use anyhow::{Result, ensure};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub enabled: Option<bool>,
    pub aliases: BTreeMap<String, String>,
    pub devices: BTreeMap<String, String>,
    #[serde(default)]
    pub terminals: BTreeMap<String, String>,
}

pub fn update<T>(
    paths: &ConfigPaths,
    change: impl FnOnce(&mut Preferences) -> Result<T>,
) -> Result<T> {
    PairingStore::remote(&paths.state_dir, &paths.runtime_dir)?.transaction(|bytes| {
        let mut value: Preferences = bytes
            .as_ref()
            .map(|b| serde_json::from_slice(b))
            .transpose()?
            .unwrap_or_default();
        let result = change(&mut value)?;
        *bytes = Some(serde_json::to_vec(&value)?);
        Ok(result)
    })
}

pub fn alias(paths: &ConfigPaths, name: &str) -> Result<Option<String>> {
    migrate_client(paths)?;
    client_update(&paths.state_dir, "default", |p| {
        Ok(p.aliases.get(name).cloned())
    })
}

/// Client-only state: never contains host enabled flags or inbound device grants.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPreferences {
    pub aliases: BTreeMap<String, String>,
    pub terminals: BTreeMap<String, String>,
    #[serde(default)]
    migrated: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientRecord {
    version: u32,
    preferences: ClientPreferences,
}

pub fn client_update<T>(
    state_dir: &std::path::Path,
    profile: &str,
    change: impl FnOnce(&mut ClientPreferences) -> Result<T>,
) -> Result<T> {
    PairingStore::client(state_dir, profile, c"preferences.json")?.transaction(|bytes| {
        let mut record = bytes
            .as_deref()
            .map_or_else(
                || {
                    Ok(ClientRecord {
                        version: 1,
                        preferences: ClientPreferences::default(),
                    })
                },
                serde_json::from_slice,
            )
            .map_err(|_| anyhow::anyhow!("invalid client preferences record"))?;
        ensure!(
            record.version == 1,
            "unsupported client preferences version"
        );
        migrate(state_dir, profile, &mut record.preferences)?;
        let result = change(&mut record.preferences)?;
        *bytes = Some(serde_json::to_vec(&record)?);
        Ok(result)
    })
}

/// Compatibility hook. Runtime is no longer required for client preferences.
pub fn migrate_client(paths: &ConfigPaths) -> Result<()> {
    client_update(&paths.state_dir, "default", |_| Ok(()))
}

fn migrate(
    state_dir: &std::path::Path,
    profile: &str,
    current: &mut ClientPreferences,
) -> Result<()> {
    if profile != "default" {
        return Ok(());
    }
    for (source, raw) in PairingStore::legacy_records(state_dir, c"remote.json")? {
        let old: Preferences = serde_json::from_slice(&raw)
            .map_err(|_| anyhow::anyhow!("invalid legacy remote preferences"))?;
        // Host enabled/devices changes must not invalidate the client migration.
        let digest = Sha256::digest(serde_json::to_vec(&(&old.aliases, &old.terminals))?)
            .iter()
            .fold(String::new(), |mut output, byte| {
                write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
                output
            });
        if let Some(previous) = current.migrated.get(&source) {
            ensure!(
                previous == &digest,
                "legacy client preferences changed after migration"
            );
            continue;
        }
        for (name, target) in old.aliases {
            name_valid(&name)?;
            ensure!(
                current
                    .aliases
                    .get(&name)
                    .is_none_or(|value| value == &target),
                "legacy host alias conflict"
            );
            current.aliases.insert(name, target);
        }
        for (name, terminal) in old.terminals {
            ensure!(
                current
                    .terminals
                    .get(&name)
                    .is_none_or(|value| value == &terminal),
                "legacy terminal preference conflict"
            );
            current.terminals.insert(name, terminal);
        }
        current.migrated.insert(source, digest);
    }
    Ok(())
}

/// Listing intentionally omits raw targets (which may carry credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAlias {
    pub name: String,
    pub terminal: Option<String>,
}

/// Resolve a saved target only for connection use; never include it in diagnostics.
pub fn host_target(
    state_dir: &std::path::Path,
    profile: &str,
    name: &str,
) -> Result<Option<String>> {
    client_update(state_dir, profile, |p| Ok(p.aliases.get(name).cloned()))
}

pub fn hosts(state_dir: &std::path::Path, profile: &str) -> Result<Vec<HostAlias>> {
    client_update(state_dir, profile, |p| {
        Ok(p.aliases
            .keys()
            .map(|name| HostAlias {
                name: name.clone(),
                terminal: p.terminals.get(name).cloned(),
            })
            .collect())
    })
}

pub fn remember_host(
    state_dir: &std::path::Path,
    profile: &str,
    name: &str,
    target: &str,
) -> Result<()> {
    name_valid(name)?;
    ensure!(!target.is_empty(), "empty host target");
    client_update(state_dir, profile, |p| {
        ensure!(
            p.aliases.get(name).is_none_or(|value| value == target),
            "host alias already exists"
        );
        p.aliases.insert(name.to_owned(), target.to_owned());
        Ok(())
    })
}

pub fn rename_host(state_dir: &std::path::Path, profile: &str, old: &str, new: &str) -> Result<()> {
    name_valid(new)?;
    client_update(state_dir, profile, |p| {
        ensure!(!p.aliases.contains_key(new), "host alias already exists");
        let target = p
            .aliases
            .remove(old)
            .ok_or_else(|| anyhow::anyhow!("unknown host alias"))?;
        p.aliases.insert(new.to_owned(), target);
        if let Some(terminal) = p.terminals.remove(old) {
            p.terminals.insert(new.to_owned(), terminal);
        }
        Ok(())
    })
}

pub fn forget_host(state_dir: &std::path::Path, profile: &str, name: &str) -> Result<bool> {
    client_update(state_dir, profile, |p| {
        p.terminals.remove(name);
        Ok(p.aliases.remove(name).is_some())
    })
}

pub fn name_valid(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name != "local"
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "name must be 1–64 letters, digits, hyphens or underscores, and not local"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn conflicting_or_corrupt_legacy_aliases_never_publish_partial_migration() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir(&state).unwrap();
        for (name, target) in [("first", "one"), ("second", "two")] {
            let paths = ConfigPaths::new(
                root.path().join("config"),
                root.path().join(name),
                root.path().join("data"),
                state.clone(),
            );
            std::fs::create_dir(&paths.runtime_dir).unwrap();
            update(&paths, |p| {
                p.aliases.insert("host".into(), target.into());
                Ok(())
            })
            .unwrap();
        }
        assert!(hosts(&state, "default").is_err());
        let destination = crate::storage::client_directory(&state, "default")
            .unwrap()
            .join("preferences.json");
        assert!(!destination.exists());
        assert_eq!(
            hosts(&state, "isolated").unwrap(),
            [] as [crate::remote_preferences::HostAlias; 0]
        );
        let records = PairingStore::legacy_records(&state, c"remote.json").unwrap();
        std::fs::write(state.join(&records[0].0).join("remote.json"), b"{invalid").unwrap();
        assert!(hosts(&state, "default").is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn client_aliases_are_durable_private_and_profile_isolated() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        remember_host(&state, "work", "host", "device://secret-target").unwrap();
        assert_eq!(
            hosts(&state, "default").unwrap(),
            [] as [crate::remote_preferences::HostAlias; 0]
        );
        assert!(!format!("{:?}", hosts(&state, "work").unwrap()).contains("secret-target"));
        rename_host(&state, "work", "host", "renamed").unwrap();
        assert_eq!(hosts(&state, "work").unwrap()[0].name, "renamed");
        assert!(forget_host(&state, "work", "renamed").unwrap());
        assert_eq!(
            hosts(&state, "work").unwrap(),
            [] as [crate::remote_preferences::HostAlias; 0]
        );
        let path = crate::storage::client_directory(&state, "work")
            .unwrap()
            .join("preferences.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(hosts(&state, "work").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt");
    }

    #[test]
    fn migration_preserves_host_config_and_forget_does_not_resurrect() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        update(&paths, |p| {
            p.enabled = Some(true);
            p.devices.insert("owner".into(), "grant".into());
            p.aliases.insert("host".into(), "target".into());
            Ok(())
        })
        .unwrap();
        let host_path = crate::storage::runtime_directory(&paths.state_dir, &paths.runtime_dir)
            .unwrap()
            .join("remote.json");
        let before = std::fs::read(&host_path).unwrap();
        assert_eq!(alias(&paths, "host").unwrap().as_deref(), Some("target"));
        assert_eq!(std::fs::read(&host_path).unwrap(), before);
        forget_host(&paths.state_dir, "default", "host").unwrap();
        assert_eq!(alias(&paths, "host").unwrap(), None);
        update(&paths, |p| {
            p.enabled = Some(false);
            Ok(())
        })
        .unwrap();
        assert_eq!(alias(&paths, "host").unwrap(), None);
        update(&paths, |p| {
            p.aliases.insert("host".into(), "changed".into());
            Ok(())
        })
        .unwrap();
        assert!(alias(&paths, "host").is_err());
        std::fs::remove_dir(&paths.runtime_dir).unwrap();
        assert!(hosts(&paths.state_dir, "default").is_err());
    }

    #[test]
    fn names_do_not_accept_targets_or_reserved_local() {
        for name in ["", "local", "device://host", "../host", "a b"] {
            assert!(name_valid(name).is_err());
        }
        assert!(name_valid("workstation-1").is_ok());
    }
}
