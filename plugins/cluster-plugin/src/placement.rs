//! Deterministic worker placement and inspectable candidate ranking.

use crate::membership::NodeId;
use bmux_cluster_plugin_api::cluster_types::{PlacementIntent, PlacementLabel};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementHealth {
    Healthy,
    Degraded,
    Unknown,
    Unavailable,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementMembership {
    ActiveCompatibleWorker,
    Inactive,
    Incompatible,
    NotWorker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementMaintenance {
    Available,
    Cordoned,
    Draining,
    Drained,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementCandidate {
    pub node_id: NodeId,
    pub membership: PlacementMembership,
    pub maintenance: PlacementMaintenance,
    pub labels: BTreeMap<String, String>,
    pub capacity_used: Option<u64>,
    pub capacity_total: Option<u64>,
    pub health: PlacementHealth,
    pub locality_rank: u32,
    pub spread_conflicts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequest {
    pub intent: PlacementIntent,
    pub current_node_id: Option<NodeId>,
    pub preserve_current: bool,
    pub spread: bool,
    pub required_capacity: u64,
    pub observation_epoch: u64,
    pub control_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlacementRank {
    preserve_current: u8,
    preferred_label_misses: u32,
    spread_conflicts: u32,
    health: u8,
    capacity_pressure_basis_points: u16,
    locality: u32,
    node_id: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementRejection {
    Inactive,
    Incompatible,
    NotWorker,
    Drained,
    Draining,
    Cordoned,
    ExplicitNodeMismatch,
    RequiredLabelMismatch { key: String },
    InsufficientCapacity,
    CapacityUnknown,
    Unavailable,
    HealthUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementCandidateExplanation {
    pub node_id: NodeId,
    pub rank: Option<PlacementRank>,
    pub rejection: Option<PlacementRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementDecision {
    pub control_revision: u64,
    pub observation_epoch: u64,
    pub selected_node_id: Option<NodeId>,
    pub candidates: Vec<PlacementCandidateExplanation>,
}

#[must_use]
pub fn choose_worker(
    request: &PlacementRequest,
    candidates: impl IntoIterator<Item = PlacementCandidate>,
) -> PlacementDecision {
    let explicit = request.intent.explicit_node_id.as_deref();
    let mut explanations = candidates
        .into_iter()
        .map(|candidate| {
            let rejection = reject_candidate(request, &candidate, explicit);
            let rank = rejection
                .is_none()
                .then(|| candidate_rank(request, &candidate));
            PlacementCandidateExplanation {
                node_id: candidate.node_id,
                rank,
                rejection,
            }
        })
        .collect::<Vec<_>>();
    explanations.sort_by(|left, right| {
        left.rank
            .as_ref()
            .cmp(&right.rank.as_ref())
            .reverse()
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let selected_node_id = explanations
        .iter()
        .filter_map(|candidate| {
            candidate
                .rank
                .as_ref()
                .map(|rank| (rank, candidate.node_id))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, node_id)| node_id);
    PlacementDecision {
        control_revision: request.control_revision,
        observation_epoch: request.observation_epoch,
        selected_node_id,
        candidates: explanations,
    }
}

fn reject_candidate(
    request: &PlacementRequest,
    candidate: &PlacementCandidate,
    explicit: Option<&str>,
) -> Option<PlacementRejection> {
    match candidate.membership {
        PlacementMembership::Inactive => return Some(PlacementRejection::Inactive),
        PlacementMembership::Incompatible => return Some(PlacementRejection::Incompatible),
        PlacementMembership::NotWorker => return Some(PlacementRejection::NotWorker),
        PlacementMembership::ActiveCompatibleWorker => {}
    }
    let preserving_current =
        request.preserve_current && request.current_node_id == Some(candidate.node_id);
    match candidate.maintenance {
        PlacementMaintenance::Drained => return Some(PlacementRejection::Drained),
        PlacementMaintenance::Draining => return Some(PlacementRejection::Draining),
        PlacementMaintenance::Cordoned if !preserving_current => {
            return Some(PlacementRejection::Cordoned);
        }
        PlacementMaintenance::Available | PlacementMaintenance::Cordoned => {}
    }
    if explicit.is_some_and(|expected| expected != candidate.node_id.to_string()) {
        return Some(PlacementRejection::ExplicitNodeMismatch);
    }
    if let Some(label) = request
        .intent
        .required_labels
        .iter()
        .find(|label| !matches_label(&candidate.labels, label))
    {
        return Some(PlacementRejection::RequiredLabelMismatch {
            key: label.key.clone(),
        });
    }
    match (candidate.capacity_used, candidate.capacity_total) {
        (Some(used), Some(total)) if total.saturating_sub(used) >= request.required_capacity => {}
        (Some(_), Some(_)) => return Some(PlacementRejection::InsufficientCapacity),
        _ if explicit.is_none() => return Some(PlacementRejection::CapacityUnknown),
        _ => {}
    }
    match candidate.health {
        PlacementHealth::Unavailable => return Some(PlacementRejection::Unavailable),
        PlacementHealth::Unknown if explicit.is_none() => {
            return Some(PlacementRejection::HealthUnknown);
        }
        PlacementHealth::Healthy | PlacementHealth::Degraded | PlacementHealth::Unknown => {}
        PlacementHealth::Incompatible => return Some(PlacementRejection::Incompatible),
    }
    None
}

fn candidate_rank(request: &PlacementRequest, candidate: &PlacementCandidate) -> PlacementRank {
    let preferred_label_misses = request
        .intent
        .preferred_labels
        .iter()
        .filter(|label| !matches_label(&candidate.labels, label))
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    let health = match candidate.health {
        PlacementHealth::Healthy => 0,
        PlacementHealth::Degraded => 1,
        PlacementHealth::Unknown => 2,
        PlacementHealth::Unavailable | PlacementHealth::Incompatible => u8::MAX,
    };
    PlacementRank {
        preserve_current: u8::from(
            !request.preserve_current || request.current_node_id != Some(candidate.node_id),
        ),
        preferred_label_misses,
        spread_conflicts: if request.spread {
            candidate.spread_conflicts
        } else {
            0
        },
        health,
        capacity_pressure_basis_points: capacity_pressure(candidate),
        locality: candidate.locality_rank,
        node_id: *candidate.node_id.as_bytes(),
    }
}

fn capacity_pressure(candidate: &PlacementCandidate) -> u16 {
    match (candidate.capacity_used, candidate.capacity_total) {
        (Some(used), Some(total)) if total > 0 => used
            .saturating_mul(10_000)
            .checked_div(total)
            .unwrap_or(u64::MAX)
            .min(10_000)
            .try_into()
            .unwrap_or(10_000),
        _ => u16::MAX,
    }
}

fn matches_label(labels: &BTreeMap<String, String>, required: &PlacementLabel) -> bool {
    labels.get(&required.key) == Some(&required.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: u64) -> PlacementCandidate {
        PlacementCandidate {
            node_id: NodeId::from(id),
            membership: PlacementMembership::ActiveCompatibleWorker,
            maintenance: PlacementMaintenance::Available,
            labels: BTreeMap::from([
                ("region".to_string(), "west".to_string()),
                ("disk".to_string(), "ssd".to_string()),
            ]),
            capacity_used: Some(10),
            capacity_total: Some(100),
            health: PlacementHealth::Healthy,
            locality_rank: 0,
            spread_conflicts: 0,
        }
    }

    fn request() -> PlacementRequest {
        PlacementRequest {
            intent: PlacementIntent {
                explicit_node_id: None,
                required_labels: vec![PlacementLabel {
                    key: "region".to_string(),
                    value: "west".to_string(),
                }],
                preferred_labels: vec![PlacementLabel {
                    key: "disk".to_string(),
                    value: "ssd".to_string(),
                }],
            },
            current_node_id: None,
            preserve_current: true,
            spread: true,
            required_capacity: 1,
            observation_epoch: 8,
            control_revision: 9,
        }
    }

    #[test]
    fn candidate_permutations_produce_identical_decisions() {
        let candidates = vec![candidate(3), candidate(1), candidate(2)];
        let forward = choose_worker(&request(), candidates.clone());
        let reverse = choose_worker(&request(), candidates.into_iter().rev());
        assert_eq!(forward, reverse);
        assert_eq!(forward.selected_node_id, Some(NodeId::from(1)));
        assert_eq!(forward.control_revision, 9);
        assert_eq!(forward.observation_epoch, 8);
    }

    #[test]
    fn hard_filters_cannot_be_bypassed_by_explicit_selection() {
        let mut request = request();
        request.intent.explicit_node_id = Some(NodeId::from(1).to_string());
        for rejection in [
            PlacementRejection::Inactive,
            PlacementRejection::Incompatible,
            PlacementRejection::NotWorker,
            PlacementRejection::Drained,
            PlacementRejection::Draining,
            PlacementRejection::RequiredLabelMismatch {
                key: "region".to_string(),
            },
            PlacementRejection::InsufficientCapacity,
            PlacementRejection::Unavailable,
        ] {
            let mut value = candidate(1);
            match rejection {
                PlacementRejection::Inactive => value.membership = PlacementMembership::Inactive,
                PlacementRejection::Incompatible => {
                    value.membership = PlacementMembership::Incompatible;
                }
                PlacementRejection::NotWorker => {
                    value.membership = PlacementMembership::NotWorker;
                }
                PlacementRejection::Drained => {
                    value.maintenance = PlacementMaintenance::Drained;
                }
                PlacementRejection::Draining => {
                    value.maintenance = PlacementMaintenance::Draining;
                }
                PlacementRejection::RequiredLabelMismatch { .. } => {
                    value.labels.remove("region");
                }
                PlacementRejection::InsufficientCapacity => value.capacity_used = Some(100),
                PlacementRejection::Unavailable => value.health = PlacementHealth::Unavailable,
                _ => unreachable!(),
            }
            let decision = choose_worker(&request, [value]);
            assert_eq!(decision.selected_node_id, None);
            assert_eq!(decision.candidates[0].rejection, Some(rejection));
        }
    }

    #[test]
    fn eligible_current_node_is_stable_until_move_is_requested() {
        let mut request = request();
        request.current_node_id = Some(NodeId::from(2));
        let candidates = [candidate(1), candidate(2)];
        assert_eq!(
            choose_worker(&request, candidates.clone()).selected_node_id,
            Some(NodeId::from(2))
        );
        request.preserve_current = false;
        assert_eq!(
            choose_worker(&request, candidates).selected_node_id,
            Some(NodeId::from(1))
        );
    }

    #[test]
    fn ranking_applies_labels_spread_health_capacity_and_locality_in_order() {
        let request = request();
        let mut preferred_miss = candidate(1);
        preferred_miss.labels.remove("disk");
        let mut spread = candidate(2);
        spread.spread_conflicts = 1;
        let mut degraded = candidate(3);
        degraded.health = PlacementHealth::Degraded;
        let mut pressure = candidate(4);
        pressure.capacity_used = Some(90);
        let mut remote = candidate(5);
        remote.locality_rank = 1;
        let best = candidate(6);
        let decision = choose_worker(
            &request,
            [preferred_miss, spread, degraded, pressure, remote, best],
        );
        assert_eq!(decision.selected_node_id, Some(NodeId::from(6)));
    }

    #[test]
    fn unknown_observations_require_explicit_selection() {
        let mut value = candidate(1);
        value.health = PlacementHealth::Unknown;
        value.capacity_used = None;
        value.capacity_total = None;
        let automatic = choose_worker(&request(), [value.clone()]);
        assert_eq!(automatic.selected_node_id, None);

        let mut explicit = request();
        explicit.intent.explicit_node_id = Some(value.node_id.to_string());
        assert_eq!(
            choose_worker(&explicit, [value]).selected_node_id,
            Some(NodeId::from(1))
        );
    }

    #[test]
    fn cordon_allows_only_preserving_current_execution() {
        let mut value = candidate(1);
        value.maintenance = PlacementMaintenance::Cordoned;
        let mut request = request();
        assert_eq!(
            choose_worker(&request, [value.clone()]).selected_node_id,
            None
        );
        request.current_node_id = Some(value.node_id);
        assert_eq!(
            choose_worker(&request, [value]).selected_node_id,
            Some(NodeId::from(1))
        );
    }
}
