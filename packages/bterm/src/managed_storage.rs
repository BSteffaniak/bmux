//! Managed routing must preserve the selected durable installation.
//!
//! Domain-owned transactions authorize shared plugin writes. Execution receipts
//! remain under the stable state root and are routed by their owning target;
//! unrelated retained executions never prevent admitting independent terminals.
use anyhow::{Result, ensure};

use crate::runtime_paths::{RuntimeRoots, validate_absolute};

/// Read-only prelaunch guard, not a writer lease or readiness proof.
/// External and borrowed sandbox selections do not pass through this gate.
pub(crate) fn validate_managed_storage_roots(
    durable_roots: &RuntimeRoots,
    routed_roots: &RuntimeRoots,
) -> Result<()> {
    for roots in [durable_roots, routed_roots] {
        for path in [
            &roots.config_dir,
            &roots.runtime_dir,
            &roots.data_dir,
            &roots.state_dir,
        ] {
            validate_absolute(path)?;
        }
    }
    ensure!(
        routed_roots.data_dir == durable_roots.data_dir
            && routed_roots.state_dir == durable_roots.state_dir,
        "managed routing changed the selected durable installation; data and state roots must remain stable across artifact and transport scopes",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_preserves_installation_without_hiding_old_executions() {
        let temp = tempfile::tempdir().unwrap();
        let durable = RuntimeRoots::from_root(temp.path().join("selected")).unwrap();
        let mut routed = durable.clone();
        routed.config_dir = temp.path().join("target-config");
        routed.runtime_dir = temp.path().join("target-runtime");
        let retained = durable.state_dir.join("pane-executions-v4/old-owner");
        std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
        std::fs::write(&retained, b"retained authority").unwrap();
        validate_managed_storage_roots(&durable, &routed).unwrap();
        assert_eq!(std::fs::read(&retained).unwrap(), b"retained authority");
        assert!(!routed.runtime_dir.exists());
        assert!(!routed.config_dir.exists());
        routed.data_dir = temp.path().join("empty-build-data");
        assert!(validate_managed_storage_roots(&durable, &routed).is_err());
        routed.data_dir.clone_from(&durable.data_dir);
        routed.state_dir = temp.path().join("empty-build-state");
        assert!(validate_managed_storage_roots(&durable, &routed).is_err());
        assert!(!routed.state_dir.exists());
    }
}
