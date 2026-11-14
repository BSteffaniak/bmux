//! Generated BPDL client adapter for participants in another loaded component.
//! Static handles are an optimization: the orchestrator uses the same `StatefulPlugin`
//! contract and preserves its existing validation and dependency ordering.
use std::sync::Mutex;

use bmux_plugin_sdk::{
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot, TypedDispatchClient,
};
use bmux_snapshot_plugin_api::{
    snapshot_participant_commands_v1::client as commands,
    snapshot_participant_state_v1::client as state,
    snapshot_participant_types_v1::ParticipantSnapshot,
};

/// The client must already be bound to the selected provider and trusted caller
/// authority by the host. IDs and dependencies come from that provider's
/// advertised participants and restore-dependencies; map them through declared
/// participant identities without interning arbitrary untrusted strings.
/// No raw callback or state address is serialized. Register returned handles in
/// the original authoritative restore order, never discovery completion order.
#[must_use]
pub fn portable_participant<C: TypedDispatchClient + Send + 'static>(
    id: PluginEventKind,
    dependencies: Vec<PluginEventKind>,
    client: C,
) -> StatefulPluginHandle {
    StatefulPluginHandle::new(PortableParticipant {
        id,
        dependencies,
        client: Mutex::new(client),
    })
}

struct PortableParticipant<C> {
    id: PluginEventKind,
    dependencies: Vec<PluginEventKind>,
    client: Mutex<C>,
}
impl<C> PortableParticipant<C> {
    fn error(&self, details: impl std::fmt::Display) -> StatefulPluginError {
        StatefulPluginError::RestoreFailed {
            plugin: self.id.as_str().into(),
            details: details.to_string(),
        }
    }
    fn wire(&self, saved: &StatefulPluginSnapshot) -> StatefulPluginResult<ParticipantSnapshot> {
        if saved.id != self.id || saved.bytes.len() > 16 * 1024 * 1024 {
            return Err(self.error("snapshot identity mismatch or payload capacity exceeded"));
        }
        Ok(ParticipantSnapshot {
            participant_id: self.id.as_str().into(),
            schema_version: saved.version,
            payload: saved.bytes.clone(),
        })
    }
}
impl<C: TypedDispatchClient + Send + 'static> StatefulPlugin for PortableParticipant<C> {
    fn id(&self) -> PluginEventKind {
        self.id.clone()
    }
    fn restore_dependencies(&self) -> Vec<PluginEventKind> {
        self.dependencies.clone()
    }

    fn prepare_final_snapshot(&self) -> StatefulPluginResult<()> {
        // Reject reentrant use instead of holding a blocking lock through nested
        // dispatch. Each participant has its own client; sibling calls do not share
        // an exclusive host callback mutex.
        let mut client = self.client.try_lock().map_err(|error| self.error(error))?;
        bmux_plugin::block_on_typed_dispatch(commands::quiesce(
            &mut *client,
            self.id.as_str().into(),
        ))
        .map_err(|error| self.error(error))?
        .map_err(|error| self.error(format!("{error:?}")))
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let snapshot = || -> StatefulPluginResult<StatefulPluginSnapshot> {
            let mut client = self.client.try_lock().map_err(|error| self.error(error))?;
            let saved = bmux_plugin::block_on_typed_dispatch(state::snapshot(
                &mut *client,
                self.id.as_str().into(),
            ))
            .map_err(|error| self.error(error))?
            .map_err(|error| self.error(format!("{error:?}")))?;
            if saved.participant_id != self.id.as_str() || saved.payload.len() > 16 * 1024 * 1024 {
                return Err(self.error("provider returned wrong identity or oversized snapshot"));
            }
            Ok(StatefulPluginSnapshot::new(
                self.id.clone(),
                saved.schema_version,
                saved.payload,
            ))
        };
        snapshot().map_err(|error| StatefulPluginError::SnapshotFailed {
            plugin: self.id.as_str().into(),
            details: error.to_string(),
        })
    }

    fn validate_snapshot(&self, saved: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        let saved = self.wire(saved)?;
        let mut client = self.client.try_lock().map_err(|error| self.error(error))?;
        let dependencies = bmux_plugin::block_on_typed_dispatch(state::restore_dependencies(
            &mut *client,
            self.id.as_str().into(),
        ))
        .map_err(|error| self.error(error))?
        .map_err(|error| self.error(format!("{error:?}")))?;
        let expected: Vec<&str> = self
            .dependencies
            .iter()
            .map(PluginEventKind::as_str)
            .collect();
        if dependencies.iter().map(String::as_str).collect::<Vec<_>>() != expected {
            return Err(self.error("provider restore dependencies changed; renegotiation required"));
        }
        bmux_plugin::block_on_typed_dispatch(state::validate(&mut *client, saved))
            .map_err(|error| self.error(error))?
            .map_err(|error| self.error(format!("{error:?}")))
    }

    fn restore_snapshot(&self, saved: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        // Validate on the provider before effects; the provider repeats validation
        // inside restore. Transport failures are propagated, never auto-replayed.
        self.validate_snapshot(&saved)?;
        let saved = self.wire(&saved)?;
        let mut client = self.client.try_lock().map_err(|error| self.error(error))?;
        bmux_plugin::block_on_typed_dispatch(commands::restore(&mut *client, saved))
            .map_err(|error| self.error(error))?
            .map_err(|error| self.error(format!("{error:?}")))
    }
}
