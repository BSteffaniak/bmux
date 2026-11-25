//! Machine host key and per-profile device keys, both used directly as Iroh
//! endpoint keys. Key purposes stay separate: a host key is never a device key.
//!
//! - host key: `state_dir/connections/host.key`
//! - device key: `state_dir/connections/devices/<profile>.key`
use crate::storage::{self, HeldLock, PrivateDirectory};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::path::Path;

const HOST_PURPOSE: &str = "bmux.connections.host.v1";
const DEVICE_PURPOSE: &str = "bmux.connections.device.v1";
const KEY_LIMIT: u64 = 4_096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyRecord {
    version: u32,
    purpose: String,
    secret: [u8; 32],
    public: String,
}

fn load_or_create(
    directory: &PrivateDirectory,
    name: &str,
    purpose: &str,
) -> Result<iroh::SecretKey> {
    let name = CString::new(name)?;
    let bytes = if let Some(bytes) = directory.read_record(&name, KEY_LIMIT)? {
        bytes
    } else {
        let secret = iroh::SecretKey::generate();
        let record = KeyRecord {
            version: 1,
            purpose: purpose.to_owned(),
            secret: secret.to_bytes(),
            public: secret.public().to_string(),
        };
        directory.publish_record(&name, &serde_json::to_vec(&record)?)?
    };
    // Never include secret-bearing parser input in diagnostics.
    let record: KeyRecord =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid key record"))?;
    let secret = iroh::SecretKey::from_bytes(&record.secret);
    if record.version != 1
        || record.purpose != purpose
        || secret.public().to_string() != record.public
    {
        bail!("key record purpose/version/public key mismatch");
    }
    Ok(secret)
}

/// The one host key of this machine (per user state directory).
#[derive(Clone)]
pub struct HostKey(iroh::SecretKey);

impl std::fmt::Debug for HostKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostKey({})", self.0.public())
    }
}

impl HostKey {
    pub fn load_or_create(state_dir: &Path) -> Result<Self> {
        let root = storage::connections_dir(state_dir)?;
        let directory = PrivateDirectory::open(&root)?;
        load_or_create(&directory, "host.key", HOST_PURPOSE).map(Self)
    }

    pub const fn secret(&self) -> &iroh::SecretKey {
        &self.0
    }

    pub fn public(&self) -> iroh::PublicKey {
        self.0.public()
    }
}

/// Exclusive right to host on this machine; released on drop.
pub struct HostLock {
    _held: HeldLock,
}

impl HostLock {
    /// Fails when another runtime of this user already hosts.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        let root = storage::connections_dir(state_dir)?;
        let directory = PrivateDirectory::open(&root)?;
        let held = storage::try_lock(&directory, c"host.lock")?.context(
            "another bmux runtime on this machine is already hosting remote access; disable it there first",
        )?;
        Ok(Self { _held: held })
    }
}

/// This client profile's device key.
#[derive(Clone)]
pub struct DeviceKey(iroh::SecretKey);

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.0.public())
    }
}

impl DeviceKey {
    pub fn load_or_create(state_dir: &Path, profile: &str) -> Result<Self> {
        if !storage::valid_component(profile) {
            bail!("invalid client profile name");
        }
        let (_, directory) = storage::private_subdirectory(state_dir, "devices")?;
        load_or_create(&directory, &format!("{profile}.key"), DEVICE_PURPOSE).map(Self)
    }

    pub const fn secret(&self) -> &iroh::SecretKey {
        &self.0
    }

    pub fn public(&self) -> iroh::PublicKey {
        self.0.public()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_separate_and_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let host = HostKey::load_or_create(root.path()).unwrap();
        assert_eq!(
            HostKey::load_or_create(root.path()).unwrap().public(),
            host.public()
        );
        let device = DeviceKey::load_or_create(root.path(), "default").unwrap();
        assert_ne!(device.public(), host.public());
        assert_eq!(
            DeviceKey::load_or_create(root.path(), "default")
                .unwrap()
                .public(),
            device.public()
        );
        assert_ne!(
            DeviceKey::load_or_create(root.path(), "work")
                .unwrap()
                .public(),
            device.public()
        );
        assert!(DeviceKey::load_or_create(root.path(), "../x").is_err());
        assert!(root.path().join("connections/host.key").is_file());
        assert!(
            root.path()
                .join("connections/devices/default.key")
                .is_file()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| {
                std::fs::metadata(root.path().join(p))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("connections"), 0o700);
            assert_eq!(mode("connections/host.key"), 0o600);
        }
        assert!(!format!("{host:?}").contains(&format!("{:?}", host.secret().to_bytes())));
    }

    #[test]
    fn tampered_purpose_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        HostKey::load_or_create(root.path()).unwrap();
        let path = root.path().join("connections/host.key");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace(HOST_PURPOSE, DEVICE_PURPOSE);
        std::fs::write(&path, text).unwrap();
        assert!(HostKey::load_or_create(root.path()).is_err());
    }

    #[test]
    fn only_one_host_lock_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let lock = HostLock::acquire(root.path()).unwrap();
        assert!(HostLock::acquire(root.path()).is_err());
        drop(lock);
        assert!(HostLock::acquire(root.path()).is_ok());
    }
}
