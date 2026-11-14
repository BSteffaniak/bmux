//! Portable participant operations over the same retained registry used by saves.
//! No callback addresses or concrete runtime layouts enter the wire contract.
use std::sync::{Arc, RwLock};

use bmux_plugin_sdk::{StatefulPluginHandle, StatefulPluginSnapshot};
use bmux_snapshot_plugin_api::snapshot_participant_types_v1::{
    ParticipantError, ParticipantSnapshot,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;

pub(crate) struct Participants(pub Arc<RwLock<StatefulPluginRegistry>>);

const MAX_PARTICIPANTS: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

impl Participants {
    pub(crate) fn ids(&self) -> Result<Vec<String>, ParticipantError> {
        let registry = self
            .0
            .read()
            .map_err(|_| unavailable("participant registry poisoned"))?;
        if registry.len() > MAX_PARTICIPANTS {
            return Err(unavailable("participant capacity exceeded"));
        }
        let ids: Vec<String> = registry
            .as_slice()
            .iter()
            .map(|entry| entry.as_dyn().id().as_str().to_owned())
            .collect();
        let unique: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        if unique.len() != ids.len() {
            return Err(unavailable("ambiguous participant registration"));
        }
        Ok(ids)
    }

    fn get(&self, id: &str) -> Result<StatefulPluginHandle, ParticipantError> {
        if id.is_empty() || id.len() > 256 {
            return Err(invalid("invalid participant identity"));
        }
        let registry = self
            .0
            .read()
            .map_err(|_| unavailable("participant registry poisoned"))?;
        if registry.len() > MAX_PARTICIPANTS {
            return Err(unavailable("participant capacity exceeded"));
        }
        let mut matches = registry
            .as_slice()
            .iter()
            .filter(|entry| entry.as_dyn().id().as_str() == id);
        let found = matches
            .next()
            .cloned()
            .ok_or_else(|| unavailable("participant not registered"))?;
        if matches.next().is_some() {
            return Err(unavailable("ambiguous participant registration"));
        }
        Ok(found)
    }

    pub(crate) fn dependencies(&self, id: &str) -> Result<Vec<String>, ParticipantError> {
        let dependencies = self.get(id)?.as_dyn().restore_dependencies();
        if dependencies.len() > MAX_PARTICIPANTS {
            return Err(invalid("restore dependency capacity exceeded"));
        }
        Ok(dependencies
            .into_iter()
            .map(|id| id.as_str().to_owned())
            .collect())
    }

    pub(crate) fn snapshot(&self, id: &str) -> Result<ParticipantSnapshot, ParticipantError> {
        let entry = self.get(id)?;
        let saved = entry.as_dyn().snapshot().map_err(failed)?;
        if saved.bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("snapshot payload capacity exceeded"));
        }
        if saved.id.as_str() != id {
            return Err(invalid(
                "participant returned a different snapshot identity",
            ));
        }
        Ok(ParticipantSnapshot {
            participant_id: id.to_owned(),
            schema_version: saved.version,
            payload: saved.bytes,
        })
    }

    fn decode(
        &self,
        saved: ParticipantSnapshot,
    ) -> Result<(StatefulPluginHandle, StatefulPluginSnapshot), ParticipantError> {
        // Resolve to the retained owner's ID, never leak/intern untrusted strings
        // to manufacture the SDK's static event-kind identity.
        if saved.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("snapshot payload capacity exceeded"));
        }
        let entry = self.get(&saved.participant_id)?;
        let snapshot =
            StatefulPluginSnapshot::new(entry.as_dyn().id(), saved.schema_version, saved.payload);
        entry
            .as_dyn()
            .validate_snapshot(&snapshot)
            .map_err(failed)?;
        Ok((entry, snapshot))
    }

    pub(crate) fn validate(&self, saved: ParticipantSnapshot) -> Result<(), ParticipantError> {
        self.decode(saved).map(|_| ())
    }

    pub(crate) fn restore(&self, saved: ParticipantSnapshot) -> Result<(), ParticipantError> {
        let (entry, snapshot) = self.decode(saved)?;
        entry.as_dyn().restore_snapshot(snapshot).map_err(failed)
    }

    pub(crate) fn quiesce(&self, id: &str) -> Result<(), ParticipantError> {
        self.get(id)?
            .as_dyn()
            .prepare_final_snapshot()
            .map_err(failed)
    }
}

pub(crate) fn unavailable(message: &str) -> ParticipantError {
    ParticipantError::Unavailable {
        message: message.into(),
    }
}
fn invalid(message: &str) -> ParticipantError {
    ParticipantError::Invalid {
        message: message.into(),
    }
}
fn failed(error: impl std::fmt::Display) -> ParticipantError {
    ParticipantError::Failed {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::{
        PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginResult,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Owner(Arc<AtomicUsize>);
    impl StatefulPlugin for Owner {
        fn id(&self) -> PluginEventKind {
            PluginEventKind::from_static("test.owner")
        }
        fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
            Ok(StatefulPluginSnapshot::new(self.id(), 1, vec![7]))
        }
        fn validate_snapshot(&self, saved: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            if saved.version != 1 || saved.bytes != [7] {
                return Err(StatefulPluginError::RestoreFailed {
                    plugin: "test.owner".into(),
                    details: "invalid payload".into(),
                });
            }
            Ok(())
        }
        fn restore_snapshot(&self, _saved: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[test]
    fn portable_restore_validates_before_effects_and_keeps_owner_identity() {
        let restored = Arc::new(AtomicUsize::new(0));
        let mut registry = StatefulPluginRegistry::new();
        registry.push(StatefulPluginHandle::new(Owner(Arc::clone(&restored))));
        let port = Participants(Arc::new(RwLock::new(registry)));
        let mut saved = port.snapshot("test.owner").unwrap();
        saved.schema_version = 99;
        assert!(port.restore(saved).is_err());
        assert_eq!(restored.load(Ordering::SeqCst), 0);
        port.restore(port.snapshot("test.owner").unwrap()).unwrap();
        assert_eq!(restored.load(Ordering::SeqCst), 1);
        assert!(port.snapshot("another.owner").is_err());
    }
    #[test]
    fn duplicate_identity_is_not_arbitrarily_selected() {
        let mut registry = StatefulPluginRegistry::new();
        for _ in 0..2 {
            registry.push(StatefulPluginHandle::new(Owner(Arc::new(
                AtomicUsize::new(0),
            ))));
        }
        let port = Participants(Arc::new(RwLock::new(registry)));
        assert!(port.snapshot("test.owner").is_err());
        assert!(port.ids().is_err());
    }

    #[test]
    fn unbound_definition_retains_canonical_identity() {
        let definition = crate::plugin_definition();
        let canonical = bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<
            crate::SnapshotPlugin,
        >(
            include_str!("../plugin.toml"),
            vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
        )
        .unwrap();
        assert_eq!(definition.metadata, canonical.metadata);
    }

    #[test]
    fn independent_hosts_never_discover_each_others_participants() {
        let mut a = StatefulPluginRegistry::new();
        a.push(StatefulPluginHandle::new(Owner(Arc::new(
            AtomicUsize::new(0),
        ))));
        let a = Participants(Arc::new(RwLock::new(a)));
        let b = Participants(Arc::new(RwLock::new(StatefulPluginRegistry::new())));
        assert_eq!(a.ids().unwrap(), ["test.owner"]);
        assert_eq!(b.ids().unwrap(), Vec::<String>::new());
        assert!(b.restore(a.snapshot("test.owner").unwrap()).is_err());
    }
}
