//! Owner-authorized outbound pairing records, separate from inbound device grants.
use crate::{identity::DeviceIdentity, storage::PairingStore, target::IrohTarget};
use anyhow::{Result, ensure};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Outbound {
    version: u32,
    hosts: BTreeMap<String, Binding>,
}

impl Default for Outbound {
    fn default() -> Self {
        Self {
            version: 1,
            hosts: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    device_key: String,
    subject: uuid::Uuid,
}

pub fn remember(paths: &ConfigPaths, host: &str, key: &str, subject: uuid::Uuid) -> Result<()> {
    PairingStore::outbound(&paths.state_dir, &paths.runtime_dir)?.transaction(|bytes| {
        let mut state: Outbound = bytes
            .as_deref()
            .map_or_else(|| Ok(Outbound::default()), serde_json::from_slice)?;
        ensure!(
            state.version == 1,
            "unsupported outbound pairing record version"
        );
        state.hosts.insert(
            host.to_owned(),
            Binding {
                device_key: key.to_owned(),
                subject,
            },
        );
        *bytes = Some(serde_json::to_vec(&state)?);
        Ok(())
    })
}

pub fn subject(
    paths: &ConfigPaths,
    target: &IrohTarget,
    identity: &DeviceIdentity,
) -> Result<uuid::Uuid> {
    PairingStore::outbound(&paths.state_dir, &paths.runtime_dir)?.transaction(|bytes| {
        let state: Outbound = serde_json::from_slice(
            bytes
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("outbound host is not paired"))?,
        )?;
        ensure!(
            state.version == 1,
            "unsupported outbound pairing record version"
        );
        let binding = state
            .hosts
            .get(&target.endpoint_id)
            .ok_or_else(|| anyhow::anyhow!("outbound host is not paired"))?;
        ensure!(
            binding.device_key == identity.public_key(),
            "outbound device key changed"
        );
        ensure!(
            target.device_id.is_none_or(|id| id == binding.subject),
            "outbound device id mismatch"
        );
        ensure!(!binding.subject.is_nil(), "invalid outbound device subject");
        Ok(binding.subject)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &std::path::Path, name: &str) -> ConfigPaths {
        let paths = ConfigPaths::new(
            root.join("config"),
            root.join(name),
            root.join("data"),
            root.join("state"),
        );
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        paths
    }

    #[test]
    fn public_endpoint_resolves_each_runtimes_durable_private_subject() {
        let root = tempfile::tempdir().unwrap();
        let first = paths(root.path(), "first");
        let second = paths(root.path(), "second");
        let host = iroh::SecretKey::generate().public().to_string();
        let public = format!("device://{host}?addr=127.0.0.1:40001");
        let crate::target::ResolvedTarget::Iroh(mut target) =
            crate::target::resolve_internal(&bmux_config::BmuxConfig::default(), &public).unwrap()
        else {
            panic!("device target")
        };
        let first_key =
            DeviceIdentity::load_or_create(&first.state_dir, &first.runtime_dir).unwrap();
        let second_key =
            DeviceIdentity::load_or_create(&second.state_dir, &second.runtime_dir).unwrap();
        assert!(subject(&first, &target, &first_key).is_err());
        let first_subject = uuid::Uuid::new_v4();
        let second_subject = uuid::Uuid::new_v4();
        remember(&first, &host, &first_key.public_key(), first_subject).unwrap();
        remember(&second, &host, &second_key.public_key(), second_subject).unwrap();
        let reloaded =
            DeviceIdentity::load_or_create(&first.state_dir, &first.runtime_dir).unwrap();
        assert_eq!(subject(&first, &target, &reloaded).unwrap(), first_subject);
        assert_eq!(
            subject(&second, &target, &second_key).unwrap(),
            second_subject
        );
        assert!(subject(&first, &target, &second_key).is_err());
        target.device_id = Some(second_subject);
        assert!(subject(&first, &target, &first_key).is_err());
        target.device_id = None;
        target.endpoint_id = iroh::SecretKey::generate().public().to_string();
        assert!(subject(&first, &target, &first_key).is_err());
    }
}
