//! Owner-authorized outbound pairing records, separate from inbound device grants.
use crate::{identity::DeviceIdentity, storage::PairingStore, target::IrohTarget};
use anyhow::{Result, ensure};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Outbound {
    version: u32,
    hosts: BTreeMap<String, Binding>,
    #[serde(default)]
    migrated: BTreeMap<String, String>,
}

impl Default for Outbound {
    fn default() -> Self {
        Self {
            version: 1,
            hosts: BTreeMap::new(),
            migrated: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    device_key: String,
    subject: uuid::Uuid,
}

pub fn remember(paths: &ConfigPaths, host: &str, key: &str, subject: uuid::Uuid) -> Result<()> {
    remember_local(&paths.state_dir, "default", host, key, subject)
}

pub fn remember_local(
    state_dir: &Path,
    profile: &str,
    host: &str,
    key: &str,
    subject: uuid::Uuid,
) -> Result<()> {
    ensure!(!subject.is_nil(), "invalid outbound device subject");
    let _: iroh::PublicKey = host
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid host endpoint key"))?;
    let _: iroh::PublicKey = key
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid device key"))?;
    PairingStore::client(state_dir, profile, c"outbound.json")?.transaction(|bytes| {
        let mut state: Outbound = bytes
            .as_deref()
            .map_or_else(|| Ok(Outbound::default()), serde_json::from_slice)
            .map_err(|_| anyhow::anyhow!("invalid outbound pairing record"))?;
        ensure!(
            state.version == 1,
            "unsupported outbound pairing record version"
        );
        migrate(state_dir, profile, &mut state)?;
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
    subject_local(&paths.state_dir, "default", target, identity)
}

pub fn subject_local(
    state_dir: &Path,
    profile: &str,
    target: &IrohTarget,
    identity: &DeviceIdentity,
) -> Result<uuid::Uuid> {
    PairingStore::client(state_dir, profile, c"outbound.json")?.transaction(|bytes| {
        let mut state: Outbound = bytes
            .as_deref()
            .map_or_else(|| Ok(Outbound::default()), serde_json::from_slice)
            .map_err(|_| anyhow::anyhow!("invalid outbound pairing record"))?;
        migrate(state_dir, profile, &mut state)?;

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
        let subject = binding.subject;
        *bytes = Some(serde_json::to_vec(&state)?);
        Ok(subject)
    })
}

/// Import client bindings from legacy stores only into the default profile.
fn migrate(state_dir: &Path, profile: &str, current: &mut Outbound) -> Result<()> {
    if profile != "default" {
        return Ok(());
    }
    for (source, legacy) in PairingStore::legacy_records(state_dir, c"outbound.json")? {
        let old: Outbound = serde_json::from_slice(&legacy)
            .map_err(|_| anyhow::anyhow!("invalid legacy outbound record"))?;
        ensure!(old.version == 1, "unsupported legacy outbound version");
        ensure!(current.version == 1, "unsupported outbound version");
        let digest = Sha256::digest(&legacy)
            .iter()
            .fold(String::new(), |mut output, byte| {
                write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
                output
            });
        if let Some(previous) = current.migrated.get(&source) {
            ensure!(
                previous == &digest,
                "legacy outbound record changed after migration"
            );
            continue;
        }
        current.migrated.insert(source, digest);
        for (host, binding) in old.hosts {
            ensure!(
                current
                    .hosts
                    .get(&host)
                    .is_none_or(|value| value == &binding),
                "legacy outbound binding conflict"
            );
            current.hosts.insert(host, binding);
        }
    }
    Ok(())
}

pub fn forget_local(state_dir: &Path, profile: &str, endpoint_id: &str) -> Result<bool> {
    PairingStore::client(state_dir, profile, c"outbound.json")?.transaction(|bytes| {
        let mut state: Outbound = bytes
            .as_deref()
            .map_or_else(|| Ok(Outbound::default()), serde_json::from_slice)
            .map_err(|_| anyhow::anyhow!("invalid outbound pairing record"))?;
        ensure!(state.version == 1, "unsupported outbound version");
        migrate(state_dir, profile, &mut state)?;
        let removed = state.hosts.remove(endpoint_id).is_some();
        *bytes = Some(serde_json::to_vec(&state)?);
        Ok(removed)
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
            root.join(format!("state-{name}")),
        );
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        paths
    }

    #[test]
    fn migrated_binding_survives_runtime_removal_and_forget_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path(), "legacy");
        let key = DeviceIdentity::load_or_create_local(&paths.state_dir, "default").unwrap();
        let host = iroh::SecretKey::generate().public().to_string();
        let subject = uuid::Uuid::new_v4();
        let legacy = Outbound {
            hosts: BTreeMap::from([(
                host.clone(),
                Binding {
                    device_key: key.public_key(),
                    subject,
                },
            )]),
            ..Outbound::default()
        };
        PairingStore::outbound(&paths.state_dir, &paths.runtime_dir)
            .unwrap()
            .transaction(|bytes| {
                *bytes = Some(serde_json::to_vec(&legacy)?);
                Ok(())
            })
            .unwrap();
        std::fs::remove_dir(&paths.runtime_dir).unwrap();
        let crate::target::ResolvedTarget::Iroh(target) = crate::target::resolve_internal(
            &bmux_config::BmuxConfig::default(),
            &format!("device://{host}"),
        )
        .unwrap() else {
            panic!("iroh target")
        };
        assert_eq!(
            subject_local(&paths.state_dir, "default", &target, &key).unwrap(),
            subject
        );
        assert!(subject_local(&paths.state_dir, "work", &target, &key).is_err());
        assert!(forget_local(&paths.state_dir, "default", &host).unwrap());
        assert!(subject_local(&paths.state_dir, "default", &target, &key).is_err());
        assert!(!forget_local(&paths.state_dir, "default", &host).unwrap());
    }

    #[test]
    fn concurrent_writes_and_corruption_are_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();
        let key = DeviceIdentity::load_or_create_local(state, "default").unwrap();
        let host = iroh::SecretKey::generate().public().to_string();
        let subject = uuid::Uuid::new_v4();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let host = &host;
                let key = &key;
                scope.spawn(move || {
                    remember_local(state, "default", host, &key.public_key(), subject).unwrap();
                });
            }
        });
        let path = crate::storage::client_directory(state, "default")
            .unwrap()
            .join("outbound.json");
        let before = std::fs::read(&path).unwrap();
        let mut record: Outbound = serde_json::from_slice(&before).unwrap();
        record.version = 999;
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let corrupt = std::fs::read(&path).unwrap();
        assert!(remember_local(state, "default", &host, &key.public_key(), subject).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
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
