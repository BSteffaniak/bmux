//! Public-safe retained indication; no publisher initialization on reads.
use bmux_broadcast_plugin_api::broadcast_presentation_v1::Indicator;
use bmux_presentation_state::{
    PresentationEntityRef, PresentationFact, PresentationFactRole, PresentationFactSnapshot,
    global_presentation_fact_host_service,
};
use std::sync::Mutex;
static CURRENT: Mutex<(&str, u64)> = Mutex::new(("offline", 1));
pub(crate) fn status() -> Indicator {
    let current = CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Indicator {
        state: current.0.into(),
        revision: current.1,
    }
}
pub(crate) fn update(local: &str, remote: &str) {
    let state = match local {
        "stopped" => "offline",
        "paused" => "paused",
        "running" if remote == "live" => "live",
        _ => "unavailable",
    };
    let mut current = CURRENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.0 != state {
        current.0 = state;
        current.1 = current.1.saturating_add(1);
    }
    {
        let host = global_presentation_fact_host_service();
        let (text, role) = match state {
            "live" => ("LIVE", PresentationFactRole::Active),
            "paused" => ("PAUSED", PresentationFactRole::Warning),
            "offline" => ("OFFLINE", PresentationFactRole::Idle),
            _ => ("UNAVAILABLE", PresentationFactRole::Error),
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
}
pub(crate) fn remove() {
    {
        let host = global_presentation_fact_host_service();
        host.registry().remove_producer("bmux.broadcast");
    }
}
