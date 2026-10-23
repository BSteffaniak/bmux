//! Explicit attach-only versus managed runtime intent. Selection never starts or stops a process.
//! Managed lifetimes belong to the companion, not to any individual native window.

use std::path::PathBuf;

use anyhow::Result;

use crate::compatibility::VerifiedCompanion;
use crate::runtime_paths::RuntimeRoots;

/// There is deliberately no fallback from an external runtime to a managed one.
#[derive(Debug, Clone)]
pub enum RuntimeSelection {
    /// Launch is permitted only through the verified companion and these exact roots.
    Managed {
        roots: RuntimeRoots,
        companion: VerifiedCompanion,
    },
    /// Attach only. Failure or incompatibility never grants launch/replace authority.
    External { roots: RuntimeRoots },
}

impl RuntimeSelection {
    /// Resolve captured arguments. An external root never even opens the companion.
    /// CLI callers must separately reject an explicitly supplied `--companion` together
    /// with `--runtime-root`; `companion` here may already be the bundle default.
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
        Ok(Self::Managed {
            roots: RuntimeRoots::managed(managed_root)?,
            companion: VerifiedCompanion::load(companion)?,
        })
    }

    /// Select already-captured managed paths (including sandbox contexts).
    /// External attachment keeps its exact layout and never opens a companion.
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
        for path in [
            &managed_roots.config_dir,
            &managed_roots.runtime_dir,
            &managed_roots.data_dir,
            &managed_roots.state_dir,
        ] {
            crate::runtime_paths::validate_absolute(path)?;
        }
        Ok(Self::Managed {
            roots: managed_roots,
            companion: VerifiedCompanion::load(companion)?,
        })
    }

    /// Exact resolved paths shared by connection and launch code.
    #[must_use]
    pub const fn roots(&self) -> &RuntimeRoots {
        match self {
            Self::Managed { roots, .. } | Self::External { roots } => roots,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_never_loads_or_falls_back_to_companion() {
        let root = std::env::temp_dir().join("bterm-external-unit");
        let selection = RuntimeSelection::resolve(
            Some(root.clone()),
            PathBuf::from("invalid-companion"),
            PathBuf::from("invalid-managed-root"),
        )
        .unwrap();
        assert!(matches!(selection, RuntimeSelection::External { .. }));
        assert_eq!(selection.roots().runtime_dir, root.join("runtime"));
    }

    #[test]
    fn invalid_external_root_is_error_not_managed_fallback() {
        assert!(
            RuntimeSelection::resolve(
                Some(PathBuf::from("relative")),
                PathBuf::from("companion"),
                std::env::temp_dir(),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_requires_real_verified_bundle() {
        assert!(
            RuntimeSelection::resolve(
                None,
                std::env::temp_dir().join("absent-bterm-bundle/bmux"),
                std::env::temp_dir().join("bterm-managed-unit"),
            )
            .is_err()
        );
    }
}
