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
    /// Compatibility entry point. Runtime is only a legacy locator, never identity scope.
    pub fn load_or_create(state_dir: &Path, _runtime_dir: &Path) -> Result<Self> {
        Self::load_or_create_local(state_dir, "default")
    }

    /// Local-process identity: requires no daemon or live runtime directory.
    /// Only the default profile imports legacy identities, and only if all agree.
    pub fn load_or_create_local(state_dir: &Path, profile: &str) -> Result<Self> {
        let store = crate::storage::PairingStore::client(state_dir, profile, c"identity.json")?;
        store.transaction(|bytes| {
            let mut legacy_secret = None;
            if profile == "default" {
                for entry in std::fs::read_dir(state_dir)? {
                    let entry = entry?;
                    if !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("connections-device-")
                    {
                        continue;
                    }
                    let raw = PrivateDirectory::open(&entry.path())?
                        .read()?
                        .context("legacy device identity missing")?;
                    let record: DeviceRecord = serde_json::from_slice(&raw)
                        .map_err(|_| anyhow::anyhow!("invalid legacy device identity record"))?;
                    let secret = iroh::SecretKey::from_bytes(&record.secret);
                    if record.version != 1
                        || record.purpose != "bmux.connecting-device.v1"
                        || secret.public().to_string() != record.device_key
                        || !Path::new(&record.runtime).is_absolute()
                    {
                        bail!("invalid legacy device identity metadata");
                    }
                    let namespace = Sha256::digest(
                        format!("bmux.device-identity.v1\0{}", record.runtime).as_bytes(),
                    )
                    .iter()
                    .fold(String::new(), |mut out, b| {
                        use std::fmt::Write;
                        write!(out, "{b:02x}").expect("write to String");
                        out
                    });
                    if entry.file_name()
                        != std::ffi::OsString::from(format!("connections-device-{namespace}"))
                    {
                        bail!("legacy device identity scope mismatch");
                    }
                    if legacy_secret.is_some_and(|value| value != record.secret) {
                        bail!("conflicting legacy device identities; explicit migration required");
                    }
                    legacy_secret = Some(record.secret);
                }
            }
            let record = if let Some(raw) = bytes.as_deref() {
                serde_json::from_slice::<DeviceRecord>(raw)
                    .map_err(|_| anyhow::anyhow!("invalid device identity record"))?
            } else {
                let secret = legacy_secret.map_or_else(iroh::SecretKey::generate, |key| {
                    iroh::SecretKey::from_bytes(&key)
                });
                DeviceRecord {
                    version: 2,
                    purpose: "bmux.connecting-device.v2".into(),
                    runtime: profile.into(),
                    device_key: secret.public().to_string(),
                    secret: secret.to_bytes(),
                }
            };
            let secret = iroh::SecretKey::from_bytes(&record.secret);
            if record.version != 2
                || record.purpose != "bmux.connecting-device.v2"
                || record.runtime != profile
                || secret.public().to_string() != record.device_key
            {
                bail!("device identity purpose/version/profile/key mismatch");
            }
            if legacy_secret.is_some_and(|value| value != record.secret) {
                bail!("durable and legacy device identity conflict");
            }
            *bytes = Some(serde_json::to_vec(&record)?);
            Ok(Self { secret })
        })
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
    use std::fmt::Write;
    #[cfg(unix)]
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
    fn local_identity_is_runtime_independent_and_profile_isolated() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("new-state");
        let first = DeviceIdentity::load_or_create_local(&state, "default")
            .unwrap()
            .public_key();
        assert_eq!(
            first,
            DeviceIdentity::load_or_create(&state, &root.path().join("missing-runtime"))
                .unwrap()
                .public_key()
        );
        assert_ne!(
            first,
            DeviceIdentity::load_or_create_local(&state, "work")
                .unwrap()
                .public_key()
        );
        assert_ne!(
            first,
            DeviceIdentity::load_or_create_local(&root.path().join("other-state"), "default")
                .unwrap()
                .public_key()
        );
        assert!(DeviceIdentity::load_or_create_local(&state, "../escape").is_err());
        std::thread::scope(|scope| {
            let handles: [_; 8] = std::array::from_fn(|_| {
                scope.spawn(|| {
                    DeviceIdentity::load_or_create_local(&state, "concurrent")
                        .unwrap()
                        .public_key()
                })
            });
            let mut keys = handles.into_iter().map(|handle| handle.join().unwrap());
            let first = keys.next().unwrap();
            assert!(keys.all(|key| key == first));
        });
    }

    fn legacy_device(state: &Path, runtime: &str, secret: &iroh::SecretKey) {
        let digest = Sha256::digest(format!("bmux.device-identity.v1\0{runtime}").as_bytes())
            .iter()
            .fold(String::new(), |mut output, byte| {
                write!(&mut output, "{byte:02x}").unwrap();
                output
            });
        let directory =
            PrivateDirectory::open(&state.join(format!("connections-device-{digest}"))).unwrap();
        directory
            .publish(
                &serde_json::to_vec(&DeviceRecord {
                    version: 1,
                    purpose: "bmux.connecting-device.v1".into(),
                    runtime: runtime.into(),
                    secret: secret.to_bytes(),
                    device_key: secret.public().to_string(),
                })
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn legacy_identity_migration_preserves_key_and_conflicts_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();
        let secret = iroh::SecretKey::generate();
        legacy_device(state, "/expired-runtime", &secret);
        assert_eq!(
            DeviceIdentity::load_or_create_local(state, "default")
                .unwrap()
                .public_key(),
            secret.public().to_string()
        );
        legacy_device(state, "/other-runtime", &iroh::SecretKey::generate());
        let path = crate::storage::client_directory(state, "default")
            .unwrap()
            .join("identity.json");
        let before = std::fs::read(&path).unwrap();
        assert!(DeviceIdentity::load_or_create_local(state, "default").is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(DeviceIdentity::load_or_create_local(state, "explicit-profile").is_ok());
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
                    .starts_with("connections-client-")
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
        #[cfg(unix)]
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

    #[cfg(unix)]
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
