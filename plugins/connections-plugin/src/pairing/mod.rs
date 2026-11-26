//! Host pairing sessions. One session is open at a time, while the owner runs
//! `bmux remote pair`. Its secret is single-use and expires; failed proofs
//! and waiting requests are bounded. Each request waits on a one-shot decision
//! that the owner's approval or rejection resolves, and the owner's command
//! receives requests as events (no polling interval).
pub mod client;
pub mod protocol;

use crate::link::Secret;
use crate::trust::Role;
use protocol::{DeviceMessage, Refusal};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DEFAULT_TTL: Duration = Duration::from_secs(600);
pub const MAX_TTL: Duration = Duration::from_secs(3_600);
/// Failed proofs before the session closes.
pub const MAX_FAILED_ATTEMPTS: u32 = 5;
/// Requests waiting for the owner at once.
pub const MAX_WAITING: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Approved {
        principal: Uuid,
        host_name: String,
        role: Role,
    },
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Request {
        request_id: String,
        device_name: String,
        device_key: String,
        phrase: String,
    },
    /// A request's device disconnected before a decision.
    Withdrawn { request_id: String },
    /// The session ended; no more events follow.
    Ended { reason: Refusal },
    /// Nothing happened within the wait.
    Idle,
}

struct Waiting {
    device_key: iroh::PublicKey,
    device_name: String,
    decision: tokio::sync::oneshot::Sender<Decision>,
}

struct Session {
    id: String,
    secret: Secret,
    role: Role,
    name: Option<String>,
    expires_at: Instant,
    failed_attempts: u32,
    waiting: BTreeMap<String, Waiting>,
    events: VecDeque<Event>,
    ended: Option<Refusal>,
}

impl Session {
    fn end(&mut self, reason: Refusal) {
        if self.ended.is_some() {
            return;
        }
        self.ended = Some(reason);
        for (_, waiting) in std::mem::take(&mut self.waiting) {
            let _ = waiting.decision.send(Decision::Refused(reason));
        }
        self.events.push_back(Event::Ended { reason });
    }

    fn expire_if_due(&mut self) {
        if self.ended.is_none() && Instant::now() >= self.expires_at {
            self.end(Refusal::Expired);
        }
    }
}

/// What the owner approves: the request's device and the session's terms.
pub struct Approval {
    pub device_key: iroh::PublicKey,
    pub name: String,
    pub role: Role,
    decision: tokio::sync::oneshot::Sender<Decision>,
}

impl Approval {
    /// Tell the waiting device the outcome.
    pub fn resolve(self, decision: Decision) {
        let _ = self.decision.send(decision);
    }
}

/// An accepted proof, waiting for the owner.
pub struct Submitted {
    pub request_id: String,
    pub phrase: String,
    pub expires_at: Instant,
    pub decision: tokio::sync::oneshot::Receiver<Decision>,
}

#[derive(Clone, Debug)]
pub struct Opened {
    pub session_id: String,
    pub secret: Secret,
    pub expires_at_unix: u64,
}

#[derive(Default)]
pub struct Sessions {
    state: Mutex<Option<Session>>,
    changed: tokio::sync::Notify,
}

impl Sessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Open a session, ending any previous one.
    pub fn open(&self, role: Role, name: Option<String>, ttl: Duration) -> anyhow::Result<Opened> {
        let ttl = ttl.clamp(Duration::from_secs(1), MAX_TTL);
        let secret = Secret::generate()?;
        let id = Uuid::new_v4().to_string();
        let mut state = self.lock();
        if let Some(previous) = state.as_mut() {
            previous.end(Refusal::Cancelled);
        }
        *state = Some(Session {
            id: id.clone(),
            secret: secret.clone(),
            role,
            name,
            expires_at: Instant::now() + ttl,
            failed_attempts: 0,
            waiting: BTreeMap::new(),
            events: VecDeque::new(),
            ended: None,
        });
        drop(state);
        self.changed.notify_waiters();
        Ok(Opened {
            session_id: id,
            secret,
            expires_at_unix: crate::trust::now_secs() + ttl.as_secs(),
        })
    }

    /// Whether a pairing connection may be accepted now.
    pub fn is_open(&self) -> bool {
        let mut state = self.lock();
        state.as_mut().is_some_and(|session| {
            session.expire_if_due();
            session.ended.is_none()
        })
    }

    /// Check a device's hello. Only a valid proof creates a request.
    pub fn submit(
        &self,
        host_key: &iroh::PublicKey,
        device_key: iroh::PublicKey,
        hello: &DeviceMessage,
    ) -> Result<Submitted, Refusal> {
        let DeviceMessage::Hello {
            version,
            role,
            device_name,
            proof,
        } = hello;
        let mut state = self.lock();
        let session = state.as_mut().ok_or(Refusal::Expired)?;
        session.expire_if_due();
        if session.ended.is_some() {
            return Err(Refusal::Expired);
        }
        let transcript = protocol::transcript(host_key, &device_key, &session.secret, *role);
        if *version != protocol::VERSION
            || !protocol::proof_matches(proof, &protocol::proof(&session.secret, &transcript))
        {
            session.failed_attempts += 1;
            if session.failed_attempts >= MAX_FAILED_ATTEMPTS {
                session.end(Refusal::Expired);
                drop(state);
                self.changed.notify_waiters();
            }
            return Err(Refusal::Invalid);
        }
        if session.waiting.len() >= MAX_WAITING
            || session.waiting.values().any(|w| w.device_key == device_key)
        {
            return Err(Refusal::Busy);
        }
        let request_id = Uuid::new_v4().simple().to_string()[..8].to_owned();
        let phrase = protocol::phrase(&session.secret, &transcript);
        let device_name = protocol::sanitize_name(device_name);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        session.waiting.insert(
            request_id.clone(),
            Waiting {
                device_key,
                device_name: device_name.clone(),
                decision: sender,
            },
        );
        session.events.push_back(Event::Request {
            request_id: request_id.clone(),
            device_name,
            device_key: device_key.to_string(),
            phrase: phrase.clone(),
        });
        let expires_at = session.expires_at;
        drop(state);
        self.changed.notify_waiters();
        Ok(Submitted {
            request_id,
            phrase,
            expires_at,
            decision: receiver,
        })
    }

    /// The device disconnected or timed out before a decision.
    pub fn withdraw(&self, request_id: &str) {
        let mut state = self.lock();
        if let Some(session) = state.as_mut()
            && session.waiting.remove(request_id).is_some()
        {
            session.events.push_back(Event::Withdrawn {
                request_id: request_id.to_owned(),
            });
            drop(state);
            self.changed.notify_waiters();
        }
    }

    /// The next queued event, or how long until the session expires.
    fn poll_event(&self, session_id: &str) -> anyhow::Result<Result<Event, Duration>> {
        let mut state = self.lock();
        let session = state
            .as_mut()
            .filter(|session| session.id == session_id)
            .ok_or_else(|| anyhow::anyhow!("pairing session is no longer open"))?;
        session.expire_if_due();
        let next = session.events.pop_front().map_or_else(
            || {
                session.ended.map_or_else(
                    || Err(session.expires_at.saturating_duration_since(Instant::now())),
                    |reason| Ok(Event::Ended { reason }),
                )
            },
            Ok,
        );
        drop(state);
        Ok(next)
    }

    /// Wait up to `wait` for the next event of `session_id`.
    pub async fn next_event(&self, session_id: &str, wait: Duration) -> anyhow::Result<Event> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.changed.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            let until_expiry = match self.poll_event(session_id)? {
                Ok(event) => return Ok(event),
                Err(until_expiry) => until_expiry,
            };
            // Wake at expiry to report it.
            let wake = deadline.min(tokio::time::Instant::now() + until_expiry);
            if tokio::time::timeout_at(wake, notified).await.is_err()
                && tokio::time::Instant::now() >= deadline
            {
                return Ok(Event::Idle);
            }
        }
    }

    /// Take a waiting request for the owner's decision.
    pub fn take(&self, session_id: &str, request_id: &str) -> anyhow::Result<Approval> {
        let mut state = self.lock();
        let session = state
            .as_mut()
            .filter(|session| session.id == session_id && session.ended.is_none())
            .ok_or_else(|| anyhow::anyhow!("pairing session is no longer open"))?;
        session.expire_if_due();
        anyhow::ensure!(session.ended.is_none(), "the pairing link expired");
        let waiting = session
            .waiting
            .remove(request_id)
            .ok_or_else(|| anyhow::anyhow!("that pairing request is no longer waiting"))?;
        let (name, role) = (session.name.clone(), session.role);
        drop(state);
        Ok(Approval {
            device_key: waiting.device_key,
            name: name.unwrap_or(waiting.device_name),
            role,
            decision: waiting.decision,
        })
    }

    /// End `session_id` (used after an approval: the secret is single-use).
    pub fn end(&self, session_id: &str, reason: Refusal) {
        let mut state = self.lock();
        if let Some(session) = state.as_mut().filter(|session| session.id == session_id) {
            session.end(reason);
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Drop a finished session entirely.
    pub fn close(&self, session_id: &str) {
        let mut state = self.lock();
        if state
            .as_ref()
            .is_some_and(|session| session.id == session_id)
        {
            if let Some(session) = state.as_mut() {
                session.end(Refusal::Cancelled);
            }
            *state = None;
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> iroh::PublicKey {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    fn hello(secret: &Secret, host: u8, device: u8, role: Role) -> DeviceMessage {
        let transcript = protocol::transcript(&key(host), &key(device), secret, role);
        DeviceMessage::Hello {
            version: protocol::VERSION,
            role,
            device_name: "phone".into(),
            proof: protocol::hex(&protocol::proof(secret, &transcript)),
        }
    }

    #[tokio::test]
    async fn valid_proof_reaches_the_owner_with_matching_phrase() {
        let sessions = Sessions::default();
        let opened = sessions.open(Role::Full, None, DEFAULT_TTL).unwrap();
        let submitted = sessions
            .submit(&key(1), key(2), &hello(&opened.secret, 1, 2, Role::Full))
            .unwrap();
        let Event::Request {
            request_id, phrase, ..
        } = sessions
            .next_event(&opened.session_id, Duration::from_secs(1))
            .await
            .unwrap()
        else {
            panic!("expected a request");
        };
        assert_eq!(request_id, submitted.request_id);
        assert_eq!(phrase, submitted.phrase);
        let approval = sessions.take(&opened.session_id, &request_id).unwrap();
        assert_eq!(approval.device_key, key(2));
        assert_eq!(approval.name, "phone");
        let principal = Uuid::new_v4();
        approval.resolve(Decision::Approved {
            principal,
            host_name: "h".into(),
            role: Role::Full,
        });
        assert_eq!(
            submitted.decision.await.unwrap(),
            Decision::Approved {
                principal,
                host_name: "h".into(),
                role: Role::Full,
            }
        );
        // Single use: ending after approval refuses any reuse of the secret.
        sessions.end(&opened.session_id, Refusal::Expired);
        assert!(!sessions.is_open());
        assert_eq!(
            sessions
                .submit(&key(1), key(3), &hello(&opened.secret, 1, 3, Role::Full))
                .err(),
            Some(Refusal::Expired)
        );
    }

    #[test]
    fn wrong_secret_wrong_host_and_replayed_proofs_are_refused() {
        let sessions = Sessions::default();
        let opened = sessions.open(Role::Full, None, DEFAULT_TTL).unwrap();
        let wrong = Secret([1; 32]);
        assert_eq!(
            sessions
                .submit(&key(1), key(2), &hello(&wrong, 1, 2, Role::Full))
                .err(),
            Some(Refusal::Invalid)
        );
        // A proof made for another host key, or replayed by another device key.
        assert_eq!(
            sessions
                .submit(&key(1), key(2), &hello(&opened.secret, 9, 2, Role::Full))
                .err(),
            Some(Refusal::Invalid)
        );
        assert_eq!(
            sessions
                .submit(&key(1), key(3), &hello(&opened.secret, 1, 2, Role::Full))
                .err(),
            Some(Refusal::Invalid)
        );
        // The role is bound too.
        let DeviceMessage::Hello { proof, .. } = hello(&opened.secret, 1, 2, Role::ViewOnly);
        let tampered = DeviceMessage::Hello {
            version: protocol::VERSION,
            role: Role::Full,
            device_name: "x".into(),
            proof,
        };
        assert_eq!(
            sessions.submit(&key(1), key(2), &tampered).err(),
            Some(Refusal::Invalid)
        );
        assert!(sessions.is_open());
        assert_eq!(
            sessions
                .submit(&key(1), key(2), &hello(&wrong, 1, 2, Role::Full))
                .err(),
            Some(Refusal::Invalid)
        );
        // Five failures close the session for everyone.
        assert!(!sessions.is_open());
        assert_eq!(
            sessions
                .submit(&key(1), key(2), &hello(&opened.secret, 1, 2, Role::Full))
                .err(),
            Some(Refusal::Expired)
        );
    }

    #[tokio::test]
    async fn waiting_requests_are_bounded_and_cancel_refuses_them() {
        let sessions = Sessions::default();
        let opened = sessions
            .open(Role::ViewOnly, Some("tablet".into()), DEFAULT_TTL)
            .unwrap();
        let mut waiting = Vec::new();
        for device in 2..2 + u8::try_from(MAX_WAITING).unwrap() {
            waiting.push(
                sessions
                    .submit(
                        &key(1),
                        key(device),
                        &hello(&opened.secret, 1, device, Role::Full),
                    )
                    .unwrap(),
            );
        }
        assert_eq!(
            sessions
                .submit(&key(1), key(50), &hello(&opened.secret, 1, 50, Role::Full))
                .err(),
            Some(Refusal::Busy)
        );
        sessions.withdraw(&waiting.pop().unwrap().request_id);
        sessions.end(&opened.session_id, Refusal::Cancelled);
        for submitted in waiting {
            assert_eq!(
                submitted.decision.await.unwrap(),
                Decision::Refused(Refusal::Cancelled)
            );
        }
        let mut saw_end = false;
        for _ in 0..10 {
            if let Event::Ended { reason } = sessions
                .next_event(&opened.session_id, Duration::from_millis(10))
                .await
                .unwrap()
            {
                assert_eq!(reason, Refusal::Cancelled);
                saw_end = true;
                break;
            }
        }
        assert!(saw_end);
        sessions.close(&opened.session_id);
        assert!(
            sessions
                .next_event(&opened.session_id, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sessions_expire() {
        let sessions = Sessions::default();
        let opened = sessions
            .open(Role::Full, None, Duration::from_secs(1))
            .unwrap();
        let submitted = sessions
            .submit(&key(1), key(2), &hello(&opened.secret, 1, 2, Role::Full))
            .unwrap();
        let mut events = Vec::new();
        loop {
            let event = sessions
                .next_event(&opened.session_id, Duration::from_secs(3))
                .await
                .unwrap();
            let ended = matches!(event, Event::Ended { .. });
            events.push(event);
            if ended {
                break;
            }
        }
        assert_eq!(
            events.last(),
            Some(&Event::Ended {
                reason: Refusal::Expired
            })
        );
        assert_eq!(
            submitted.decision.await.unwrap(),
            Decision::Refused(Refusal::Expired)
        );
        assert!(!sessions.is_open());
        assert!(sessions.take(&opened.session_id, "x").is_err());
    }
}
