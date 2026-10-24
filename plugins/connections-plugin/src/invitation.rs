//! Short-lived, peer-bound invitation approval. This is not admission authority.
use super::{LocalOwner, PairingAuthority, digest, now, random_secret, verify};
use anyhow::{Context, Result, ensure};
use bmux_connections_plugin_api::pairing_types::{
    Device, DeviceAccess, PairingRequest, PairingSession, SessionPhase,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Mutex};
use uuid::Uuid;

const PREFIX: &str = "bmux-pair:v1:";
const MAX_SESSIONS: usize = 32;
const MAX_LINK: usize = 16_384;

/// Never derive Debug: the encoded link contains a reusable invitation secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationLink {
    version: u32,
    pub host_key: String,
    pub session_id: String,
    secret: [u8; 32],
    pub expires_at: u64,
    pub endpoint_targets: Vec<String>,
}
impl std::fmt::Debug for InvitationLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InvitationLink([REDACTED])")
    }
}
impl InvitationLink {
    pub fn parse(link: &str) -> Result<Self> {
        // Never include input or underlying parser diagnostics in an error.
        let parsed = (|| -> Option<Self> {
            if link.len() > MAX_LINK {
                return None;
            }
            let encoded = link.strip_prefix(PREFIX)?;
            if encoded.len() % 2 != 0 {
                return None;
            }
            let bytes: Option<Vec<u8>> = encoded
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    let text = std::str::from_utf8(pair).ok()?;
                    u8::from_str_radix(text, 16).ok()
                })
                .collect();
            serde_json::from_slice(&bytes?).ok()
        })();
        let value = parsed.context("invalid pairing invitation")?;
        ensure!(value.version == 1, "unsupported pairing invitation version");
        ensure!(value.expires_at > now()?, "pairing invitation expired");
        ensure!(
            value.host_key.parse::<iroh::PublicKey>().is_ok(),
            "invalid pairing host"
        );
        ensure!(
            Uuid::parse_str(&value.session_id).is_ok(),
            "invalid pairing session"
        );
        ensure!(
            !value.endpoint_targets.is_empty() && value.endpoint_targets.len() <= 16,
            "invalid pairing routes"
        );
        validate_routes(&value.host_key, &value.endpoint_targets)?;
        Ok(value)
    }
    fn encode(&self) -> Result<String> {
        use std::fmt::Write;
        let mut link = String::from(PREFIX);
        for byte in serde_json::to_vec(self)? {
            write!(link, "{byte:02x}")?;
        }
        ensure!(link.len() <= MAX_LINK, "pairing invitation too large");
        Ok(link)
    }
    #[cfg(test)]
    pub fn transcript(&self, peer: &str, nonce: &[u8]) -> Result<Vec<u8>> {
        self.transcript_named(peer, nonce, None)
    }
    pub fn transcript_named(
        &self,
        peer: &str,
        nonce: &[u8],
        device_name: Option<&str>,
    ) -> Result<Vec<u8>> {
        validate_name(device_name)?;
        transcript(self, peer, nonce, device_name)
    }
}
fn validate_name(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        ensure!(
            !name.trim().is_empty()
                && name.len() <= 64
                && name.chars().all(|c| !c.is_control()
                    && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
            "invalid device display name"
        );
    }
    Ok(())
}
fn validate_routes(host: &str, routes: &[String]) -> Result<()> {
    ensure!(
        !routes.is_empty() && routes.len() <= 16,
        "invalid pairing routes"
    );
    for route in routes {
        let valid = (|| -> Option<()> {
            if route.len() > 1024 || route.chars().any(char::is_control) {
                return None;
            }
            let url = url::Url::parse(route).ok()?;
            if url.scheme() != "device"
                || url.host_str()? != host
                || !url.username().is_empty()
                || url.password().is_some()
                || url.port().is_some()
                || !url.path().is_empty()
                || url.fragment().is_some()
            {
                return None;
            }
            let pairs: Vec<_> = url.query_pairs().collect();
            if pairs.len() != 1 {
                return None;
            }
            let (kind, value) = &pairs[0];
            match kind.as_ref() {
                "addr" => {
                    value.parse::<std::net::SocketAddr>().ok()?;
                }
                "relay" => {
                    let relay = url::Url::parse(value).ok()?;
                    if !matches!(relay.scheme(), "https" | "http")
                        || relay.host_str().is_none()
                        || !relay.username().is_empty()
                        || relay.password().is_some()
                        || relay.fragment().is_some()
                        || relay.query().is_some()
                    {
                        return None;
                    }
                }
                _ => return None,
            }
            Some(())
        })();
        ensure!(valid.is_some(), "invalid pairing route");
    }
    Ok(())
}

fn transcript(
    link: &InvitationLink,
    peer: &str,
    nonce: &[u8],
    device_name: Option<&str>,
) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        "bmux.invitation.v1",
        &link.host_key,
        &link.session_id,
        peer,
        &link.secret,
        nonce,
        link.expires_at,
        DeviceAccess::FullLocalRuntime,
        link.version,
        &link.endpoint_targets,
        device_name,
    ))?)
}
fn phrase(transcript: &[u8]) -> String {
    let hash = digest(transcript);
    // 64 authenticated transcript bits, grouped for comparison; not a password.
    hash[..8]
        .chunks_exact(2)
        .map(|pair| format!("{:02x}{:02x}", pair[0], pair[1]))
        .collect::<Vec<_>>()
        .join("-")
}
struct Session {
    link: InvitationLink,
    phase: SessionPhase,
    peer: Option<String>,
    device_name: Option<String>,
    nonce: Vec<u8>,
    phrase: Option<String>,
    device: Option<Device>,
}
#[derive(Default)]
pub struct PairingSessions {
    sessions: Mutex<BTreeMap<String, Session>>,
}
impl PairingSessions {
    pub fn begin_session(
        &self,
        _owner: &LocalOwner,
        authority: &PairingAuthority,
        ttl: u64,
        targets: Vec<String>,
    ) -> Result<PairingSession> {
        ensure!(
            (1..=600).contains(&ttl),
            "pairing TTL must be 1..600 seconds"
        );
        ensure!(
            !targets.is_empty() && targets.len() <= 16,
            "invalid pairing routes"
        );
        let host = authority.endpoint().parse::<iroh::PublicKey>()?.to_string();
        validate_routes(&host, &targets)?;
        let time = now()?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state unavailable"))?;
        sessions.retain(|_, session| session.link.expires_at > time);
        ensure!(sessions.len() < MAX_SESSIONS, "too many pairing sessions");
        let link = InvitationLink {
            version: 1,
            host_key: host,
            session_id: Uuid::new_v4().to_string(),
            secret: random_secret(),
            expires_at: time.checked_add(ttl).context("expiry overflow")?,
            endpoint_targets: targets,
        };
        let result = PairingSession {
            session_id: link.session_id.clone(),
            invitation_link: link.encode()?,
            expires_at: link.expires_at,
        };
        sessions.insert(
            link.session_id.clone(),
            Session {
                link,
                phase: SessionPhase::WaitingPeer,
                peer: None,
                device_name: None,
                nonce: Vec::new(),
                phrase: None,
                device: None,
            },
        );
        drop(sessions);
        Ok(result)
    }
    fn with<T>(&self, id: &str, operation: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state unavailable"))?;
        let session = sessions.get_mut(id).context("unknown pairing session")?;
        if session.link.expires_at <= now()? {
            session.phase = SessionPhase::Expired;
        }
        let result = operation(session);
        drop(sessions);
        result
    }
    #[cfg(test)]
    pub fn submit(
        &self,
        id: &str,
        peer: &str,
        nonce: &[u8],
        signature: &[u8],
    ) -> Result<PairingRequest> {
        self.submit_named(id, peer, nonce, signature, None)
    }
    pub fn submit_named(
        &self,
        id: &str,
        peer: &str,
        nonce: &[u8],
        signature: &[u8],
        device_name: Option<&str>,
    ) -> Result<PairingRequest> {
        validate_name(device_name)?;
        ensure!(
            nonce.len() == 32 && signature.len() == 64,
            "invalid pairing proof"
        );
        self.with(id, |session| {
            ensure!(session.link.expires_at > now()?, "pairing session expired");
            let message = session.link.transcript_named(peer, nonce, device_name)?;
            verify(peer, &message, signature)
                .map_err(|_| anyhow::anyhow!("invalid pairing proof"))?;
            if session.phase == SessionPhase::WaitingPeer {
                session.peer = Some(peer.to_owned());
                session.device_name = device_name.map(str::to_owned);
                session.nonce = nonce.to_vec();
                session.phrase = Some(phrase(&message));
                session.phase = SessionPhase::PendingApproval;
            } else {
                ensure!(
                    session.peer.as_deref() == Some(peer)
                        && session.nonce == nonce
                        && session.device_name.as_deref() == device_name,
                    "pairing session already bound"
                );
                ensure!(
                    matches!(
                        session.phase,
                        SessionPhase::PendingApproval
                            | SessionPhase::PendingGrant
                            | SessionPhase::Active
                    ),
                    "pairing session closed"
                );
            }
            Ok(info(session))
        })
    }
    pub fn pending_request(&self, _owner: &LocalOwner, id: &str) -> Result<PairingRequest> {
        self.with(id, |session| Ok(info(session)))
    }
    pub fn poll(&self, id: &str, peer: &str, nonce: &[u8]) -> Result<PairingRequest> {
        self.with(id, |session| {
            bound(session, peer, nonce)?;
            Ok(info(session))
        })
    }
    pub fn result(
        &self,
        authority: &PairingAuthority,
        id: &str,
        peer: &str,
        nonce: &[u8],
    ) -> Result<Device> {
        self.with(id, |session| {
            bound(session, peer, nonce)?;
            ensure!(session.phase == SessionPhase::Active, "pairing not active");
            authority.authorized_device(
                session
                    .device
                    .as_ref()
                    .context("missing pairing device")?
                    .device_id
                    .parse()?,
            )
        })
    }
    pub fn approve_session(
        &self,
        owner: &LocalOwner,
        authority: &PairingAuthority,
        id: &str,
        verified_phrase: &str,
    ) -> Result<Device> {
        self.with(id, |session| {
            ensure!(
                session.phase == SessionPhase::PendingApproval,
                "pairing not pending approval"
            );
            ensure!(
                session.phrase.as_deref() == Some(verified_phrase),
                "pairing phrase mismatch"
            );
            let device = authority.stage_device(
                owner,
                Uuid::parse_str(id)?,
                session.peer.as_deref().context("missing pairing peer")?,
            )?;
            session.device = Some(device.clone());
            session.phase = SessionPhase::PendingGrant;
            Ok(device)
        })
    }
    /// Coordinator calls ONLY after the local owner's resource grant succeeds.
    /// No await or permissions call belongs under this mutex. A crash before this
    /// step leaves a denied durable device; recovery requires explicit owner work.
    pub fn activate_session(
        &self,
        owner: &LocalOwner,
        authority: &PairingAuthority,
        id: &str,
    ) -> Result<Device> {
        self.with(id, |session| {
            ensure!(
                session.phase == SessionPhase::PendingGrant,
                "pairing not pending grant"
            );
            let device = authority.grant(
                owner,
                &session
                    .device
                    .as_ref()
                    .context("missing pairing device")?
                    .device_id,
                DeviceAccess::FullLocalRuntime,
            )?;
            session.device = Some(device.clone());
            session.phase = SessionPhase::Active;
            Ok(device)
        })
    }
    /// Revoke first, then compensate resource grants externally. Never re-enable
    /// a device after a partial failure. Active admission leases check revision.
    pub fn abort_session(
        &self,
        owner: &LocalOwner,
        authority: &PairingAuthority,
        id: &str,
        rejected: bool,
    ) -> Result<PairingRequest> {
        self.with(id, |session| {
            if let Some(device) = &session.device {
                authority.revoke(owner, &device.device_id)?;
            }
            session.phase = if rejected {
                SessionPhase::Rejected
            } else {
                SessionPhase::Cancelled
            };
            Ok(info(session))
        })
    }
}
fn bound(session: &Session, peer: &str, nonce: &[u8]) -> Result<()> {
    ensure!(
        session.peer.as_deref() == Some(peer) && session.nonce == nonce,
        "pairing peer mismatch"
    );
    Ok(())
}
fn info(session: &Session) -> PairingRequest {
    PairingRequest {
        session_id: session.link.session_id.clone(),
        phase: session.phase,
        peer_key: session.peer.clone(),
        device_name: session.device_name.clone(),
        verification_phrase: session.phrase.clone(),
        device_id: session
            .device
            .as_ref()
            .map(|device| device.device_id.clone()),
        expires_at: session.link.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (
        tempfile::TempDir,
        PairingAuthority,
        LocalOwner,
        iroh::SecretKey,
        InvitationLink,
    ) {
        let root = tempfile::tempdir().unwrap();
        let host = iroh::SecretKey::generate();
        std::fs::create_dir_all(root.path().join("state")).unwrap();
        std::fs::create_dir_all(root.path().join("runtime")).unwrap();
        let authority = PairingAuthority::open(
            &root.path().join("state"),
            &root.path().join("runtime"),
            host.public().to_string(),
        )
        .unwrap();
        let owner = LocalOwner::from_trusted_local_owner();
        let session = authority
            .sessions
            .begin_session(
                &owner,
                &authority,
                60,
                vec![format!("device://{}?addr=127.0.0.1:1234", host.public())],
            )
            .unwrap();
        let link = InvitationLink::parse(&session.invitation_link).unwrap();
        (root, authority, owner, iroh::SecretKey::generate(), link)
    }
    fn submit(
        authority: &PairingAuthority,
        link: &InvitationLink,
        key: &iroh::SecretKey,
        nonce: &[u8],
    ) -> PairingRequest {
        let signature = key
            .sign(&link.transcript(&key.public().to_string(), nonce).unwrap())
            .to_bytes();
        authority
            .sessions
            .submit(
                &link.session_id,
                &key.public().to_string(),
                nonce,
                &signature,
            )
            .unwrap()
    }
    #[test]
    fn routing_names_expiry_and_abort_are_bound() {
        let (_root, authority, owner, key, link) = fixture();
        let nonce = random_secret();
        let peer = key.public().to_string();
        let name = Some("Work laptop");
        let signature = key
            .sign(&link.transcript_named(&peer, &nonce, name).unwrap())
            .to_bytes();
        assert!(
            authority
                .sessions
                .submit_named(
                    &link.session_id,
                    &peer,
                    &nonce,
                    &signature,
                    Some("Other laptop")
                )
                .is_err()
        );
        let mut changed = link.clone();
        changed.endpoint_targets[0] = format!("device://{}?addr=127.0.0.1:5678", link.host_key);
        let wrong = key
            .sign(&changed.transcript_named(&peer, &nonce, name).unwrap())
            .to_bytes();
        assert!(
            authority
                .sessions
                .submit_named(&link.session_id, &peer, &nonce, &wrong, name)
                .is_err()
        );
        for route in [
            "device://other?addr=127.0.0.1:1".to_owned(),
            format!("device://user@{}?addr=127.0.0.1:1", link.host_key),
            format!(
                "device://{}?relay=https://user:password@example.com",
                link.host_key
            ),
            format!(
                "device://{}?addr=127.0.0.1:1&addr=127.0.0.1:2",
                link.host_key
            ),
        ] {
            changed.endpoint_targets = vec![route];
            assert!(InvitationLink::parse(&changed.encode().unwrap()).is_err());
        }
        assert!(
            link.transcript_named(&peer, &nonce, Some("bad\nname"))
                .is_err()
        );
        let pending = authority
            .sessions
            .submit_named(&link.session_id, &peer, &nonce, &signature, name)
            .unwrap();
        assert_eq!(pending.device_name.as_deref(), name);
        authority
            .sessions
            .approve_session(
                &owner,
                &authority,
                &link.session_id,
                pending.verification_phrase.as_deref().unwrap(),
            )
            .unwrap();
        authority
            .sessions
            .sessions
            .lock()
            .unwrap()
            .get_mut(&link.session_id)
            .unwrap()
            .link
            .expires_at = 0;
        assert!(
            authority
                .sessions
                .activate_session(&owner, &authority, &link.session_id)
                .is_err()
        );
        authority
            .sessions
            .abort_session(&owner, &authority, &link.session_id, false)
            .unwrap();
        assert!(
            authority
                .sessions
                .activate_session(&owner, &authority, &link.session_id)
                .is_err()
        );
    }

    #[test]
    fn grant_gate_and_revocation() {
        let (_root, authority, owner, key, link) = fixture();
        let nonce = random_secret();
        let pending = submit(&authority, &link, &key, &nonce);
        assert_eq!(
            authority
                .sessions
                .pending_request(&owner, &link.session_id)
                .unwrap()
                .phase,
            SessionPhase::PendingApproval
        );
        assert_eq!(pending.phase, SessionPhase::PendingApproval);
        assert!(
            authority
                .sessions
                .result(
                    &authority,
                    &link.session_id,
                    &key.public().to_string(),
                    &nonce
                )
                .is_err()
        );
        assert!(
            authority
                .sessions
                .approve_session(&owner, &authority, &link.session_id, "wrong")
                .is_err()
        );
        let device = authority
            .sessions
            .approve_session(
                &owner,
                &authority,
                &link.session_id,
                pending.verification_phrase.as_deref().unwrap(),
            )
            .unwrap();
        assert_eq!(device.access, DeviceAccess::Denied);
        assert!(
            authority
                .authorized_device(device.device_id.parse().unwrap())
                .is_err()
        );
        authority
            .sessions
            .activate_session(&owner, &authority, &link.session_id)
            .unwrap();
        assert!(
            authority
                .sessions
                .result(
                    &authority,
                    &link.session_id,
                    &key.public().to_string(),
                    &nonce
                )
                .is_ok()
        );
        authority.revoke(&owner, &device.device_id).unwrap();
        assert!(
            authority
                .sessions
                .result(
                    &authority,
                    &link.session_id,
                    &key.public().to_string(),
                    &nonce
                )
                .is_err()
        );
        assert!(
            authority
                .sessions
                .activate_session(&owner, &authority, &link.session_id)
                .is_err()
        );
    }
    #[test]
    fn peer_nonce_signature_secret_and_version_bound() {
        let (_root, authority, _owner, key, link) = fixture();
        let nonce = random_secret();
        let peer = key.public().to_string();
        let mut changed = link.clone();
        changed.secret = random_secret();
        let signature = key
            .sign(&changed.transcript(&peer, &nonce).unwrap())
            .to_bytes();
        assert!(
            authority
                .sessions
                .submit(&link.session_id, &peer, &nonce, &signature)
                .is_err()
        );
        submit(&authority, &link, &key, &nonce);
        assert!(
            authority
                .sessions
                .poll(&link.session_id, &peer, &random_secret())
                .is_err()
        );
        let other = iroh::SecretKey::generate();
        let signature = other
            .sign(
                &link
                    .transcript(&other.public().to_string(), &nonce)
                    .unwrap(),
            )
            .to_bytes();
        assert!(
            authority
                .sessions
                .submit(
                    &link.session_id,
                    &other.public().to_string(),
                    &nonce,
                    &signature
                )
                .is_err()
        );
        changed.version = 2;
        assert!(InvitationLink::parse(&changed.encode().unwrap()).is_err());
        assert_eq!(format!("{link:?}"), "InvitationLink([REDACTED])");
        assert_eq!(
            InvitationLink::parse("bmux-pair:v1:SECRET")
                .unwrap_err()
                .to_string(),
            "invalid pairing invitation"
        );
    }
    #[test]
    fn cancellation_expiry_bounds_and_restart_fail_closed() {
        let (root, authority, owner, key, link) = fixture();
        let nonce = random_secret();
        let pending = submit(&authority, &link, &key, &nonce);
        let device = authority
            .sessions
            .approve_session(
                &owner,
                &authority,
                &link.session_id,
                pending.verification_phrase.as_deref().unwrap(),
            )
            .unwrap();
        let reopened = PairingAuthority::open(
            &root.path().join("state"),
            &root.path().join("runtime"),
            authority.endpoint().into(),
        )
        .unwrap();
        assert!(
            reopened
                .authorized_device(device.device_id.parse().unwrap())
                .is_err()
        );
        authority
            .sessions
            .abort_session(&owner, &authority, &link.session_id, true)
            .unwrap();
        assert!(
            authority
                .sessions
                .activate_session(&owner, &authority, &link.session_id)
                .is_err()
        );
        authority
            .sessions
            .sessions
            .lock()
            .unwrap()
            .get_mut(&link.session_id)
            .unwrap()
            .link
            .expires_at = 0;
        assert!(
            authority
                .sessions
                .submit(
                    &link.session_id,
                    &key.public().to_string(),
                    &nonce,
                    &[0; 64]
                )
                .is_err()
        );
        assert!(
            authority
                .sessions
                .begin_session(&owner, &authority, 601, vec![])
                .is_err()
        );
        for _ in 0..MAX_SESSIONS {
            authority
                .sessions
                .begin_session(&owner, &authority, 60, link.endpoint_targets.clone())
                .unwrap();
        }
        assert!(
            authority
                .sessions
                .begin_session(&owner, &authority, 60, link.endpoint_targets)
                .is_err()
        );
    }
}
