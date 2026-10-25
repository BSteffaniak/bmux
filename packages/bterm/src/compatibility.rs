//! Bundle integrity and IPC compatibility are separate gates; neither uses mtime/size identity.
//! A bundle manifest is an integrity contract, not a signature or a trust anchor.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use bmux_ipc::{NegotiatedProtocol, ProtocolContract};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime_paths::validate_absolute;

/// Explicit frontend/companion bundle contract. Change when the paired contract changes.
pub const COMPATIBILITY_ID: &str = "bterm-bmux-v1";
/// Immutable bundle manifest located beside the native executable and companion.
pub const BUNDLE_MANIFEST: &str = "bterm-bundle.json";

/// Packaging-owned immutable bundle metadata; all artifact paths are bundle-relative.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema_version: u32,
    pub compatibility_id: String,
    pub target: String,
    pub artifacts: Vec<BundleArtifact>,
}

/// One bundled file. Roles `frontend` and `companion` must each occur exactly once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleArtifact {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
}

/// Verified launch input. Construct only through [`Self::load`].
#[derive(Debug, Clone)]
pub struct VerifiedCompanion {
    pub executable: PathBuf,
    pub bundle_id: String,
    pub sha256: String,
    /// Digest of the complete verified manifest (including every artifact identity).
    pub artifact_set_sha256: String,
    /// The frontend contract; the connected server must still negotiate it.
    pub protocol: ProtocolContract,
}

impl VerifiedCompanion {
    /// Verify a companion in its explicitly selected bundle. Never search PATH.
    /// Every listed artifact is hashed, and symlinks escaping the bundle are rejected.
    pub fn load(executable: PathBuf) -> Result<Self> {
        // Linux exposes the image actually executing, including after rename or
        // unlink. Other supported platforms use their executable locator.
        #[cfg(target_os = "linux")]
        let frontend = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let frontend = std::env::current_exe().context("locate running frontend")?;
        Self::load_for_frontend(executable, &frontend)
    }

    /// Prepare a private immutable launch bundle on macOS, without modifying
    /// mutable development outputs. `cache` must already be a secured private
    /// transport/artifact directory. The caller must spawn the returned path.
    /// Other platforms retain their OS-backed executing-image verification.
    pub fn prepare_launch_image(&self, cache: &Path) -> Result<PathBuf> {
        let verified = Self::load(self.executable.clone())?;
        ensure!(
            verified.sha256 == self.sha256
                && verified.artifact_set_sha256 == self.artifact_set_sha256,
            "bundle changed before immutable image preparation"
        );
        #[cfg(target_os = "macos")]
        {
            self.prepare_macos_image(cache)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cache;
            Ok(self.executable.clone())
        }
    }

    #[cfg(target_os = "macos")]
    fn prepare_macos_image(&self, cache: &Path) -> Result<PathBuf> {
        use std::os::darwin::fs::MetadataExt as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        validate_absolute(cache)?;
        let metadata = std::fs::symlink_metadata(cache)?;
        // SAFETY: geteuid only queries the current effective user.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == uid
                && metadata.mode() & 0o077 == 0,
            "immutable image cache must be a private user-owned real directory"
        );
        let cache = cache.canonicalize()?;
        let source_root = self
            .executable
            .parent()
            .context("companion has no bundle root")?;
        let manifest: BundleManifest =
            serde_json::from_reader(File::open(source_root.join(BUNDLE_MANIFEST))?)?;
        manifest.validate_contract()?;
        ensure!(
            hex_digest(&Sha256::digest(serde_json::to_vec(&manifest)?)) == self.artifact_set_sha256,
            "bundle manifest changed before immutable image preparation"
        );
        let destination = cache.join(format!("image-{}", self.artifact_set_sha256));
        let relative = self.executable.strip_prefix(source_root)?;
        let selected = destination.join(relative);
        let verify_image = || -> Result<()> {
            let verified = Self::load(selected.clone())?;
            ensure!(
                verified.sha256 == self.sha256
                    && verified.artifact_set_sha256 == self.artifact_set_sha256,
                "immutable launch bundle identity mismatch"
            );
            let metadata = std::fs::symlink_metadata(&selected)?;
            ensure!(
                metadata.is_file()
                    && metadata.st_flags() & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE) != 0,
                "private launch image is not immutable"
            );
            Ok(())
        };
        if destination.try_exists()? {
            let published = std::fs::symlink_metadata(&destination)?;
            ensure!(
                published.is_dir() && !published.file_type().is_symlink(),
                "immutable image destination is not a real directory"
            );
            verify_image()?;
            return Ok(selected);
        }
        let staging = cache.join(format!(
            "image-pending-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
        for artifact in &manifest.artifacts {
            let source = artifact.verify(source_root)?;
            let path = staging.join(&artifact.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &path)?;
            ensure!(
                file_sha256(&path)? == artifact.sha256,
                "bundle artifact changed while preparing immutable image"
            );
        }
        std::fs::write(
            staging.join(BUNDLE_MANIFEST),
            serde_json::to_vec(&manifest)?,
        )?;
        let image = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(staging.join(relative))?;
        image.sync_all()?;
        // SAFETY: the descriptor refers only to our newly copied private image.
        // Source artifacts and live images are never changed or cleaned up.
        ensure!(
            unsafe { libc::fchflags(image.as_raw_fd(), libc::UF_IMMUTABLE) } == 0,
            "cannot pin private macOS companion image: {}",
            std::io::Error::last_os_error()
        );
        match std::fs::rename(&staging, &destination) {
            Ok(()) => {}
            Err(error) if destination.try_exists()? => {
                // A concurrent preparer won. Preserve our immutable staging
                // evidence rather than deleting files under uncertain ownership.
                let _ = error;
            }
            Err(error) => return Err(error).context("publish immutable companion image"),
        }
        File::open(&cache)?.sync_all()?;
        verify_image()?;
        Ok(selected)
    }

    fn load_for_frontend(executable: PathBuf, running_frontend: &Path) -> Result<Self> {
        validate_absolute(&executable)?;
        let parent = executable
            .parent()
            .context("companion has no bundle directory")?;
        let bundle_root = parent
            .canonicalize()
            .context("resolve companion bundle directory")?;
        let manifest: BundleManifest = serde_json::from_reader(BufReader::new(
            File::open(bundle_root.join(BUNDLE_MANIFEST))
                .context("open companion bundle manifest")?,
        ))
        .context("parse companion bundle manifest")?;
        manifest.validate_contract()?;
        let mut companion = None;
        let mut frontend = None;
        let mut paths = std::collections::BTreeSet::new();
        for artifact in &manifest.artifacts {
            ensure!(
                paths.insert(artifact.path.clone()),
                "duplicate bundle artifact path"
            );
            let path = artifact.verify(&bundle_root)?;
            match artifact.role.as_str() {
                "companion" => {
                    ensure!(companion.is_none(), "duplicate companion artifact");
                    companion = Some((path, artifact.sha256.clone()));
                }
                "frontend" => {
                    ensure!(frontend.is_none(), "duplicate frontend artifact");
                    frontend = Some((path, artifact));
                }
                _ => {}
            }
        }
        let (_, frontend_artifact) = frontend.context("bundle has no frontend artifact")?;
        // An explicitly located bundle must contain this frontend's bytes, not
        // merely an arbitrary executable paired with a matching manifest hash.
        ensure!(
            file_sha256(running_frontend)? == frontend_artifact.sha256,
            "bundle frontend does not match the running frontend executable"
        );
        let (verified_path, sha256) = companion.context("bundle has no companion artifact")?;
        ensure!(
            executable
                .canonicalize()
                .context("resolve companion executable")?
                == verified_path,
            "selected companion is not the manifest companion artifact"
        );
        Ok(Self {
            executable: verified_path,
            artifact_set_sha256: hex_digest(&Sha256::digest(serde_json::to_vec(&manifest)?)),
            bundle_id: manifest.compatibility_id,
            sha256,
            protocol: frontend_contract(),
        })
    }
}

impl BundleManifest {
    /// Validate schema, explicit compatibility identity and native target before reading artifacts.
    pub fn validate_contract(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported bundle schema {}",
            self.schema_version
        );
        ensure!(
            self.compatibility_id == COMPATIBILITY_ID,
            "incompatible companion bundle {}",
            self.compatibility_id
        );
        ensure!(
            self.target == native_target(),
            "bundle target {} does not match {}",
            self.target,
            native_target()
        );
        Ok(())
    }
}

impl BundleArtifact {
    fn verify(&self, root: &Path) -> Result<PathBuf> {
        ensure!(
            !self.path.as_os_str().is_empty()
                && self
                    .path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "artifact path must be a relative normal path: {}",
            self.path.display()
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "artifact SHA-256 must be lowercase hexadecimal"
        );
        let path = root
            .join(&self.path)
            .canonicalize()
            .context("resolve bundle artifact")?;
        ensure!(path.starts_with(root), "artifact escapes bundle root");
        ensure!(path.is_file(), "bundle artifact is not a regular file");
        ensure!(
            file_sha256(&path)? == self.sha256,
            "bundle artifact hash mismatch: {}",
            self.path.display()
        );
        Ok(path)
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path).context("open executable/artifact")?);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = reader
            .read(&mut buffer)
            .context("hash executable/artifact")?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex_digest(&digest.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible.
        write!(&mut hex, "{byte:02x}").expect("String formatting cannot fail");
    }
    hex
}

/// Rust target triples supported by the initial native distribution.
#[must_use]
pub fn native_target() -> String {
    let architecture = std::env::consts::ARCH;
    let suffix = match std::env::consts::OS {
        "linux" => {
            if cfg!(target_env = "musl") {
                "unknown-linux-musl"
            } else {
                "unknown-linux-gnu"
            }
        }
        "macos" => "apple-darwin",
        "windows" => {
            if cfg!(target_env = "gnu") {
                "pc-windows-gnu"
            } else {
                "pc-windows-msvc"
            }
        }
        other => return format!("{architecture}-{other}-unsupported"),
    };
    format!("{architecture}-{suffix}")
}

/// BMUX's existing capability/revision contract, not a second native protocol.
#[must_use]
pub fn frontend_contract() -> ProtocolContract {
    ProtocolContract::current(bmux_ipc::default_supported_capabilities())
}

/// Check the actual server's advertised core contract before granting attach authority.
/// This is not sufficient for an attach path using plugin services: that path
/// must call `BmuxClient::require_service_contracts` and use generated typed endpoints.
/// Rejection never authorizes replacing the server or trying another root.
pub fn verify_server_contract(server: &ProtocolContract) -> Result<NegotiatedProtocol> {
    match bmux_ipc::negotiate_protocol(
        &frontend_contract(),
        server,
        bmux_ipc::CORE_PROTOCOL_CAPABILITIES,
    ) {
        Ok(protocol) => Ok(protocol),
        Err(reason) => bail!("incompatible BMUX server: {reason:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn prepares_private_immutable_image_without_pinning_development_output() {
        use std::os::darwin::fs::MetadataExt as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("development-bundle");
        let cache = root.join("private-images");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&cache).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), source.join("bterm")).unwrap();
        std::fs::write(source.join("bmux"), b"private companion fixture").unwrap();
        let manifest = BundleManifest {
            schema_version: 1,
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            target: native_target(),
            artifacts: ["bterm", "bmux"]
                .into_iter()
                .map(|name| BundleArtifact {
                    role: if name == "bterm" {
                        "frontend"
                    } else {
                        "companion"
                    }
                    .to_owned(),
                    path: name.into(),
                    sha256: file_sha256(&source.join(name)).unwrap(),
                })
                .collect(),
        };
        std::fs::write(
            source.join(BUNDLE_MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let companion = VerifiedCompanion::load(source.join("bmux")).unwrap();
        let image = companion.prepare_launch_image(&cache).unwrap();
        assert_ne!(image, companion.executable);
        assert_eq!(file_sha256(&image).unwrap(), companion.sha256);
        assert_ne!(
            std::fs::metadata(&image).unwrap().st_flags() & libc::UF_IMMUTABLE,
            0
        );
        assert_eq!(
            std::fs::metadata(&companion.executable).unwrap().st_flags() & libc::UF_IMMUTABLE,
            0
        );
        assert_eq!(companion.prepare_launch_image(&cache).unwrap(), image);
        std::fs::write(
            &companion.executable,
            b"development rebuild remains writable",
        )
        .unwrap();
        assert_eq!(file_sha256(&image).unwrap(), companion.sha256);
        assert!(companion.prepare_launch_image(&cache).is_err());
        // Only the fixture-owned immutable image is unpinned for TempDir cleanup.
        let file = File::open(&image).unwrap();
        // SAFETY: this descriptor is the private image created by this test.
        assert_eq!(unsafe { libc::fchflags(file.as_raw_fd(), 0) }, 0);
    }

    #[test]
    fn rejects_unknown_bundle_contracts() {
        let mut manifest = BundleManifest {
            schema_version: 1,
            compatibility_id: COMPATIBILITY_ID.into(),
            target: native_target(),
            artifacts: vec![],
        };
        assert!(manifest.validate_contract().is_ok());
        manifest.schema_version = 2;
        assert!(manifest.validate_contract().is_err());
        manifest.schema_version = 1;
        manifest.compatibility_id = "another-bundle".into();
        assert!(manifest.validate_contract().is_err());
        manifest.compatibility_id = COMPATIBILITY_ID.into();
        manifest.target = "wrong-target".into();
        assert!(manifest.validate_contract().is_err());
    }

    #[test]
    fn rejects_incompatible_actual_server() {
        let mut server = frontend_contract();
        assert!(verify_server_contract(&server).is_ok());
        server.wire_epoch += 1;
        assert!(verify_server_contract(&server).is_err());
        server = frontend_contract();
        server.capabilities.clear();
        assert!(verify_server_contract(&server).is_err());
    }

    #[test]
    fn verifies_bundle_then_rejects_tampered_artifact() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("bterm-bundle-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let frontend = b"native frontend";
        let companion = b"bundled companion";
        std::fs::write(root.join("bterm"), frontend).unwrap();
        std::fs::write(root.join("bmux"), companion).unwrap();
        let manifest = BundleManifest {
            schema_version: 1,
            compatibility_id: COMPATIBILITY_ID.into(),
            target: native_target(),
            artifacts: vec![
                BundleArtifact {
                    role: "frontend".into(),
                    path: "bterm".into(),
                    sha256: hex_digest(&Sha256::digest(frontend)),
                },
                BundleArtifact {
                    role: "companion".into(),
                    path: "bmux".into(),
                    sha256: hex_digest(&Sha256::digest(companion)),
                },
            ],
        };
        std::fs::write(
            root.join(BUNDLE_MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let other_frontend = root.join("unrelated-bterm");
        std::fs::write(&other_frontend, b"unrelated frontend bytes").unwrap();
        assert!(VerifiedCompanion::load_for_frontend(root.join("bmux"), &other_frontend).is_err());
        let verified =
            VerifiedCompanion::load_for_frontend(root.join("bmux"), &root.join("bterm")).unwrap();
        assert_eq!(
            verified.executable,
            root.join("bmux").canonicalize().unwrap()
        );
        // An additional verified plugin changes the artifact set even though
        // the companion bytes and compatibility label remain identical.
        let mut with_plugin = manifest.clone();
        std::fs::write(root.join("plugin.so"), b"plugin bytes").unwrap();
        with_plugin.artifacts.push(BundleArtifact {
            role: "plugin".into(),
            path: "plugin.so".into(),
            sha256: hex_digest(&Sha256::digest(b"plugin bytes")),
        });
        std::fs::write(
            root.join(BUNDLE_MANIFEST),
            serde_json::to_vec(&with_plugin).unwrap(),
        )
        .unwrap();
        let plugin_verified =
            VerifiedCompanion::load_for_frontend(root.join("bmux"), &root.join("bterm")).unwrap();
        assert_eq!(verified.sha256, plugin_verified.sha256);
        assert_eq!(verified.bundle_id, plugin_verified.bundle_id);
        assert_ne!(
            verified.artifact_set_sha256,
            plugin_verified.artifact_set_sha256
        );
        // Same length, different bytes: executable metadata is not identity.
        std::fs::write(root.join("bmux"), b"replaced companion").unwrap();
        assert!(
            VerifiedCompanion::load_for_frontend(root.join("bmux"), &root.join("bterm")).is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_artifact_traversal_before_opening() {
        let artifact = BundleArtifact {
            role: "companion".into(),
            path: "../bmux".into(),
            sha256: "0".repeat(64),
        };
        assert!(artifact.verify(&std::env::temp_dir()).is_err());
    }
}
