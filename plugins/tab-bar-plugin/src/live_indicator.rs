//! Bounded projection of broadcast-owned retained facts. No service or network IO.
use bmux_presentation_state::{PresentationEntityRef, PresentationFactRegistry};

const PRODUCER: &str = "bmux.broadcast";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveIndicator {
    Live,
    Paused,
    Offline,
    Unavailable,
}

impl LiveIndicator {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::Paused => "PAUSED",
            Self::Offline => "OFFLINE",
            Self::Unavailable => "UNAVAILABLE",
        }
    }
}

pub fn read(registry: &PresentationFactRegistry) -> Option<LiveIndicator> {
    registry
        .facts_for_entity(&PresentationEntityRef::new(PRODUCER, "indicator"))
        .into_iter()
        .find(|(producer, fact)| producer == PRODUCER && fact.key == "status")
        .map(|(_, fact)| match fact.short_text.as_str() {
            "LIVE" => LiveIndicator::Live,
            "PAUSED" => LiveIndicator::Paused,
            "OFFLINE" => LiveIndicator::Offline,
            _ => LiveIndicator::Unavailable,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_presentation_state::{
        PresentationFact, PresentationFactRole, PresentationFactSnapshot,
    };

    fn publish(registry: &PresentationFactRegistry, producer: &str, revision: u64, label: &str) {
        registry
            .publish_authorized(
                producer,
                producer,
                PresentationFactSnapshot {
                    revision,
                    facts: vec![PresentationFact {
                        entity: PresentationEntityRef::new(PRODUCER, "indicator"),
                        key: "status".into(),
                        role: PresentationFactRole::Neutral,
                        short_text: label.into(),
                        detail_text: None,
                        icon_id: None,
                        priority: 0,
                    }],
                },
            )
            .unwrap();
    }

    #[test]
    fn absence_and_unrelated_producer_do_not_add_ui() {
        let registry = PresentationFactRegistry::new();
        assert_eq!(read(&registry), None);
        publish(&registry, "other.plugin", 1, "LIVE");
        assert_eq!(read(&registry), None);
    }

    #[test]
    fn all_states_are_bounded_and_unknown_is_unavailable() {
        let registry = PresentationFactRegistry::new();
        for (revision, (label, expected)) in [
            ("LIVE", LiveIndicator::Live),
            ("PAUSED", LiveIndicator::Paused),
            ("OFFLINE", LiveIndicator::Offline),
            ("STARTING", LiveIndicator::Unavailable),
            ("UNAVAILABLE", LiveIndicator::Unavailable),
            ("unexpected private diagnostic", LiveIndicator::Unavailable),
        ]
        .into_iter()
        .enumerate()
        {
            publish(
                &registry,
                PRODUCER,
                u64::try_from(revision).unwrap() + 1,
                label,
            );
            assert_eq!(read(&registry), Some(expected));
            assert!(expected.label().len() <= 11);
        }
    }

    #[test]
    fn latest_snapshot_wins_and_removal_invalidates_live() {
        let registry = PresentationFactRegistry::new();
        publish(&registry, PRODUCER, 2, "LIVE");
        publish(&registry, PRODUCER, 3, "OFFLINE");
        // Older producer updates cannot revive live output.
        publish(&registry, PRODUCER, 1, "LIVE");
        assert_eq!(read(&registry), Some(LiveIndicator::Offline));
        registry.remove_producer(PRODUCER);
        assert_eq!(read(&registry), None);
        // A new owner lifecycle may restart its producer revision.
        publish(&registry, PRODUCER, 1, "PAUSED");
        assert_eq!(read(&registry), Some(LiveIndicator::Paused));
        publish(&registry, PRODUCER, 2, "UNAVAILABLE");
        assert_eq!(read(&registry), Some(LiveIndicator::Unavailable));
    }
}
