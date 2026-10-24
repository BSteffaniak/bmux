//! Durable device authority. Transport access is never administrative authority.
#[path = "invitation.rs"]
pub mod invitation;

use crate::storage::PairingStore;
use anyhow::{Context, Result, bail, ensure};
use bmux_connections_plugin_api::pairing_types::{
    Device, DeviceAccess, DeviceAccessCheck, Invitation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Construct only from positively authenticated host-local owner provenance.
/// In particular, a missing client ID or admission binding is NOT local proof.
pub struct LocalOwner(());

impl LocalOwner {
    pub const fn from_trusted_local_owner() -> Self {
        Self(())
    }
}

pub struct PairingAuthority {
    pub sessions: invitation::PairingSessions,
    store: PairingStore,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    endpoint: String,
    invitations: BTreeMap<Uuid, StoredInvitation>,
    devices: BTreeMap<Uuid, Device>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredInvitation {
    key: String,
    digest: [u8; 32],
    expires: u64,
    access: DeviceAccess,
    consumed: Option<Uuid>,
}

impl PairingAuthority {
    pub fn open(state: &Path, runtime: &Path, endpoint: String) -> Result<Self> {
        let authority = Self {
            sessions: invitation::PairingSessions::default(),
            store: PairingStore::open(state, runtime)?,
            endpoint,
        };
        authority.update(|_| Ok(()))?;
        Ok(authority)
    }

    fn update<T>(&self, operation: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.store.transaction(|record| {
            let mut state: State = match record {
                Some(bytes) => serde_json::from_slice(bytes)
                    .map_err(|_| anyhow::anyhow!("invalid pairing record"))?,
                None => State {
                    version: 1,
                    endpoint: self.endpoint.clone(),
                    invitations: BTreeMap::new(),
                    devices: BTreeMap::new(),
                },
            };
            ensure!(
                state.version == 1 && state.endpoint == self.endpoint,
                "pairing record scope/version mismatch"
            );
            let result = operation(&mut state)?;
            *record = Some(serde_json::to_vec(&state)?);
            Ok(result)
        })
    }

    pub fn create_invitation(
        &self,
        _owner: &LocalOwner,
        device_key: &str,
        ttl_seconds: u64,
        access: DeviceAccess,
    ) -> Result<Invitation> {
        ensure!(
            (1..=3600).contains(&ttl_seconds),
            "invitation TTL must be 1..3600 seconds"
        );
        let key = device_key
            .parse::<iroh::PublicKey>()
            .context("invalid device key")?
            .to_string();
        let id = Uuid::new_v4();
        let secret = random_secret();
        let expires = now()?
            .checked_add(ttl_seconds)
            .context("invitation expiry overflow")?;
        let message = possession_message(&self.endpoint, id, &key, &secret, expires, access)?;
        self.update(|state| {
            ensure!(
                state.invitations.len() < 4096,
                "invitation capacity reached"
            );
            state.invitations.insert(
                id,
                StoredInvitation {
                    key: key.clone(),
                    digest: digest(&secret),
                    expires,
                    access,
                    consumed: None,
                },
            );
            Ok(())
        })?;
        Ok(Invitation {
            invitation_id: id.to_string(),
            endpoint_id: self.endpoint.clone(),
            device_key: key,
            secret: secret.to_vec(),
            expires_at: expires,
            access,
            possession_message: message,
        })
    }

    pub fn redeem(&self, invitation_id: &str, secret: &[u8], signature: &[u8]) -> Result<Device> {
        self.redeem_bound(invitation_id, secret, signature, None)
    }

    /// Transport peer binding is checked inside the same transaction BEFORE mutation.
    pub fn redeem_for_peer(
        &self,
        invitation_id: &str,
        secret: &[u8],
        signature: &[u8],
        peer_key: &str,
    ) -> Result<Device> {
        self.redeem_bound(invitation_id, secret, signature, Some(peer_key))
    }

    fn redeem_bound(
        &self,
        invitation_id: &str,
        secret: &[u8],
        signature: &[u8],
        peer: Option<&str>,
    ) -> Result<Device> {
        ensure!(secret.len() == 32, "invalid invitation");
        let id = Uuid::parse_str(invitation_id)?;
        self.update(|state| {
            let invitation = state
                .invitations
                .get_mut(&id)
                .context("invalid invitation")?;
            ensure!(
                peer.is_none_or(|peer| peer == invitation.key),
                "device transport mismatch"
            );
            ensure!(
                invitation.expires > now()? && invitation.digest == digest(secret),
                "expired or invalid invitation"
            );
            verify(
                &invitation.key,
                &possession_message(
                    &self.endpoint,
                    id,
                    &invitation.key,
                    secret,
                    invitation.expires,
                    invitation.access,
                )?,
                signature,
            )?;
            // A retry returns the CURRENT durable device, never restores a revoked grant.
            if let Some(device_id) = invitation.consumed {
                return state
                    .devices
                    .get(&device_id)
                    .cloned()
                    .context("missing enrolled device");
            }
            // A second invitation cannot silently revive or duplicate an existing key.
            ensure!(
                !state
                    .devices
                    .values()
                    .any(|device| device.device_key == invitation.key),
                "device already enrolled; owner must grant explicitly"
            );
            let device_id = id;
            let device = Device {
                device_id: device_id.to_string(),
                device_key: invitation.key.clone(),
                access: invitation.access,
                revision: 1,
            };
            invitation.consumed = Some(device_id);
            state.devices.insert(device_id, device.clone());
            Ok(device)
        })
    }

    pub fn grant(
        &self,
        _owner: &LocalOwner,
        device_id: &str,
        access: DeviceAccess,
    ) -> Result<Device> {
        let id = Uuid::parse_str(device_id)?;
        self.update(|state| {
            let device = state.devices.get_mut(&id).context("unknown device")?;
            device.revision = device
                .revision
                .checked_add(1)
                .context("device revision overflow")?;
            device.access = access;
            Ok(device.clone())
        })
    }

    pub fn revoke(&self, owner: &LocalOwner, device_id: &str) -> Result<Device> {
        self.grant(owner, device_id, DeviceAccess::Denied)
    }

    pub fn list_devices(&self, _owner: &LocalOwner) -> Result<Vec<Device>> {
        self.update(|state| Ok(state.devices.values().cloned().collect()))
    }

    /// Authoritative read for resource policy. Unknown subjects have no grant.
    pub fn check_device_access(&self, subject_id: Uuid) -> Result<DeviceAccessCheck> {
        self.update(|state| {
            Ok(state.devices.get(&subject_id).map_or(
                DeviceAccessCheck {
                    access: DeviceAccess::Denied,
                    revision: 0,
                },
                |device| DeviceAccessCheck {
                    access: device.access,
                    revision: device.revision,
                },
            ))
        })
    }

    pub fn authorized_device(&self, id: Uuid) -> Result<Device> {
        self.update(|state| {
            let device = state.devices.get(&id).context("unknown device")?;
            if device.access != DeviceAccess::FullLocalRuntime {
                bail!("device access denied");
            }
            Ok(device.clone())
        })
    }

    /// Stage enrollment with no admission authority. Resource grants are a
    /// separate coordinator-owned operation; activate only after they succeed.
    pub fn stage_device(&self, _owner: &LocalOwner, id: Uuid, key: &str) -> Result<Device> {
        let key = key.parse::<iroh::PublicKey>()?.to_string();
        self.update(|state| {
            ensure!(!state.devices.contains_key(&id), "device already staged");
            ensure!(
                !state
                    .devices
                    .values()
                    .any(|device| device.device_key == key),
                "device already enrolled"
            );
            let device = Device {
                device_id: id.to_string(),
                device_key: key,
                access: DeviceAccess::Denied,
                revision: 1,
            };
            state.devices.insert(id, device.clone());
            Ok(device)
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

fn possession_message(
    endpoint: &str,
    id: Uuid,
    key: &str,
    secret: &[u8],
    expires: u64,
    access: DeviceAccess,
) -> Result<Vec<u8>> {
    // Versioned, unambiguous serialization, distinct from admission signatures.
    Ok(serde_json::to_vec(&(
        "bmux.pairing.v1",
        endpoint,
        id,
        key,
        secret,
        expires,
        access,
    ))?)
}

pub fn random_secret() -> [u8; 32] {
    iroh::SecretKey::generate().to_bytes()
}
pub fn digest(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}
pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
pub fn verify(key: &str, message: &[u8], signature: &[u8]) -> Result<()> {
    key.parse::<iroh::PublicKey>()?
        .verify(message, &iroh::Signature::try_from(signature)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn authority(root: &Path) -> PairingAuthority {
        let state = root.join("state");
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        PairingAuthority::open(
            &state,
            &runtime,
            iroh::SecretKey::generate().public().to_string(),
        )
        .unwrap()
    }

    #[test]
    fn possession_retry_and_revocation_are_durable() {
        let root = tempfile::tempdir().unwrap();
        let mut authority = authority(root.path());
        let owner = LocalOwner::from_trusted_local_owner();
        let key = iroh::SecretKey::generate();
        let invitation = authority
            .create_invitation(
                &owner,
                &key.public().to_string(),
                300,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let wrong = iroh::SecretKey::generate().sign(&invitation.possession_message);
        assert!(
            authority
                .redeem(
                    &invitation.invitation_id,
                    &invitation.secret,
                    &wrong.to_bytes()
                )
                .is_err()
        );
        let signature = key.sign(&invitation.possession_message).to_bytes();
        let device = authority
            .redeem(&invitation.invitation_id, &invitation.secret, &signature)
            .unwrap();
        assert_eq!(
            device,
            authority
                .redeem(&invitation.invitation_id, &invitation.secret, &signature)
                .unwrap()
        );
        let revoked = authority.revoke(&owner, &device.device_id).unwrap();
        assert_eq!(revoked.access, DeviceAccess::Denied);
        assert_eq!(
            authority.check_device_access(Uuid::new_v4()).unwrap(),
            DeviceAccessCheck {
                access: DeviceAccess::Denied,
                revision: 0
            }
        );
        assert_eq!(
            authority
                .check_device_access(Uuid::parse_str(&device.device_id).unwrap())
                .unwrap(),
            DeviceAccessCheck {
                access: DeviceAccess::Denied,
                revision: revoked.revision
            }
        );
        assert_eq!(
            revoked,
            authority
                .redeem(&invitation.invitation_id, &invitation.secret, &signature)
                .unwrap()
        );
        let reopened = PairingAuthority::open(
            &root.path().join("state"),
            &root.path().join("runtime"),
            std::mem::take(&mut authority.endpoint),
        )
        .unwrap();
        assert!(
            reopened
                .authorized_device(Uuid::parse_str(&device.device_id).unwrap())
                .is_err()
        );
        assert_eq!(reopened.list_devices(&owner).unwrap(), vec![revoked]);
    }

    #[test]
    fn mismatched_transport_peer_does_not_consume_or_enroll() {
        let root = tempfile::tempdir().unwrap();
        let authority = authority(root.path());
        let owner = LocalOwner::from_trusted_local_owner();
        let key = iroh::SecretKey::generate();
        let invitation = authority
            .create_invitation(
                &owner,
                &key.public().to_string(),
                300,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let signature = key.sign(&invitation.possession_message).to_bytes();
        let wrong_peer = iroh::SecretKey::generate().public().to_string();
        assert!(
            authority
                .redeem_for_peer(
                    &invitation.invitation_id,
                    &invitation.secret,
                    &signature,
                    &wrong_peer
                )
                .is_err()
        );
        assert!(authority.list_devices(&owner).unwrap().is_empty());
        authority
            .update(|state| {
                assert!(
                    state.invitations[&Uuid::parse_str(&invitation.invitation_id)?]
                        .consumed
                        .is_none()
                );
                Ok(())
            })
            .unwrap();
        let device = authority
            .redeem_for_peer(
                &invitation.invitation_id,
                &invitation.secret,
                &signature,
                &key.public().to_string(),
            )
            .unwrap();
        assert_eq!(device.device_key, key.public().to_string());
        assert_eq!(authority.list_devices(&owner).unwrap(), vec![device]);
    }

    #[test]
    fn concurrent_redemptions_commit_one_device() {
        let root = tempfile::tempdir().unwrap();
        let authority = std::sync::Arc::new(authority(root.path()));
        let owner = LocalOwner::from_trusted_local_owner();
        let key = iroh::SecretKey::generate();
        let invitation = authority
            .create_invitation(
                &owner,
                &key.public().to_string(),
                300,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let signature = key.sign(&invitation.possession_message).to_bytes();
        let devices = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let authority = &authority;
                let invitation = &invitation;
                handles.push(scope.spawn(move || {
                    authority
                        .redeem(&invitation.invitation_id, &invitation.secret, &signature)
                        .unwrap()
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(devices.iter().all(|device| device == &devices[0]));
        assert_eq!(authority.list_devices(&owner).unwrap().len(), 1);
    }

    #[test]
    fn invitations_bind_scope_expiry_secret_and_explicit_grant() {
        let root = tempfile::tempdir().unwrap();
        let authority = authority(root.path());
        let owner = LocalOwner::from_trusted_local_owner();
        let key = iroh::SecretKey::generate();
        assert!(
            authority
                .create_invitation(&owner, &key.public().to_string(), 0, DeviceAccess::Denied)
                .is_err()
        );
        let invitation = authority
            .create_invitation(&owner, &key.public().to_string(), 300, DeviceAccess::Denied)
            .unwrap();
        let signature = key.sign(&invitation.possession_message).to_bytes();
        assert!(
            authority
                .redeem(&invitation.invitation_id, &[0; 32], &signature)
                .is_err()
        );
        let device = authority
            .redeem(&invitation.invitation_id, &invitation.secret, &signature)
            .unwrap();
        assert!(
            authority
                .authorized_device(Uuid::parse_str(&device.device_id).unwrap())
                .is_err()
        );
        authority
            .grant(&owner, &device.device_id, DeviceAccess::FullLocalRuntime)
            .unwrap();
        assert!(
            authority
                .authorized_device(Uuid::parse_str(&device.device_id).unwrap())
                .is_ok()
        );
        authority
            .update(|state| {
                state
                    .invitations
                    .get_mut(&Uuid::parse_str(&invitation.invitation_id)?)
                    .unwrap()
                    .expires = 0;
                Ok(())
            })
            .unwrap();
        assert!(
            authority
                .redeem(&invitation.invitation_id, &invitation.secret, &signature)
                .is_err()
        );
        assert!(
            PairingAuthority::open(
                &root.path().join("state"),
                &root.path().join("runtime"),
                "wrong-endpoint".into()
            )
            .is_err()
        );
    }
}
