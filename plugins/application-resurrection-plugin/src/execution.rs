//! Execution-scoped state transitions. Persistence must commit the candidate
//! returned by `update` before installing it or acknowledging its revision.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_DESCRIPTOR: usize = 12_288;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub(crate) version: u32,
    pub(crate) invocation: Uuid,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) descriptor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) commands: Vec<crate::recovery::Command>,
}

impl Checkpoint {
    /// Validate a decoded durable checkpoint.
    ///
    /// # Errors
    /// Rejects unknown versions, invalid generations, or malformed descriptors.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_header()?;
        let mut identities = std::collections::BTreeSet::new();
        crate::recovery::validate(self, 0, &mut identities)
    }

    pub(crate) fn validate_header(&self) -> Result<(), &'static str> {
        if !matches!(self.version, 1 | 2) || self.generation == 0 {
            return Err("unsupported or invalid checkpoint");
        }
        if self.version == 1 && !self.commands.is_empty() {
            return Err("nested recovery requires checkpoint version two");
        }
        validate_descriptor(self.descriptor.as_deref())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registration {
    invocation: Uuid,
    generation: u64,
    owner: Uuid,
}

/// Runtime ownership is deliberately excluded from the durable checkpoint.
#[derive(Debug)]
pub struct Execution {
    pub(crate) committed: Checkpoint,
    owner: Option<Uuid>,
    restoration: bool,
    ended: bool,
}

impl Execution {
    /// Begin a new invocation with no restoration offer.
    #[must_use]
    pub fn manual() -> Self {
        Self {
            committed: Checkpoint {
                version: 1,
                invocation: Uuid::new_v4(),
                generation: 1,
                revision: 0,
                descriptor: None,
                commands: Vec::new(),
            },
            owner: None,
            restoration: false,
            ended: false,
        }
    }

    /// Advance fencing generation while retaining recovery state.
    ///
    /// # Errors
    /// Rejects invalid checkpoints and exhausted generation counters.
    pub fn resurrect(checkpoint: Checkpoint) -> Result<Self, &'static str> {
        checkpoint.validate()?;
        let generation = checkpoint
            .generation
            .checked_add(1)
            .ok_or("generation exhausted")?;
        Ok(Self {
            committed: Checkpoint {
                generation,
                revision: 0,
                ..checkpoint
            },
            owner: None,
            restoration: true,
            ended: false,
        })
    }

    pub(crate) fn restoring(&self) -> bool {
        self.restoration
    }

    /// Caller identity must already have been authenticated and bound to this execution.
    ///
    /// # Errors
    /// Rejects ended executions and competing owners.
    pub fn register(&mut self, owner: Uuid) -> Result<Registration, &'static str> {
        if self.ended || self.owner.is_some_and(|current| current != owner) {
            return Err("execution unavailable or already owned");
        }
        self.owner = Some(owner);
        Ok(Registration {
            invocation: self.committed.invocation,
            generation: self.committed.generation,
            owner,
        })
    }

    fn authorize(&self, registration: Registration) -> Result<(), &'static str> {
        if self.ended
            || self.owner != Some(registration.owner)
            || registration.invocation != self.committed.invocation
            || registration.generation != self.committed.generation
        {
            return Err("stale execution registration");
        }
        Ok(())
    }

    /// Offering state does not consume the only durable recovery reference.
    ///
    /// # Errors
    /// Rejects stale registrations.
    pub fn offer(&self, registration: Registration) -> Result<Option<&str>, &'static str> {
        self.authorize(registration)?;
        Ok(if self.restoration {
            self.committed.descriptor.as_deref()
        } else {
            None
        })
    }

    /// Prepare a candidate without modifying committed state.
    ///
    /// # Errors
    /// Rejects stale registrations, conflicting revisions and invalid descriptors.
    pub fn update(
        &self,
        registration: Registration,
        revision: u64,
        descriptor: Option<String>,
    ) -> Result<Checkpoint, &'static str> {
        self.authorize(registration)?;
        validate_descriptor(descriptor.as_deref())?;
        if revision == self.committed.revision && descriptor == self.committed.descriptor {
            return Ok(self.committed.clone());
        }
        if self.committed.revision.checked_add(1) != Some(revision) {
            return Err("revision conflict");
        }
        Ok(Checkpoint {
            revision,
            descriptor,
            ..self.committed.clone()
        })
    }

    /// Call only after the candidate has been durably committed; serialize with updates.
    ///
    /// # Errors
    /// Rejects invalid candidates and stale execution or revision identities.
    pub fn committed(&mut self, candidate: Checkpoint) -> Result<(), &'static str> {
        candidate.validate()?;
        if self.ended
            || candidate.invocation != self.committed.invocation
            || candidate.generation != self.committed.generation
            || !(candidate == self.committed
                || self.committed.revision.checked_add(1) == Some(candidate.revision))
        {
            return Err("stale commit");
        }
        self.committed = candidate;
        self.restoration = false;
        Ok(())
    }

    /// A command boundary, not a disconnected transport, terminates registration.
    pub fn end(&mut self) {
        self.ended = true;
        self.owner = None;
        self.restoration = false;
    }
}

fn validate_descriptor(descriptor: Option<&str>) -> Result<(), &'static str> {
    if let Some(value) = descriptor {
        if value.len() > MAX_DESCRIPTOR {
            return Err("descriptor exceeds limit");
        }
        let parsed: serde_json::Value =
            serde_json::from_str(value).map_err(|_| "invalid descriptor JSON")?;
        if !parsed.is_object() {
            return Err("descriptor must be an object");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved() -> (Execution, Registration) {
        let mut execution = Execution::manual();
        let registration = execution.register(Uuid::new_v4()).unwrap();
        let next = execution
            .update(registration, 1, Some(r#"{"session":"one"}"#.into()))
            .unwrap();
        execution.committed(next).unwrap();
        (execution, registration)
    }

    #[test]
    fn only_resurrection_offers_saved_state() {
        let (mut old, registration) = saved();
        assert_eq!(old.offer(registration).unwrap(), None);
        let mut restored = Execution::resurrect(old.committed.clone()).unwrap();
        assert!(restored.offer(registration).is_err());
        let owner = restored.register(Uuid::new_v4()).unwrap();
        assert!(restored.offer(owner).unwrap().is_some());
        restored.end();
        assert!(restored.offer(owner).is_err());
        old.end();
        let mut manual = Execution::manual();
        let owner = manual.register(Uuid::new_v4()).unwrap();
        assert_eq!(manual.offer(owner).unwrap(), None);
    }

    #[test]
    fn preparation_does_not_acknowledge_or_change_committed_state() {
        let (execution, registration) = saved();
        let candidate = execution.update(registration, 2, None).unwrap();
        assert!(candidate.descriptor.is_none());
        assert!(execution.committed.descriptor.is_some());
        assert_eq!(execution.committed.revision, 1);
    }

    #[test]
    fn retries_and_conflicting_reuse_are_distinct() {
        let (mut execution, registration) = saved();
        assert_eq!(
            execution
                .update(registration, 1, execution.committed.descriptor.clone())
                .unwrap(),
            execution.committed
        );
        assert!(execution.update(registration, 1, None).is_err());
        assert!(execution.update(registration, 3, None).is_err());
        assert!(execution.register(Uuid::new_v4()).is_err());
        let cleared = execution.update(registration, 2, None).unwrap();
        execution.committed(cleared).unwrap();
        assert!(execution.committed.descriptor.is_none());
    }

    #[test]
    fn failed_open_and_host_restart_preserve_offer() {
        let (execution, _) = saved();
        let mut restored = Execution::resurrect(execution.committed).unwrap();
        let owner = restored.register(Uuid::new_v4()).unwrap();
        let offered = restored.offer(owner).unwrap().unwrap().to_owned();
        let mut restarted = Execution::resurrect(restored.committed).unwrap();
        let owner = restarted.register(Uuid::new_v4()).unwrap();
        assert_eq!(restarted.offer(owner).unwrap(), Some(offered.as_str()));
    }

    #[test]
    fn corrupt_and_unsupported_checkpoints_are_rejected() {
        let (mut execution, registration) = saved();
        assert!(
            execution
                .update(registration, 2, Some("x".repeat(MAX_DESCRIPTOR + 1)))
                .is_err()
        );
        assert!(
            execution
                .update(registration, 2, Some("null".into()))
                .is_err()
        );
        execution.committed.version = 99;
        assert!(Execution::resurrect(execution.committed).is_err());
    }
}
