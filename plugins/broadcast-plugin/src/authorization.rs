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
static GRANTS: OnceLock<Mutex<Option<Grants>>> = OnceLock::new();

pub fn initialize(runtime: &Path) -> Result<(), &'static str> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut all = GRANTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| "local_authorization_unavailable")?;
    if all.is_some() {
        return Ok(());
    }
    let mut proof = Uuid::new_v4().as_bytes().to_vec();
    proof.extend_from_slice(Uuid::new_v4().as_bytes());
    let path = runtime.join(PROOF_FILE);
    let temporary = runtime.join(format!("broadcast-proof-{}", Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|_| "local_authorization_unavailable")?;
    file.write_all(&proof)
        .map_err(|_| "local_authorization_unavailable")?;
    file.sync_all()
        .map_err(|_| "local_authorization_unavailable")?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "local_authorization_unavailable")?;
    std::fs::rename(&temporary, &path).map_err(|_| "local_authorization_unavailable")?;
    *all = Some(Grants {
        proof,
        path,
        callers: BTreeMap::new(),
    });
    Ok(())
}

pub fn authorize(caller: Uuid, proof: &[u8]) -> Result<bool, &'static str> {
    let mut all = GRANTS
        .get_or_init(Mutex::default)
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

pub fn check(caller: Uuid) -> Result<Uuid, &'static str> {
    let mut all = GRANTS
        .get_or_init(Mutex::default)
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

pub fn revoke() {
    if let Some(all) = GRANTS.get()
        && let Ok(mut all) = all.lock()
        && let Some(grants) = all.take()
    {
        let _ = std::fs::remove_file(grants.path);
    }
}

/// Caller reads only its selected runtime's private proof. No service reads a
/// caller-chosen proof path, and tokens never enter diagnostic strings.
pub fn local_proof(runtime: &Path) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(runtime.join(PROOF_FILE))
        .map_err(|_| "local_authorization_unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "local_authorization_unavailable")?;
    // SAFETY: geteuid observes process identity without memory access or side effects.
    if !metadata.is_file()
        || metadata.len() != 32
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
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
