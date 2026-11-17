//! Public-safe retained indication; no publisher initialization on reads.
//!
//! Each [`FactIndicator`] owns its indication state and publishes it into the
//! presentation fact host of the registry it was bound to (the attach
//! runtime's registry for the attach mirror, the activation's registry view
//! for the backend). Nothing is process-wide.
use bmux_broadcast_plugin_api::broadcast_presentation_v1::Indicator;
use bmux_presentation_state::{
    PresentationEntityRef, PresentationFact, PresentationFactHostService, PresentationFactRole,
    PresentationFactSnapshot,
};
use std::sync::{Arc, Mutex, RwLock};

type FactHost = Arc<RwLock<PresentationFactHostService>>;

/// The fact host shared by everything presenting in one host or runtime.
#[must_use]
pub fn fact_host(states: &bmux_plugin::PluginStateRegistry) -> FactHost {
    states.get_or_insert_with(PresentationFactHostService::new)
}

/// One owner's indicator state plus the fact host it publishes to.
pub struct FactIndicator {
    current: Mutex<(&'static str, u64)>,
    host: RwLock<Option<FactHost>>,
}

impl Default for FactIndicator {
    fn default() -> Self {
        Self {
            current: Mutex::new(("offline", 1)),
            host: RwLock::new(None),
        }
    }
}

impl FactIndicator {
    /// An indicator publishing into `host`.
    #[must_use]
    pub fn bound(host: FactHost) -> Self {
        let indicator = Self::default();
        indicator.bind(Some(host));
        indicator
    }

    /// Select (or clear) the fact host future publications go to.
    pub fn bind(&self, host: Option<FactHost>) {
        if let Ok(mut slot) = self.host.write() {
            *slot = host;
        }
    }

    fn host(&self) -> Option<FactHost> {
        self.host.read().ok().and_then(|host| host.clone())
    }

    pub(crate) fn status(&self) -> Indicator {
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Indicator {
            state: current.0.into(),
            revision: current.1,
        }
    }

    pub(crate) fn update(&self, local: &str, remote: &str) {
        let state = match local {
            "stopped" => "offline",
            "paused" => "paused",
            "running" if remote == "live" => "live",
            _ => "unavailable",
        };
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.0 != state {
            current.0 = state;
            current.1 = current.1.saturating_add(1);
        }
        let Some(host) = self.host() else {
            return;
        };
        let (text, role) = match state {
            "live" => ("LIVE", PresentationFactRole::Active),
            "paused" => ("PAUSED", PresentationFactRole::Warning),
            "offline" => ("OFFLINE", PresentationFactRole::Idle),
            _ => ("UNAVAILABLE", PresentationFactRole::Error),
        };
        let Ok(host) = host.read() else {
            return;
        };
        let _ = host.registry().publish_authorized(
            "bmux.broadcast",
            "bmux.broadcast",
            PresentationFactSnapshot {
                revision: current.1,
                facts: vec![PresentationFact {
                    entity: PresentationEntityRef::new("bmux.broadcast", "indicator"),
                    key: "status".into(),
                    role,
                    short_text: text.into(),
                    detail_text: None,
                    icon_id: None,
                    priority: 100,
                }],
            },
        );
    }

    pub(crate) fn remove(&self) {
        if let Some(host) = self.host()
            && let Ok(host) = host.read()
        {
            host.registry().remove_producer("bmux.broadcast");
        }
    }
}

impl crate::controller::IndicatorPort for FactIndicator {
    fn update(&self, local: &str, remote: &str) {
        Self::update(self, local, remote);
    }
    fn remove(&self) {
        Self::remove(self);
    }
    fn status(&self) -> Indicator {
        Self::status(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_runtimes_keep_independent_indicators() {
        let first_states = bmux_plugin::PluginStateRegistry::new();
        let second_states = bmux_plugin::PluginStateRegistry::new();
        let first = FactIndicator::bound(fact_host(&first_states));
        let second = FactIndicator::bound(fact_host(&second_states));
        first.update("running", "live");
        assert_eq!(first.status().state, "live");
        assert_eq!(second.status().state, "offline");
        let facts = |states: &bmux_plugin::PluginStateRegistry| {
            fact_host(states)
                .read()
                .unwrap()
                .registry()
                .facts_for_entity(&PresentationEntityRef::new("bmux.broadcast", "indicator"))
                .len()
        };
        assert_eq!(facts(&first_states), 1);
        assert_eq!(facts(&second_states), 0);
        first.remove();
        assert_eq!(facts(&first_states), 0);
    }
}
