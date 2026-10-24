//! Volatile proof challenges, single-use tickets and revocation-aware leases.
use crate::pairing::{PairingAuthority, digest, now, random_secret, verify};
use anyhow::{Context, Result, ensure};
use bmux_connections_plugin_api::pairing_types::AdmissionChallenge;
use bmux_plugin::stream_admission::{
    StreamAdmissionAuthority, StreamAdmissionBinding, StreamAdmissionError,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct AdmissionAuthority {
    pairing: Arc<PairingAuthority>,
    instance: Uuid,
    state: Mutex<Volatile>,
}

#[derive(Default)]
struct Volatile {
    challenges: BTreeMap<[u8; 32], Challenge>,
    tickets: BTreeMap<[u8; 32], Lease>,
    leases: BTreeMap<Uuid, Lease>,
}

struct Challenge {
    device: Uuid,
    revision: u64,
    expires: u64,
    message: Vec<u8>,
}

#[derive(Clone)]
struct Lease {
    binding: Arc<StreamAdmissionBinding>,
    revision: u64,
    expires: u64,
}

impl AdmissionAuthority {
    pub fn new(pairing: Arc<PairingAuthority>) -> Self {
        Self {
            pairing,
            instance: Uuid::new_v4(),
            state: Mutex::default(),
        }
    }

    pub const fn instance(&self) -> Uuid {
        self.instance
    }

    /// Public challenge creation grants no authority: possession proof is mandatory.
    pub fn begin(&self, device_id: &str) -> Result<AdmissionChallenge> {
        let id = Uuid::parse_str(device_id)?;
        let device = self.pairing.authorized_device(id)?;
        let challenge = random_secret();
        let expires = now()?.checked_add(60).context("expiry overflow")?;
        let message = serde_json::to_vec(&(
            "bmux.admission.v1",
            self.pairing.endpoint(),
            id,
            device.revision,
            challenge,
            expires,
        ))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("authority unavailable"))?;
        state
            .challenges
            .retain(|_, value| value.expires > expires - 60);
        ensure!(state.challenges.len() < 4096, "challenge capacity reached");
        state.challenges.insert(
            digest(&challenge),
            Challenge {
                device: id,
                revision: device.revision,
                expires,
                message: message.clone(),
            },
        );
        drop(state);
        Ok(AdmissionChallenge {
            challenge: challenge.to_vec(),
            possession_message: message,
        })
    }

    pub fn finish(&self, challenge: &[u8], signature: &[u8]) -> Result<Vec<u8>> {
        ensure!(challenge.len() == 32, "invalid challenge");
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("authority unavailable"))?;
        // Every attempt consumes the challenge, including invalid signatures.
        let proof = state
            .challenges
            .remove(&digest(challenge))
            .context("invalid challenge")?;
        let time = now()?;
        ensure!(proof.expires > time, "expired challenge");
        let device = self.pairing.authorized_device(proof.device)?;
        ensure!(device.revision == proof.revision, "device grant changed");
        verify(&device.device_key, &proof.message, signature)?;
        state.tickets.retain(|_, value| value.expires > time);
        state.leases.retain(|_, value| value.expires > time);
        ensure!(
            state.tickets.len() < 4096 && state.leases.len() < 4096,
            "admission capacity reached"
        );
        let ticket = random_secret();
        let lease = Lease {
            binding: Arc::new(StreamAdmissionBinding::new(proof.device, Uuid::new_v4())),
            revision: proof.revision,
            expires: time.checked_add(60).context("expiry overflow")?,
        };
        state.tickets.insert(digest(&ticket), lease);
        drop(state);
        Ok(ticket.to_vec())
    }

    fn current(&self, lease: &Lease) -> Result<()> {
        ensure!(lease.expires > now()?, "lease expired");
        let device = self.pairing.authorized_device(lease.binding.subject_id())?;
        ensure!(device.revision == lease.revision, "device grant changed");
        Ok(())
    }
}

impl StreamAdmissionAuthority for AdmissionAuthority {
    fn consume(
        &self,
        ticket: &[u8],
    ) -> std::result::Result<Arc<StreamAdmissionBinding>, StreamAdmissionError> {
        if ticket.len() != 32 {
            return Err(StreamAdmissionError);
        }
        let mut state = self.state.lock().map_err(|_| StreamAdmissionError)?;
        let lease = state
            .tickets
            .remove(&digest(ticket))
            .ok_or(StreamAdmissionError)?;
        self.current(&lease).map_err(|_| StreamAdmissionError)?;
        let binding = Arc::clone(&lease.binding);
        state.leases.insert(binding.lease_id(), lease);
        drop(state);
        Ok(binding)
    }

    fn validate(
        &self,
        binding: &StreamAdmissionBinding,
    ) -> std::result::Result<(), StreamAdmissionError> {
        let state = self.state.lock().map_err(|_| StreamAdmissionError)?;
        let lease = state
            .leases
            .get(&binding.lease_id())
            .ok_or(StreamAdmissionError)?;
        if lease.binding.as_ref() != binding {
            return Err(StreamAdmissionError);
        }
        let result = self.current(lease).map_err(|_| StreamAdmissionError);
        drop(state);
        result
    }
}

/// Stateless generated-contract adapter for the host's neutral provider bridge.
pub struct AdmissionProviderCodec;

impl AdmissionProviderCodec {
    fn encode<T: serde::Serialize>(
        value: &T,
        command: bool,
    ) -> std::result::Result<Vec<u8>, StreamAdmissionError> {
        use bmux_connections_plugin_api::{
            admission_provider_commands as commands, admission_provider_state as api,
        };
        let (interface, contract) = if command {
            (commands::INTERFACE_ID, commands::SERVICE_CONTRACT)
        } else {
            (api::INTERFACE_ID, api::SERVICE_CONTRACT)
        };
        let bytes =
            bmux_plugin_sdk::encode_service_message(value).map_err(|_| StreamAdmissionError)?;
        bmux_plugin_sdk::service_contract::encode(interface.as_str(), contract, &bytes)
            .map_err(|_| StreamAdmissionError)
    }
    fn decode<T: serde::de::DeserializeOwned>(
        bytes: &[u8],
    ) -> std::result::Result<T, StreamAdmissionError> {
        let bytes = bmux_plugin_sdk::service_contract::decode_response(bytes)
            .map_err(|_| StreamAdmissionError)?;
        bmux_plugin_sdk::decode_service_message(bytes).map_err(|_| StreamAdmissionError)
    }
}

impl bmux_plugin::stream_admission::StreamAdmissionProviderCodec for AdmissionProviderCodec {
    fn encode_probe(&self) -> std::result::Result<Vec<u8>, StreamAdmissionError> {
        Self::encode(&(), false)
    }
    fn decode_probe(&self, bytes: &[u8]) -> std::result::Result<Uuid, StreamAdmissionError> {
        let id: Uuid = Self::decode(bytes)?;
        if id.is_nil() {
            return Err(StreamAdmissionError);
        }
        Ok(id)
    }
    fn encode_consume(&self, ticket: &[u8]) -> std::result::Result<Vec<u8>, StreamAdmissionError> {
        Self::encode(
            &bmux_connections_plugin_api::admission_provider_commands::client::ConsumeRequest {
                ticket: ticket.to_vec(),
            },
            true,
        )
    }
    fn decode_consume(
        &self,
        bytes: &[u8],
    ) -> std::result::Result<StreamAdmissionBinding, StreamAdmissionError> {
        let binding: bmux_connections_plugin_api::pairing_types::AdmissionBinding =
            Self::decode(bytes)?;
        if binding.subject_id.is_nil() || binding.lease_id.is_nil() {
            return Err(StreamAdmissionError);
        }
        Ok(StreamAdmissionBinding::new(
            binding.subject_id,
            binding.lease_id,
        ))
    }
    fn encode_validate(
        &self,
        binding: &StreamAdmissionBinding,
    ) -> std::result::Result<Vec<u8>, StreamAdmissionError> {
        Self::encode(
            &bmux_connections_plugin_api::admission_provider_state::client::ValidateRequest {
                subject_id: binding.subject_id(),
                lease_id: binding.lease_id(),
            },
            false,
        )
    }
    fn decode_validate(&self, bytes: &[u8]) -> std::result::Result<(), StreamAdmissionError> {
        if Self::decode::<bool>(bytes)? {
            Ok(())
        } else {
            Err(StreamAdmissionError)
        }
    }
}

#[cfg(test)]
mod tests {
    fn reply<T: serde::Serialize>(value: &T) -> Vec<u8> {
        bmux_plugin_sdk::service_contract::encode_response(
            &bmux_plugin_sdk::encode_service_message(value).unwrap(),
        )
    }

    use super::*;
    use crate::pairing::LocalOwner;
    use bmux_connections_plugin_api::pairing_types::DeviceAccess;

    #[test]
    fn provider_codec_uses_generated_checked_contracts() {
        use bmux_plugin::stream_admission::StreamAdmissionProviderCodec;
        let codec = AdmissionProviderCodec;
        let id = Uuid::new_v4();
        let bytes = reply(&id);
        assert_eq!(codec.decode_probe(&bytes).unwrap(), id);
        assert!(codec.decode_probe(&reply(&Uuid::nil())).is_err());
        let subject = Uuid::new_v4();
        let lease = Uuid::new_v4();
        let bytes = reply(
            &bmux_connections_plugin_api::pairing_types::AdmissionBinding {
                subject_id: subject,
                lease_id: lease,
            },
        );
        let binding = codec.decode_consume(&bytes).unwrap();
        assert_eq!(binding.subject_id(), subject);
        assert_eq!(binding.lease_id(), lease);
        assert!(codec.decode_consume(&reply(&true)).is_err());
        assert!(codec.decode_validate(&reply(&true)).is_ok());
        assert!(codec.decode_validate(&reply(&false)).is_err());
        assert!(codec.decode_probe(b"unchecked").is_err());
        assert!(
            codec
                .decode_probe(&bmux_plugin_sdk::encode_service_message(&id).unwrap())
                .is_err()
        );
        assert!(
            codec
                .decode_probe(&AdmissionProviderCodec::encode(&id, false).unwrap())
                .is_err()
        );
        assert!(
            codec
                .decode_validate(&AdmissionProviderCodec::encode(&true, false).unwrap())
                .is_err()
        );
    }

    #[test]
    fn tickets_are_single_use_and_durable_revocation_invalidates_leases() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let pairing = Arc::new(
            PairingAuthority::open(
                &state,
                &runtime,
                iroh::SecretKey::generate().public().to_string(),
            )
            .unwrap(),
        );
        let key = iroh::SecretKey::generate();
        let owner = LocalOwner::from_trusted_local_owner();
        let invitation = pairing
            .create_invitation(
                &owner,
                &key.public().to_string(),
                300,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let device = pairing
            .redeem(
                &invitation.invitation_id,
                &invitation.secret,
                &key.sign(&invitation.possession_message).to_bytes(),
            )
            .unwrap();
        let authority = AdmissionAuthority::new(Arc::clone(&pairing));
        let proof = authority.begin(&device.device_id).unwrap();
        let wrong = iroh::SecretKey::generate();
        assert!(
            authority
                .finish(
                    &proof.challenge,
                    &wrong.sign(&proof.possession_message).to_bytes()
                )
                .is_err()
        );
        assert!(
            authority
                .finish(
                    &proof.challenge,
                    &key.sign(&proof.possession_message).to_bytes()
                )
                .is_err()
        );
        let proof = authority.begin(&device.device_id).unwrap();
        authority
            .state
            .lock()
            .unwrap()
            .challenges
            .get_mut(&digest(&proof.challenge))
            .unwrap()
            .expires = 0;
        assert!(
            authority
                .finish(
                    &proof.challenge,
                    &key.sign(&proof.possession_message).to_bytes()
                )
                .is_err()
        );
        let proof = authority.begin(&device.device_id).unwrap();
        let ticket = authority
            .finish(
                &proof.challenge,
                &key.sign(&proof.possession_message).to_bytes(),
            )
            .unwrap();
        assert!(
            authority
                .finish(
                    &proof.challenge,
                    &key.sign(&proof.possession_message).to_bytes()
                )
                .is_err()
        );
        let binding = authority.consume(&ticket).unwrap();
        assert!(authority.consume(&ticket).is_err());
        assert!(authority.validate(&binding).is_ok());
        let forged = StreamAdmissionBinding::new(binding.subject_id(), Uuid::new_v4());
        assert!(authority.validate(&forged).is_err());
        pairing.revoke(&owner, &device.device_id).unwrap();
        assert!(authority.validate(&binding).is_err());
        pairing
            .grant(&owner, &device.device_id, DeviceAccess::FullLocalRuntime)
            .unwrap();
        assert!(authority.validate(&binding).is_err());
        let restarted = AdmissionAuthority::new(pairing);
        assert!(restarted.consume(&ticket).is_err());
        assert!(restarted.validate(&binding).is_err());
    }
}
