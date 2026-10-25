//! Exact managed target routing. Selection never starts, stops, or repairs a runtime.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::compatibility::VerifiedCompanion;
use crate::configuration::ResolvedConfiguration;
use crate::runtime_paths::RuntimeRoots;

/// Complete expected identity, suitable for comparison with an authenticated live
/// identity. A filesystem record alone is never readiness or ownership proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedTargetIdentity {
    pub schema_version: u32,
    pub compatibility_id: String,
    pub artifact_set_sha256: String,
    pub companion_sha256: String,
    pub configuration_id: String,
    pub scope_id: String,
    /// Explicit installation roots captured BEFORE target-owned path derivation.
    /// Data remains here across artifact changes. Storage admission is separate.
    pub durable_roots: RuntimeRoots,
}

impl ManagedTargetIdentity {
    /// Full digest, never a display/build label or an executable path hash.
    pub fn target_id(&self) -> Result<String> {
        digest(b"bterm-managed-target-v1", &serde_json::to_vec(self)?)
    }

    /// Exact comparison is mandatory even when socket/registry names match.
    /// The caller must obtain `observed` from the live identity handshake.
    pub fn verify_identity(&self, observed: &Self) -> Result<()> {
        ensure!(
            self == observed,
            "managed companion target identity mismatch"
        );
        Ok(())
    }
}

/// There is deliberately no fallback from an external runtime to a managed one.
#[derive(Debug, Clone)]
pub enum RuntimeSelection {
    Managed {
        roots: RuntimeRoots,
        companion: VerifiedCompanion,
        identity: Box<ManagedTargetIdentity>,
    },
    /// Attach only. Failure never grants launch/replace authority.
    External { roots: RuntimeRoots },
}

impl RuntimeSelection {
    /// Legacy caller with no captured configuration. Uses the same deterministic
    /// target machinery with a distinct unconfigured identity; never claims it
    /// is equivalent to a configured launch.
    pub fn resolve(
        runtime_root: Option<PathBuf>,
        companion: PathBuf,
        managed_root: PathBuf,
    ) -> Result<Self> {
        if let Some(root) = runtime_root {
            return Ok(Self::External {
                roots: RuntimeRoots::from_root(root)?,
            });
        }
        Self::resolve_captured(None, companion, RuntimeRoots::from_root(managed_root)?)
    }

    pub fn resolve_captured(
        runtime_root: Option<PathBuf>,
        companion: PathBuf,
        managed_roots: RuntimeRoots,
    ) -> Result<Self> {
        if let Some(root) = runtime_root {
            return Ok(Self::External {
                roots: RuntimeRoots::from_root(root)?,
            });
        }
        Self::managed(
            companion,
            managed_roots,
            "unconfigured-v1".to_owned(),
            b"unconfigured-v1",
        )
    }

    /// Shared by native launch and config inspection. Uses only captured inputs;
    /// no environment discovery, directory creation, or companion startup occurs.
    /// External roots bypass bundle inspection and retain their exact layout.
    pub fn resolve_configured(
        runtime_root: Option<PathBuf>,
        companion: PathBuf,
        managed_roots: RuntimeRoots,
        configuration: &ResolvedConfiguration,
    ) -> Result<Self> {
        if let Some(root) = runtime_root {
            return Ok(Self::External {
                roots: RuntimeRoots::from_root(root)?,
            });
        }
        let scope = configuration.inputs.options.scope.as_ref();
        let scope_bytes = serde_json::to_vec(&(
            configuration.inputs.options.forced_profile.as_ref(),
            scope.map(|scope| (&scope.name, &scope.attributes)),
            configuration
                .sources
                .iter()
                .map(|source| (&source.family, &source.path))
                .collect::<Vec<_>>(),
        ))?;
        Self::managed(
            companion,
            managed_roots,
            configuration.runtime_configuration_id.clone(),
            &scope_bytes,
        )
    }

    fn managed(
        executable: PathBuf,
        roots: RuntimeRoots,
        configuration_id: String,
        scope: &[u8],
    ) -> Result<Self> {
        let companion = VerifiedCompanion::load(executable)?;
        let durable_roots = roots.normalized()?;
        let scope_id = digest(
            b"bterm-runtime-scope-v1",
            &serde_json::to_vec(&(&durable_roots, scope))?,
        )?;
        let identity = ManagedTargetIdentity {
            schema_version: 1,
            compatibility_id: companion.bundle_id.clone(),
            artifact_set_sha256: companion.artifact_set_sha256.clone(),
            companion_sha256: companion.sha256.clone(),
            configuration_id,
            scope_id,
            durable_roots,
        };
        let roots = identity.durable_roots.for_target(&identity.target_id()?)?;
        Ok(Self::Managed {
            roots,
            companion,
            identity: Box::new(identity),
        })
    }

    #[must_use]
    pub fn managed_identity(&self) -> Option<&ManagedTargetIdentity> {
        match self {
            Self::Managed { identity, .. } => Some(identity.as_ref()),
            Self::External { .. } => None,
        }
    }

    #[must_use]
    pub const fn roots(&self) -> &RuntimeRoots {
        match self {
            Self::Managed { roots, .. } | Self::External { roots } => roots,
        }
    }
}

fn digest(domain: &[u8], bytes: &[u8]) -> Result<String> {
    use std::fmt::Write as _;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update([0]);
    digest.update(bytes);
    let mut result = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(result, "{byte:02x}")?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ManagedTargetIdentity {
        ManagedTargetIdentity {
            schema_version: 1,
            compatibility_id: "bterm-bmux-v1".to_owned(),
            artifact_set_sha256: "a".repeat(64),
            companion_sha256: "b".repeat(64),
            configuration_id: "c".repeat(64),
            scope_id: "d".repeat(64),
            durable_roots: RuntimeRoots::from_root(PathBuf::from("/explicit-installation"))
                .unwrap(),
        }
    }

    #[test]
    fn external_never_loads_or_falls_back_to_companion() {
        let root = std::env::temp_dir().join("bterm-external-unit");
        let selection = RuntimeSelection::resolve(
            Some(root.clone()),
            PathBuf::from("invalid-companion"),
            PathBuf::from("invalid-managed-root"),
        )
        .unwrap();
        assert!(selection.managed_identity().is_none());
        assert_eq!(selection.roots().runtime_dir, root.join("runtime"));
    }

    #[test]
    fn invalid_external_root_is_error_not_managed_fallback() {
        assert!(
            RuntimeSelection::resolve(
                Some(PathBuf::from("relative")),
                PathBuf::from("companion"),
                std::env::temp_dir()
            )
            .is_err()
        );
    }

    #[test]
    fn every_identity_dimension_is_verified_not_just_a_short_socket_name() {
        let expected = identity();
        let baseline = expected.target_id().unwrap();
        assert_eq!(baseline, expected.target_id().unwrap());
        assert_eq!(baseline.len(), 64);
        for field in 0..6 {
            let mut changed = expected.clone();
            match field {
                0 => changed.compatibility_id.push('x'),
                1 => changed.artifact_set_sha256 = "e".repeat(64),
                2 => changed.companion_sha256 = "e".repeat(64),
                3 => changed.configuration_id = "e".repeat(64),
                4 => changed.scope_id = "e".repeat(64),
                _ => changed.durable_roots.data_dir.push("other"),
            }
            assert_ne!(baseline, changed.target_id().unwrap());
            assert!(expected.verify_identity(&changed).is_err());
        }
        expected.verify_identity(&expected).unwrap();
    }

    #[test]
    fn artifacts_coexist_without_selecting_empty_installation() {
        let old = identity();
        let mut new = old.clone();
        new.companion_sha256 = "e".repeat(64);
        let first = old
            .durable_roots
            .for_target(&old.target_id().unwrap())
            .unwrap();
        let second = new
            .durable_roots
            .for_target(&new.target_id().unwrap())
            .unwrap();
        assert_ne!(first.runtime_dir, second.runtime_dir);
        assert_eq!(first.state_dir, second.state_dir);
        assert_eq!(first.state_dir, old.durable_roots.state_dir);
        assert_ne!(first.config_dir, second.config_dir);
        assert_eq!(first.data_dir, second.data_dir);
        assert_eq!(first.data_dir, old.durable_roots.data_dir);
    }
}
