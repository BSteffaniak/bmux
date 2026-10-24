//! Persistent per-runtime transport identity, distinct from principal/cluster identity.
use crate::storage::{PrivateDirectory, runtime_directory};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct RuntimeIdentity {
    secret: iroh::SecretKey,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    runtime: String,
    secret: [u8; 32],
    endpoint_id: String,
}

impl RuntimeIdentity {
    pub(crate) fn load_or_create(state_dir: &Path, runtime_dir: &Path) -> Result<Self> {
        let directory = PrivateDirectory::open(&runtime_directory(state_dir, runtime_dir)?)?;
        let runtime = runtime_dir
            .canonicalize()
            .context("resolve host runtime directory")?;
        let runtime = runtime.to_str().context("runtime path must be UTF-8")?;
        let bytes = if let Some(bytes) = directory.read()? {
            bytes
        } else {
            let secret = iroh::SecretKey::generate();
            let record = Record {
                version: 1,
                runtime: runtime.to_owned(),
                secret: secret.to_bytes(),
                endpoint_id: secret.public().to_string(),
            };
            directory.publish(&serde_json::to_vec(&record)?)?
        };
        // Do not include parser input or secret-bearing records in diagnostics.
        let record: Record = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid persistent endpoint identity record"))?;
        if record.version != 1 {
            bail!("unsupported endpoint identity version");
        }
        if record.runtime != runtime {
            bail!("endpoint identity runtime mismatch");
        }
        let secret = iroh::SecretKey::from_bytes(&record.secret);
        if secret.public().to_string() != record.endpoint_id {
            bail!("endpoint identity public/private key mismatch");
        }
        Ok(Self { secret })
    }

    pub(crate) fn endpoint_builder(&self) -> iroh::endpoint::Builder {
        iroh::Endpoint::builder(iroh::endpoint::presets::N0).secret_key(self.secret.clone())
    }

    pub(crate) fn endpoint_id(&self) -> String {
        self.secret.public().to_string()
    }
}

/// Purpose-separated persistent connecting-device key. Never loads host identity.
pub struct DeviceIdentity {
    secret: iroh::SecretKey,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeviceRecord {
    version: u32,
    purpose: String,
    runtime: String,
    secret: [u8; 32],
    device_key: String,
}

impl DeviceIdentity {
    pub fn load_or_create(state_dir: &Path, runtime_dir: &Path) -> Result<Self> {
        // Reuse strict absolute/canonical root validation, not the host key path.
        let host_directory = runtime_directory(state_dir, runtime_dir)?;
        let runtime = runtime_dir
            .canonicalize()
            .context("resolve client runtime directory")?;
        let runtime = runtime.to_str().context("client runtime must be UTF-8")?;
        let namespace = Sha256::digest(format!("bmux.device-identity.v1\0{runtime}").as_bytes())
            .iter()
            .fold(String::with_capacity(64), |mut output, byte| {
                use std::fmt::Write;
                write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
                output
            });
        let directory = PrivateDirectory::open(
            &host_directory
                .parent()
                .context("client identity needs state parent")?
                .join(format!("connections-device-{namespace}")),
        )?;
        let bytes = if let Some(bytes) = directory.read()? {
            bytes
        } else {
            let secret = iroh::SecretKey::generate();
            directory.publish(&serde_json::to_vec(&DeviceRecord {
                version: 1,
                purpose: "bmux.connecting-device.v1".into(),
                runtime: runtime.into(),
                device_key: secret.public().to_string(),
                secret: secret.to_bytes(),
            })?)?
        };
        let record: DeviceRecord = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid device identity record"))?;
        if record.version != 1
            || record.purpose != "bmux.connecting-device.v1"
            || record.runtime != runtime
        {
            bail!("device identity purpose/version/runtime mismatch");
        }
        let secret = iroh::SecretKey::from_bytes(&record.secret);
        if secret.public().to_string() != record.device_key {
            bail!("device identity key mismatch");
        }
        Ok(Self { secret })
    }

    pub fn public_key(&self) -> String {
        self.secret.public().to_string()
    }

    pub fn sign(&self, message: &[u8]) -> iroh::Signature {
        self.secret.sign(message)
    }

    pub fn endpoint_builder(&self) -> iroh::endpoint::Builder {
        iroh::Endpoint::builder(iroh::endpoint::presets::N0).secret_key(self.secret.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn roots(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let state = root.join("state");
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        (state, runtime)
    }

    fn record_path(state: &Path) -> std::path::PathBuf {
        std::fs::read_dir(state)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("identity.json")
    }

    #[test]
    fn device_identity_is_persistent_purpose_separated_and_signs() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let host = RuntimeIdentity::load_or_create(&state, &runtime).unwrap();
        let device = DeviceIdentity::load_or_create(&state, &runtime).unwrap();
        assert_ne!(host.endpoint_id(), device.public_key());
        let restarted = DeviceIdentity::load_or_create(&state, &runtime).unwrap();
        assert_eq!(device.public_key(), restarted.public_key());
        let message = b"bmux.device-proof.v1";
        let signature = restarted.sign(message);
        let key: iroh::PublicKey = device.public_key().parse().unwrap();
        key.verify(message, &signature).unwrap();
        assert!(key.verify(b"different", &signature).is_err());
        assert!(host.secret.public().verify(message, &signature).is_err());
        let path = std::fs::read_dir(&state)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("connections-device-")
            })
            .unwrap()
            .join("identity.json");
        let original = std::fs::read(&path).unwrap();
        let mut record: DeviceRecord = serde_json::from_slice(&original).unwrap();
        record.purpose = "bmux.host.v1".into();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(DeviceIdentity::load_or_create(&state, &runtime).is_err());
        // Host identity is untouched by corrupt client-purpose metadata.
        assert_eq!(
            RuntimeIdentity::load_or_create(&state, &runtime)
                .unwrap()
                .endpoint_id(),
            host.endpoint_id()
        );
    }

    #[test]
    fn restart_and_concurrent_initialization_preserve_identity() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let ids = std::thread::scope(|scope| {
            let handles: [_; 12] = std::array::from_fn(|_| {
                scope.spawn(|| {
                    RuntimeIdentity::load_or_create(&state, &runtime)
                        .unwrap()
                        .endpoint_id()
                })
            });
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(ids.iter().all(|id| id == &ids[0]));
        assert_eq!(
            RuntimeIdentity::load_or_create(&state, &runtime)
                .unwrap()
                .endpoint_id(),
            ids[0]
        );
        assert_eq!(
            std::fs::metadata(record_path(&state))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn runtimes_and_state_roots_are_isolated() {
        let root = tempfile::tempdir().unwrap();
        let (state, runtime) = roots(root.path());
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let first = RuntimeIdentity::load_or_create(&state, &runtime)
            .unwrap()
            .endpoint_id();
        assert_ne!(
            first,
            RuntimeIdentity::load_or_create(&state, &other)
                .unwrap()
                .endpoint_id()
        );
        assert_ne!(
            first,
            RuntimeIdentity::load_or_create(&other, &runtime)
                .unwrap()
                .endpoint_id()
        );
    }

    #[test]
    fn corrupt_unknown_inconsistent_and_insecure_records_are_not_rotated() {
        for mutation in [
            "corrupt",
            "version",
            "key",
            "runtime",
            "permissions",
            "symlink",
        ] {
            let root = tempfile::tempdir().unwrap();
            let (state, runtime) = roots(root.path());
            RuntimeIdentity::load_or_create(&state, &runtime).unwrap();
            let path = record_path(&state);
            let mut record: Record =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            match mutation {
                "corrupt" => std::fs::write(&path, b"not-json").unwrap(),
                "version" => {
                    record.version = 2;
                    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                }
                "key" => {
                    record.secret = [9; 32];
                    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                }
                "runtime" => {
                    record.runtime = "/other".into();
                    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
                }
                "permissions" => {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                "symlink" => {
                    std::fs::rename(&path, path.with_extension("original")).unwrap();
                    symlink(path.with_extension("original"), &path).unwrap();
                }
                _ => unreachable!(),
            }
            let before = std::fs::read(&path).unwrap();
            assert!(
                RuntimeIdentity::load_or_create(&state, &runtime).is_err(),
                "accepted {mutation}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }
}
