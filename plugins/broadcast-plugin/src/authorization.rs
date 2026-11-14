//! Private local capability authorization; UUID presence is not transport trust.
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const PROOF_FILE: &str = "broadcast-local-proof";
struct Grants {
    proof: Vec<u8>,
    path: PathBuf,
    callers: BTreeMap<Uuid, Instant>,
}
#[derive(Default)]
pub struct Authorization {
    grants: Mutex<Option<Grants>>,
}
static GRANTS: OnceLock<Authorization> = OnceLock::new();
fn legacy() -> &'static Authorization {
    GRANTS.get_or_init(Authorization::default)
}
pub fn initialize(runtime: &Path) -> Result<(), &'static str> {
    legacy().initialize(runtime)
}
pub fn authorize(caller: Uuid, proof: &[u8]) -> Result<bool, &'static str> {
    legacy().authorize(caller, proof)
}
pub fn check(caller: Uuid) -> Result<Uuid, &'static str> {
    legacy().check(caller)
}
pub fn revoke() {
    let _ = legacy().revoke();
}
impl Authorization {
    pub fn initialize(&self, runtime: &Path) -> Result<(), &'static str> {
        let mut all = self
            .grants
            .lock()
            .map_err(|_| "local_authorization_unavailable")?;
        if let Some(grants) = all.as_ref() {
            return if grants.path == runtime.join(PROOF_FILE) {
                Ok(())
            } else {
                Err("local_authorization_scope_mismatch")
            };
        }
        let mut proof = Uuid::new_v4().as_bytes().to_vec();
        proof.extend_from_slice(Uuid::new_v4().as_bytes());
        let path = runtime.join(PROOF_FILE);
        let temporary = runtime.join(format!("broadcast-proof-{}", Uuid::new_v4()));
        let mut file = crate::platform::create_new(&temporary)
            .map_err(|_| "local_authorization_unavailable")?;
        file.write_all(&proof)
            .map_err(|_| "local_authorization_unavailable")?;
        file.sync_all()
            .map_err(|_| "local_authorization_unavailable")?;
        drop(file);
        // Atomic no-replace publication: a second live instance must not rotate
        // another authority's proof or inherit its grants.
        if crate::platform::publish_new(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
            return Err("local_authorization_scope_owned");
        }
        // Unix publishes by hard link (temporary remains); Windows moves it.
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("local_authorization_cleanup_required"),
        }
        // Persist the temporary's unlink. Windows publication is write-through
        // and has no directory flush, so only Unix needs this step.
        #[cfg(unix)]
        crate::platform::sync_directory(runtime).map_err(|_| "local_authorization_unavailable")?;
        *all = Some(Grants {
            proof,
            path,
            callers: BTreeMap::new(),
        });
        Ok(())
    }

    pub fn authorize(&self, caller: Uuid, proof: &[u8]) -> Result<bool, &'static str> {
        let mut all = self
            .grants
            .lock()
            .map_err(|_| "local_authorization_unavailable")?;
        let grants = all.as_mut().ok_or("local_authorization_unavailable")?;
        if proof.len() != 32
            || proof
                .iter()
                .zip(&grants.proof)
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                != 0
        {
            return Err("local_authorization_required");
        }
        grants
            .callers
            .retain(|_, expires| *expires > Instant::now());
        if !grants.callers.contains_key(&caller) && grants.callers.len() >= 128 {
            return Err("local_authorization_bound");
        }
        grants
            .callers
            .insert(caller, Instant::now() + Duration::from_secs(300));
        Ok(true)
    }

    pub fn check(&self, caller: Uuid) -> Result<Uuid, &'static str> {
        let mut all = self
            .grants
            .lock()
            .map_err(|_| "local_authorization_unavailable")?;
        let grants = all.as_mut().ok_or("local_authorization_required")?;
        let expires = grants
            .callers
            .get_mut(&caller)
            .filter(|expires| **expires > Instant::now())
            .ok_or("local_authorization_required")?;
        *expires = Instant::now() + Duration::from_secs(300);
        Ok(caller)
    }

    pub fn revoke(&self) -> Result<(), &'static str> {
        let mut all = self
            .grants
            .lock()
            .map_err(|_| "local_authorization_unavailable")?;
        if let Some(grants) = all.as_mut() {
            grants.callers.clear();
            // Do not remove a replacement authority's proof file.
            match local_proof(
                grants
                    .path
                    .parent()
                    .ok_or("local_authorization_unavailable")?,
            ) {
                Ok(proof) if proof == grants.proof => std::fs::remove_file(&grants.path)
                    .map_err(|_| "local_authorization_cleanup_required")?,
                Ok(_) => return Err("local_authorization_superseded"),
                Err(_) if !grants.path.exists() => {}
                Err(_) => return Err("local_authorization_cleanup_required"),
            }
        }
        *all = None;
        Ok(())
    }
}

/// Caller reads only its selected runtime's private proof. No service reads a
/// caller-chosen proof path, and tokens never enter diagnostic strings.
pub fn local_proof(runtime: &Path) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    let mut file = crate::platform::open_private(&runtime.join(PROOF_FILE))
        .map_err(|_| "local_authorization_required")?;
    if file
        .metadata()
        .map_err(|_| "local_authorization_unavailable")?
        .len()
        != 32
    {
        return Err("local_authorization_required");
    }
    let mut bytes = vec![0; 32];
    file.read_exact(&mut bytes)
        .map_err(|_| "local_authorization_unavailable")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn instances_do_not_share_grants_or_replace_runtime_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let first = Authorization::default();
        let second = Authorization::default();
        first.initialize(directory.path()).unwrap();
        assert!(second.initialize(directory.path()).is_err());
        let caller = Uuid::new_v4();
        let proof = local_proof(directory.path()).unwrap();
        first.authorize(caller, &proof).unwrap();
        assert!(second.check(caller).is_err());
        assert_eq!(first.check(caller).unwrap(), caller);
        first.revoke().unwrap();
        second.initialize(directory.path()).unwrap();
        assert!(second.authorize(caller, &proof).is_err());
        second.revoke().unwrap();
    }
    #[test]
    fn unproven_remote_uuid_cannot_authorize_and_grants_bind_exact_caller() {
        let directory = tempfile::tempdir().unwrap();
        initialize(directory.path()).unwrap();
        let caller = Uuid::new_v4();
        let remote = Uuid::new_v4();
        assert!(check(remote).is_err());
        assert!(authorize(remote, &[0; 32]).is_err());
        authorize(caller, &local_proof(directory.path()).unwrap()).unwrap();
        assert_eq!(check(caller).unwrap(), caller);
        assert!(check(remote).is_err());
        revoke();
        assert!(check(caller).is_err());
    }
}
