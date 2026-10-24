//! Private durable remote preferences; secrets remain in the existing pairing stores.
use crate::storage::PairingStore;
use anyhow::{Result, ensure};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    update(paths, |p| Ok(p.aliases.get(name).cloned()))
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
    #[test]
    fn names_do_not_accept_targets_or_reserved_local() {
        for name in ["", "local", "device://host", "../host", "a b"] {
            assert!(name_valid(name).is_err());
        }
        assert!(name_valid("workstation-1").is_ok());
    }
}
