#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Neutral, bounded semantic facts for retained presentation projection.
//!
//! Entity identity remains namespaced by the domain plugin that owns it. This
//! crate owns only lifecycle-safe primitives and registry mechanics; it does
//! not know what a window, pane, session, agent, or client is.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, RwLock};

pub const DEFAULT_MAXIMUM_PRODUCERS: usize = 64;
pub const DEFAULT_MAXIMUM_FACTS_PER_PRODUCER: usize = 1_024;
pub const DEFAULT_MAXIMUM_SHORT_TEXT_BYTES: usize = 256;
pub const DEFAULT_MAXIMUM_DETAIL_TEXT_BYTES: usize = 4_096;
pub const DEFAULT_MAXIMUM_ICON_ID_BYTES: usize = 128;

/// Stable capability name for host-routed semantic fact publication.
pub const PRESENTATION_FACTS_PUBLISH_CAPABILITY: &str = "bmux.presentation_facts.publish";
/// Stable typed interface name for producer snapshot replacement/removal.
pub const PRESENTATION_FACTS_INTERFACE: &str = "presentation-facts";

static GLOBAL_PRESENTATION_FACT_HOST_SERVICE: OnceLock<PresentationFactHostService> =
    OnceLock::new();

#[must_use]
pub fn global_presentation_fact_host_service() -> &'static PresentationFactHostService {
    GLOBAL_PRESENTATION_FACT_HOST_SERVICE.get_or_init(PresentationFactHostService::new)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishPresentationFactsRequest {
    pub producer_id: String,
    pub snapshot: PresentationFactSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovePresentationFactsRequest {
    pub producer_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationFactPublishAck {
    Applied,
    Unchanged,
    Stale,
    Removed,
    NotFound,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct PresentationEntityRef {
    pub namespace: String,
    pub id: String,
}

impl PresentationEntityRef {
    #[must_use]
    pub fn new(namespace: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationFactRole {
    Neutral,
    Idle,
    Active,
    Success,
    Warning,
    Attention,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresentationFact {
    pub entity: PresentationEntityRef,
    pub key: String,
    pub role: PresentationFactRole,
    pub short_text: String,
    pub detail_text: Option<String>,
    pub icon_id: Option<String>,
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresentationFactSnapshot {
    pub revision: u64,
    pub facts: Vec<PresentationFact>,
}

/// Domain-supplied relationship used by presentation plugins to aggregate
/// child facts into a parent card. The neutral registry stores no relationship
/// graph and applies no aggregation policy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresentationEntityRelation {
    pub parent: PresentationEntityRef,
    pub child: PresentationEntityRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationFactAggregation {
    /// Return the deterministic highest-priority fact from all related children.
    HighestPriority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationFactChange {
    pub revision: u64,
    pub affected_entities: BTreeSet<PresentationEntityRef>,
}

impl PresentationFactChange {
    const fn initial() -> Self {
        Self {
            revision: 0,
            affected_entities: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationFactPublishOutcome {
    Applied,
    Unchanged,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationFactPublishError {
    EmptyProducer,
    UnauthorizedProducer {
        producer_id: String,
    },
    EmptyEntityNamespace,
    EmptyEntityId,
    EmptyKey,
    TooManyProducers {
        count: usize,
        maximum: usize,
    },
    TooManyFacts {
        count: usize,
        maximum: usize,
    },
    DuplicateFact {
        entity: PresentationEntityRef,
        key: String,
    },
    ShortTextTooLong {
        bytes: usize,
        maximum: usize,
    },
    DetailTextTooLong {
        bytes: usize,
        maximum: usize,
    },
    IconIdTooLong {
        bytes: usize,
        maximum: usize,
    },
    TerminalControlData,
    ConflictingRevision,
}

impl std::fmt::Display for PresentationFactPublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProducer => {
                formatter.write_str("presentation fact producer must not be empty")
            }
            Self::UnauthorizedProducer { producer_id } => write!(
                formatter,
                "presentation fact producer '{producer_id}' is not authorized"
            ),
            Self::EmptyEntityNamespace => {
                formatter.write_str("presentation fact entity namespace must not be empty")
            }
            Self::EmptyEntityId => {
                formatter.write_str("presentation fact entity id must not be empty")
            }
            Self::EmptyKey => formatter.write_str("presentation fact key must not be empty"),
            Self::TooManyProducers { count, maximum } => write!(
                formatter,
                "presentation fact registry has {count} producers; maximum is {maximum}"
            ),
            Self::TooManyFacts { count, maximum } => write!(
                formatter,
                "presentation fact snapshot has {count} facts; maximum is {maximum}"
            ),
            Self::DuplicateFact { entity, key } => write!(
                formatter,
                "presentation fact '{}:{}:{key}' is duplicated",
                entity.namespace, entity.id
            ),
            Self::ShortTextTooLong { bytes, maximum } => write!(
                formatter,
                "presentation fact short text is {bytes} bytes; maximum is {maximum}"
            ),
            Self::DetailTextTooLong { bytes, maximum } => write!(
                formatter,
                "presentation fact detail text is {bytes} bytes; maximum is {maximum}"
            ),
            Self::IconIdTooLong { bytes, maximum } => write!(
                formatter,
                "presentation fact icon id is {bytes} bytes; maximum is {maximum}"
            ),
            Self::TerminalControlData => {
                formatter.write_str("presentation facts must not contain terminal control data")
            }
            Self::ConflictingRevision => formatter
                .write_str("presentation fact revision conflicts with retained producer state"),
        }
    }
}

impl std::error::Error for PresentationFactPublishError {}

#[derive(Debug)]
pub struct PresentationFactRegistry {
    maximum_producers: usize,
    maximum_facts_per_producer: usize,
    maximum_short_text_bytes: usize,
    maximum_detail_text_bytes: usize,
    maximum_icon_id_bytes: usize,
    snapshots: RwLock<BTreeMap<String, PresentationFactSnapshot>>,
    revision_tx: tokio::sync::watch::Sender<u64>,
    change_tx: tokio::sync::watch::Sender<PresentationFactChange>,
}

/// Owner-bound producer handle. Dropping the handle removes the producer's
/// complete retained snapshot, making plugin unload cleanup the default.
#[derive(Debug)]
pub struct PresentationFactPublisher<'a> {
    registry: &'a PresentationFactRegistry,
    producer_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationEntityProjection {
    pub entity: PresentationEntityRef,
    pub facts: Vec<(String, PresentationFact)>,
}

/// Minimal projection view consumed by presentation plugins. It tracks only
/// retained registry revision and deterministic entity queries; product
/// rendering and filtering policy remain in the consumer.
#[derive(Debug)]
pub struct PresentationFactConsumer<'a> {
    registry: &'a PresentationFactRegistry,
    revision_rx: tokio::sync::watch::Receiver<u64>,
}

impl PresentationFactConsumer<'_> {
    #[must_use]
    pub fn revision(&self) -> u64 {
        *self.revision_rx.borrow()
    }

    pub fn acknowledge_revision(&mut self) -> u64 {
        *self.revision_rx.borrow_and_update()
    }

    #[must_use]
    pub fn facts_for_entity(
        &self,
        entity: &PresentationEntityRef,
    ) -> Vec<(String, PresentationFact)> {
        self.registry.facts_for_entity(entity)
    }

    #[must_use]
    pub fn project_entities(
        &self,
        entities: impl IntoIterator<Item = PresentationEntityRef>,
    ) -> Vec<PresentationEntityProjection> {
        entities
            .into_iter()
            .map(|entity| PresentationEntityProjection {
                facts: self.facts_for_entity(&entity),
                entity,
            })
            .collect()
    }

    #[must_use]
    pub fn highest_related_fact(
        &self,
        parent: &PresentationEntityRef,
        relations: &[PresentationEntityRelation],
    ) -> Option<(String, PresentationFact)> {
        self.registry
            .aggregate_related_facts(
                parent,
                relations,
                PresentationFactAggregation::HighestPriority,
            )
            .into_iter()
            .next()
    }
}

impl PresentationFactPublisher<'_> {
    /// Replace this producer's complete retained snapshot.
    ///
    /// # Errors
    /// Returns the registry's bounded validation and revision failures.
    pub fn publish(
        &self,
        snapshot: PresentationFactSnapshot,
    ) -> Result<PresentationFactPublishOutcome, PresentationFactPublishError> {
        self.registry
            .publish_authorized(&self.producer_id, &self.producer_id, snapshot)
    }
}

impl Drop for PresentationFactPublisher<'_> {
    fn drop(&mut self) {
        self.registry.remove_producer(&self.producer_id);
    }
}

impl Default for PresentationFactRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationFactRegistry {
    #[must_use]
    pub fn new() -> Self {
        let (revision_tx, _) = tokio::sync::watch::channel(0);
        let (change_tx, _) = tokio::sync::watch::channel(PresentationFactChange::initial());
        Self {
            maximum_producers: DEFAULT_MAXIMUM_PRODUCERS,
            maximum_facts_per_producer: DEFAULT_MAXIMUM_FACTS_PER_PRODUCER,
            maximum_short_text_bytes: DEFAULT_MAXIMUM_SHORT_TEXT_BYTES,
            maximum_detail_text_bytes: DEFAULT_MAXIMUM_DETAIL_TEXT_BYTES,
            maximum_icon_id_bytes: DEFAULT_MAXIMUM_ICON_ID_BYTES,
            snapshots: RwLock::new(BTreeMap::new()),
            revision_tx,
            change_tx,
        }
    }

    #[must_use]
    pub fn consumer(&self) -> PresentationFactConsumer<'_> {
        PresentationFactConsumer {
            registry: self,
            revision_rx: self.subscribe(),
        }
    }

    #[must_use]
    pub fn publisher(&self, producer_id: impl Into<String>) -> PresentationFactPublisher<'_> {
        PresentationFactPublisher {
            registry: self,
            producer_id: producer_id.into(),
        }
    }

    /// Replace one authorized producer's complete retained fact snapshot.
    ///
    /// Authorization is supplied by the host/plugin capability boundary. The
    /// registry verifies the authorized identity matches the snapshot owner so
    /// callers cannot publish or remove another producer's retained state.
    ///
    /// # Errors
    /// Returns authorization, bounded-content, duplicate-identity,
    /// ownership-count, lock, and conflicting-revision failures.
    pub fn publish_authorized(
        &self,
        authorized_producer_id: &str,
        producer_id: &str,
        snapshot: PresentationFactSnapshot,
    ) -> Result<PresentationFactPublishOutcome, PresentationFactPublishError> {
        authorize_producer(authorized_producer_id, producer_id)?;
        self.publish(producer_id, snapshot)
    }

    /// Replace one producer's complete retained fact snapshot.
    ///
    /// Use this only within an already owner-bound trusted adapter. External
    /// publication paths must call [`Self::publish_authorized`].
    ///
    /// # Errors
    /// Returns bounded-content, duplicate-identity, ownership-count, lock, and
    /// conflicting-revision failures.
    pub fn publish(
        &self,
        producer_id: &str,
        snapshot: PresentationFactSnapshot,
    ) -> Result<PresentationFactPublishOutcome, PresentationFactPublishError> {
        validate_snapshot(
            producer_id,
            &snapshot,
            self.maximum_facts_per_producer,
            self.maximum_short_text_bytes,
            self.maximum_detail_text_bytes,
            self.maximum_icon_id_bytes,
        )?;
        let mut snapshots = self
            .snapshots
            .write()
            .map_err(|_| PresentationFactPublishError::ConflictingRevision)?;
        if !snapshots.contains_key(producer_id) && snapshots.len() >= self.maximum_producers {
            return Err(PresentationFactPublishError::TooManyProducers {
                count: snapshots.len().saturating_add(1),
                maximum: self.maximum_producers,
            });
        }
        if let Some(current) = snapshots.get(producer_id) {
            if snapshot.revision < current.revision {
                return Ok(PresentationFactPublishOutcome::Stale);
            }
            if snapshot.revision == current.revision {
                return if snapshot == *current {
                    Ok(PresentationFactPublishOutcome::Unchanged)
                } else {
                    Err(PresentationFactPublishError::ConflictingRevision)
                };
            }
        }
        let affected_entities = snapshots.get(producer_id).map_or_else(
            || changed_entities(&[], &snapshot.facts),
            |current| changed_entities(&current.facts, &snapshot.facts),
        );
        snapshots.insert(producer_id.to_string(), snapshot);
        drop(snapshots);
        let revision = {
            let mut next = 0;
            self.revision_tx.send_modify(|revision| {
                *revision = revision.wrapping_add(1);
                next = *revision;
            });
            next
        };
        self.change_tx.send_replace(PresentationFactChange {
            revision,
            affected_entities,
        });
        Ok(PresentationFactPublishOutcome::Applied)
    }

    /// Remove retained state after verifying producer ownership.
    ///
    /// # Errors
    /// Returns an authorization error when the caller is not the producer.
    pub fn remove_producer_authorized(
        &self,
        authorized_producer_id: &str,
        producer_id: &str,
    ) -> Result<bool, PresentationFactPublishError> {
        authorize_producer(authorized_producer_id, producer_id)?;
        Ok(self.remove_producer(producer_id))
    }

    pub fn remove_producer(&self, producer_id: &str) -> bool {
        let removed = self.snapshots.write().ok().and_then(|mut snapshots| {
            snapshots.remove(producer_id).map(|snapshot| {
                snapshot
                    .facts
                    .into_iter()
                    .map(|fact| fact.entity)
                    .collect::<BTreeSet<_>>()
            })
        });
        removed.is_some_and(|affected_entities| {
            let revision = {
                let mut next = 0;
                self.revision_tx.send_modify(|revision| {
                    *revision = revision.wrapping_add(1);
                    next = *revision;
                });
                next
            };
            self.change_tx.send_replace(PresentationFactChange {
                revision,
                affected_entities,
            });
            true
        })
    }

    #[must_use]
    pub fn facts_for_entity(
        &self,
        entity: &PresentationEntityRef,
    ) -> Vec<(String, PresentationFact)> {
        let snapshots = self.snapshots.read();
        let Ok(snapshots) = snapshots.as_ref() else {
            return Vec::new();
        };
        sorted_facts_for_entities(snapshots, std::slice::from_ref(entity))
    }

    /// Aggregate child-entity facts for one parent using explicit
    /// domain-supplied relationships. This keeps relationship ownership and
    /// parent-card policy out of the neutral registry.
    #[must_use]
    pub fn aggregate_related_facts(
        &self,
        parent: &PresentationEntityRef,
        relations: &[PresentationEntityRelation],
        aggregation: PresentationFactAggregation,
    ) -> Vec<(String, PresentationFact)> {
        let children = relations
            .iter()
            .filter(|relation| relation.parent == *parent)
            .map(|relation| relation.child.clone())
            .collect::<BTreeSet<_>>();
        let snapshots = self.snapshots.read();
        let Ok(snapshots) = snapshots.as_ref() else {
            return Vec::new();
        };
        let mut facts =
            sorted_facts_for_entities(snapshots, &children.into_iter().collect::<Vec<_>>());
        match aggregation {
            PresentationFactAggregation::HighestPriority => facts.truncate(1),
        }
        facts
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision_tx.subscribe()
    }

    #[must_use]
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<PresentationFactChange> {
        self.change_tx.subscribe()
    }

    /// Execute one typed publication operation using a capability-authenticated
    /// producer identity supplied by the host dispatch boundary.
    ///
    /// # Errors
    /// Returns unknown-operation, decode, encode, authorization, validation,
    /// or retained-registry failures.
    pub fn dispatch_typed(
        &self,
        authenticated_producer_id: &str,
        operation: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, PresentationFactDispatchError> {
        match operation {
            "publish" => {
                let request: PublishPresentationFactsRequest =
                    bmux_codec::from_typed_bytes(payload).map_err(|error| {
                        PresentationFactDispatchError::Decode(error.to_string())
                    })?;
                let outcome = self.publish_authorized(
                    authenticated_producer_id,
                    &request.producer_id,
                    request.snapshot,
                )?;
                let ack = match outcome {
                    PresentationFactPublishOutcome::Applied => PresentationFactPublishAck::Applied,
                    PresentationFactPublishOutcome::Unchanged => {
                        PresentationFactPublishAck::Unchanged
                    }
                    PresentationFactPublishOutcome::Stale => PresentationFactPublishAck::Stale,
                };
                bmux_codec::to_typed_vec(&ack)
                    .map_err(|error| PresentationFactDispatchError::Encode(error.to_string()))
            }
            "remove" => {
                let request: RemovePresentationFactsRequest = bmux_codec::from_typed_bytes(payload)
                    .map_err(|error| PresentationFactDispatchError::Decode(error.to_string()))?;
                let removed = self
                    .remove_producer_authorized(authenticated_producer_id, &request.producer_id)?;
                let ack = if removed {
                    PresentationFactPublishAck::Removed
                } else {
                    PresentationFactPublishAck::NotFound
                };
                bmux_codec::to_typed_vec(&ack)
                    .map_err(|error| PresentationFactDispatchError::Encode(error.to_string()))
            }
            other => Err(PresentationFactDispatchError::UnknownOperation(
                other.to_string(),
            )),
        }
    }
}

/// Host-side service adapter that binds typed dispatch to a neutral registry.
///
/// The host is responsible for deriving `authenticated_producer_id` from the
/// plugin invocation context; request payloads never choose their own identity.
#[derive(Debug)]
pub struct PresentationFactHostService {
    registry: PresentationFactRegistry,
}

impl Default for PresentationFactHostService {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationFactHostService {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: PresentationFactRegistry::new(),
        }
    }

    #[must_use]
    pub const fn registry(&self) -> &PresentationFactRegistry {
        &self.registry
    }

    /// Dispatch one capability-authenticated host service request.
    ///
    /// # Errors
    /// Returns typed transport, authorization, validation, or registry errors.
    pub fn dispatch(
        &self,
        authenticated_producer_id: &str,
        interface_id: &str,
        operation: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, PresentationFactDispatchError> {
        if interface_id != PRESENTATION_FACTS_INTERFACE {
            return Err(PresentationFactDispatchError::UnknownInterface(
                interface_id.to_string(),
            ));
        }
        self.registry
            .dispatch_typed(authenticated_producer_id, operation, payload)
    }
}

#[derive(Debug)]
pub enum PresentationFactDispatchError {
    UnknownInterface(String),
    UnknownOperation(String),
    Decode(String),
    Encode(String),
    Publish(PresentationFactPublishError),
}

impl From<PresentationFactPublishError> for PresentationFactDispatchError {
    fn from(error: PresentationFactPublishError) -> Self {
        Self::Publish(error)
    }
}

impl std::fmt::Display for PresentationFactDispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownInterface(interface) => {
                write!(
                    formatter,
                    "unknown presentation fact interface '{interface}'"
                )
            }
            Self::UnknownOperation(operation) => {
                write!(
                    formatter,
                    "unknown presentation fact operation '{operation}'"
                )
            }
            Self::Decode(error) => write!(formatter, "decoding presentation facts: {error}"),
            Self::Encode(error) => write!(formatter, "encoding presentation facts: {error}"),
            Self::Publish(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PresentationFactDispatchError {}

fn changed_entities(
    current: &[PresentationFact],
    next: &[PresentationFact],
) -> BTreeSet<PresentationEntityRef> {
    let current = current
        .iter()
        .map(|fact| ((&fact.entity, &fact.key), fact))
        .collect::<BTreeMap<_, _>>();
    let next = next
        .iter()
        .map(|fact| ((&fact.entity, &fact.key), fact))
        .collect::<BTreeMap<_, _>>();
    current
        .keys()
        .chain(next.keys())
        .filter(|identity| current.get(*identity) != next.get(*identity))
        .map(|identity| identity.0.clone())
        .collect()
}

fn contains_terminal_control(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(character, '\u{001b}' | '\u{009b}' | '\u{009d}')
            || (character.is_control() && !matches!(character, '\n' | '\t'))
    })
}

fn sorted_facts_for_entities(
    snapshots: &BTreeMap<String, PresentationFactSnapshot>,
    entities: &[PresentationEntityRef],
) -> Vec<(String, PresentationFact)> {
    let entities = entities.iter().collect::<BTreeSet<_>>();
    let mut facts = snapshots
        .iter()
        .flat_map(|(producer, snapshot)| {
            snapshot
                .facts
                .iter()
                .filter(|fact| entities.contains(&fact.entity))
                .cloned()
                .map(|fact| (producer.clone(), fact))
        })
        .collect::<Vec<_>>();
    facts.sort_by(|left, right| {
        right
            .1
            .priority
            .cmp(&left.1.priority)
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.1.key.cmp(&right.1.key))
    });
    facts
}

fn authorize_producer(
    authorized_producer_id: &str,
    producer_id: &str,
) -> Result<(), PresentationFactPublishError> {
    if authorized_producer_id != producer_id {
        return Err(PresentationFactPublishError::UnauthorizedProducer {
            producer_id: producer_id.to_string(),
        });
    }
    Ok(())
}

fn validate_snapshot(
    producer_id: &str,
    snapshot: &PresentationFactSnapshot,
    maximum_facts: usize,
    maximum_short_text_bytes: usize,
    maximum_detail_text_bytes: usize,
    maximum_icon_id_bytes: usize,
) -> Result<(), PresentationFactPublishError> {
    if producer_id.is_empty() {
        return Err(PresentationFactPublishError::EmptyProducer);
    }
    if snapshot.facts.len() > maximum_facts {
        return Err(PresentationFactPublishError::TooManyFacts {
            count: snapshot.facts.len(),
            maximum: maximum_facts,
        });
    }
    let mut identities = BTreeSet::new();
    for fact in &snapshot.facts {
        if fact.entity.namespace.is_empty() {
            return Err(PresentationFactPublishError::EmptyEntityNamespace);
        }
        if fact.entity.id.is_empty() {
            return Err(PresentationFactPublishError::EmptyEntityId);
        }
        if fact.key.is_empty() {
            return Err(PresentationFactPublishError::EmptyKey);
        }
        if !identities.insert((fact.entity.clone(), fact.key.clone())) {
            return Err(PresentationFactPublishError::DuplicateFact {
                entity: fact.entity.clone(),
                key: fact.key.clone(),
            });
        }
        if contains_terminal_control(&fact.short_text)
            || fact
                .detail_text
                .as_deref()
                .is_some_and(contains_terminal_control)
            || fact
                .icon_id
                .as_deref()
                .is_some_and(contains_terminal_control)
        {
            return Err(PresentationFactPublishError::TerminalControlData);
        }
        if fact.short_text.len() > maximum_short_text_bytes {
            return Err(PresentationFactPublishError::ShortTextTooLong {
                bytes: fact.short_text.len(),
                maximum: maximum_short_text_bytes,
            });
        }
        if let Some(detail) = &fact.detail_text
            && detail.len() > maximum_detail_text_bytes
        {
            return Err(PresentationFactPublishError::DetailTextTooLong {
                bytes: detail.len(),
                maximum: maximum_detail_text_bytes,
            });
        }
        if let Some(icon_id) = &fact.icon_id
            && icon_id.len() > maximum_icon_id_bytes
        {
            return Err(PresentationFactPublishError::IconIdTooLong {
                bytes: icon_id.len(),
                maximum: maximum_icon_id_bytes,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(entity: PresentationEntityRef, key: &str, priority: i32) -> PresentationFact {
        PresentationFact {
            entity,
            key: key.to_string(),
            role: PresentationFactRole::Active,
            short_text: "working".to_string(),
            detail_text: Some("bounded detail".to_string()),
            icon_id: Some("activity.working".to_string()),
            priority,
        }
    }

    #[test]
    fn change_notifications_name_only_affected_entities() {
        let registry = PresentationFactRegistry::new();
        let entity_a = PresentationEntityRef::new("example.tasks", "task-a");
        let entity_b = PresentationEntityRef::new("example.tasks", "task-b");
        let mut changes = registry.subscribe_changes();
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![
                        fact(entity_a.clone(), "state", 1),
                        fact(entity_b.clone(), "state", 1),
                    ],
                },
            )
            .unwrap();
        assert_eq!(changes.borrow_and_update().affected_entities.len(), 2);

        let mut changed_fact = fact(entity_a.clone(), "state", 1);
        changed_fact.short_text = "changed".to_string();
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 2,
                    facts: vec![changed_fact, fact(entity_b, "state", 1)],
                },
            )
            .unwrap();
        let change = changes.borrow_and_update().clone();
        assert_eq!(change.affected_entities, BTreeSet::from([entity_a]));
    }

    #[test]
    fn consumer_projection_preserves_requested_order_and_empty_entities() {
        let registry = PresentationFactRegistry::new();
        let entity_a = PresentationEntityRef::new("example.tasks", "task-a");
        let entity_b = PresentationEntityRef::new("example.tasks", "task-b");
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity_b.clone(), "state", 1)],
                },
            )
            .unwrap();
        let projection = registry
            .consumer()
            .project_entities([entity_a.clone(), entity_b.clone()]);
        assert_eq!(projection[0].entity, entity_a);
        assert!(projection[0].facts.is_empty());
        assert_eq!(projection[1].entity, entity_b);
        assert_eq!(projection[1].facts.len(), 1);
    }

    #[test]
    fn consumer_observes_incremental_revision_and_entity_queries() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        let mut consumer = registry.consumer();
        assert_eq!(consumer.revision(), 0);
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 1)],
                },
            )
            .unwrap();
        assert!(consumer.revision() > 0);
        assert_eq!(consumer.facts_for_entity(&entity).len(), 1);
        let acknowledged = consumer.acknowledge_revision();
        assert_eq!(acknowledged, consumer.revision());

        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 2,
                    facts: Vec::new(),
                },
            )
            .unwrap();
        assert!(consumer.revision() > acknowledged);
        assert!(consumer.facts_for_entity(&entity).is_empty());
    }

    #[test]
    fn global_host_service_is_stable() {
        assert!(std::ptr::eq(
            global_presentation_fact_host_service(),
            global_presentation_fact_host_service()
        ));
    }

    #[test]
    fn host_service_binds_interface_and_authenticated_producer() {
        let service = PresentationFactHostService::new();
        let request = PublishPresentationFactsRequest {
            producer_id: "producer.a".to_string(),
            snapshot: PresentationFactSnapshot {
                revision: 1,
                facts: vec![fact(
                    PresentationEntityRef::new("example.tasks", "task-1"),
                    "state",
                    1,
                )],
            },
        };
        let payload = bmux_codec::to_typed_vec(&request).unwrap();
        assert!(matches!(
            service.dispatch("producer.a", "wrong-interface", "publish", &payload),
            Err(PresentationFactDispatchError::UnknownInterface(_))
        ));
        assert!(
            service
                .dispatch(
                    "producer.a",
                    PRESENTATION_FACTS_INTERFACE,
                    "publish",
                    &payload,
                )
                .is_ok()
        );
    }

    #[test]
    fn generic_schema_expresses_working_and_input_required_without_domain_changes() {
        let entity = PresentationEntityRef::new("example.work", "job-1");
        let working = PresentationFact {
            entity: entity.clone(),
            key: "activity".to_string(),
            role: PresentationFactRole::Active,
            short_text: "working".to_string(),
            detail_text: Some("processing request".to_string()),
            icon_id: Some("activity.working".to_string()),
            priority: 10,
        };
        let input_required = PresentationFact {
            entity,
            key: "activity".to_string(),
            role: PresentationFactRole::Attention,
            short_text: "input required".to_string(),
            detail_text: Some("waiting for confirmation".to_string()),
            icon_id: Some("activity.input_required".to_string()),
            priority: 20,
        };
        let registry = PresentationFactRegistry::new();
        assert_eq!(
            registry.publish(
                "example.integration",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![working]
                }
            ),
            Ok(PresentationFactPublishOutcome::Applied)
        );
        assert_eq!(
            registry.publish(
                "example.integration",
                PresentationFactSnapshot {
                    revision: 2,
                    facts: vec![input_required]
                }
            ),
            Ok(PresentationFactPublishOutcome::Applied)
        );
    }

    #[test]
    fn multiple_producers_conflicts_entity_deletion_and_unload_are_consistent() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        let other = PresentationEntityRef::new("example.tasks", "task-2");
        let publisher_a = registry.publisher("producer.a");
        publisher_a
            .publish(PresentationFactSnapshot {
                revision: 1,
                facts: vec![
                    fact(entity.clone(), "state", 5),
                    fact(other.clone(), "state", 1),
                ],
            })
            .unwrap();
        registry
            .publish(
                "producer.b",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 7)],
                },
            )
            .unwrap();

        let facts = registry.facts_for_entity(&entity);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].0, "producer.b");
        assert_eq!(facts[0].1.priority, 7);

        publisher_a
            .publish(PresentationFactSnapshot {
                revision: 2,
                facts: vec![fact(other.clone(), "state", 1)],
            })
            .unwrap();
        assert_eq!(registry.facts_for_entity(&entity).len(), 1);
        assert_eq!(registry.facts_for_entity(&other).len(), 1);

        assert!(registry.remove_producer("producer.b"));
        assert!(registry.facts_for_entity(&entity).is_empty());
        drop(publisher_a);
        assert!(registry.facts_for_entity(&other).is_empty());
    }

    #[test]
    fn owner_bound_publisher_removes_facts_on_drop() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        {
            let publisher = registry.publisher("producer.a");
            publisher
                .publish(PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 1)],
                })
                .unwrap();
            assert_eq!(registry.facts_for_entity(&entity).len(), 1);
        }
        assert!(registry.facts_for_entity(&entity).is_empty());
    }

    #[test]
    #[ignore = "manual fact-update baseline; run with --release --ignored --nocapture"]
    fn fact_update_performance_baseline() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITERATIONS: u32 = 100_000;
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 1)],
                },
            )
            .unwrap();
        let started = Instant::now();
        for revision in 2..u64::from(ITERATIONS) + 2 {
            black_box(
                registry
                    .publish(
                        "producer.a",
                        PresentationFactSnapshot {
                            revision,
                            facts: vec![fact(entity.clone(), "state", 1)],
                        },
                    )
                    .unwrap(),
            );
        }
        let one_fact_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);

        let high_frequency = PresentationFactRegistry::new();
        let started = Instant::now();
        for revision in 1..=u64::from(ITERATIONS) {
            let facts = (0..32)
                .map(|index| {
                    fact(
                        PresentationEntityRef::new("example.tasks", format!("task-{index}")),
                        "state",
                        index,
                    )
                })
                .collect();
            black_box(
                high_frequency
                    .publish("producer.a", PresentationFactSnapshot { revision, facts })
                    .unwrap(),
            );
        }
        let bounded_batch_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);
        println!(
            "presentation facts iterations={ITERATIONS} one_fact_average_ns={one_fact_ns} bounded_32_fact_average_ns={bounded_batch_ns}"
        );
    }

    #[test]
    fn related_child_facts_are_aggregated_by_domain_supplied_relationships() {
        let registry = PresentationFactRegistry::new();
        let parent = PresentationEntityRef::new("example.groups", "group-1");
        let child_a = PresentationEntityRef::new("example.tasks", "task-a");
        let child_b = PresentationEntityRef::new("example.tasks", "task-b");
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![
                        fact(child_a.clone(), "state", 2),
                        fact(child_b.clone(), "state", 7),
                    ],
                },
            )
            .unwrap();
        let relations = vec![
            PresentationEntityRelation {
                parent: parent.clone(),
                child: child_a,
            },
            PresentationEntityRelation {
                parent: parent.clone(),
                child: child_b.clone(),
            },
            PresentationEntityRelation {
                parent: PresentationEntityRef::new("example.groups", "other"),
                child: child_b,
            },
        ];
        let facts = registry.aggregate_related_facts(
            &parent,
            &relations,
            PresentationFactAggregation::HighestPriority,
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1.entity.id, "task-b");
        assert_eq!(facts[0].1.priority, 7);
    }

    #[test]
    fn typed_dispatch_publishes_and_removes_authenticated_snapshots() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        let publish = PublishPresentationFactsRequest {
            producer_id: "producer.a".to_string(),
            snapshot: PresentationFactSnapshot {
                revision: 1,
                facts: vec![fact(entity.clone(), "state", 1)],
            },
        };
        let payload = bmux_codec::to_typed_vec(&publish).unwrap();
        let response = registry
            .dispatch_typed("producer.a", "publish", &payload)
            .unwrap();
        assert_eq!(
            bmux_codec::from_typed_bytes::<PresentationFactPublishAck>(&response).unwrap(),
            PresentationFactPublishAck::Applied
        );
        assert_eq!(registry.facts_for_entity(&entity).len(), 1);

        let denied = registry.dispatch_typed("producer.b", "publish", &payload);
        assert!(matches!(
            denied,
            Err(PresentationFactDispatchError::Publish(
                PresentationFactPublishError::UnauthorizedProducer { .. }
            ))
        ));

        let remove = bmux_codec::to_typed_vec(&RemovePresentationFactsRequest {
            producer_id: "producer.a".to_string(),
        })
        .unwrap();
        let response = registry
            .dispatch_typed("producer.a", "remove", &remove)
            .unwrap();
        assert_eq!(
            bmux_codec::from_typed_bytes::<PresentationFactPublishAck>(&response).unwrap(),
            PresentationFactPublishAck::Removed
        );
        assert!(registry.facts_for_entity(&entity).is_empty());
    }

    #[test]
    fn authorization_prevents_cross_producer_publication_and_removal() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        let snapshot = PresentationFactSnapshot {
            revision: 1,
            facts: vec![fact(entity, "state", 1)],
        };
        assert!(matches!(
            registry.publish_authorized("producer.a", "producer.b", snapshot.clone()),
            Err(PresentationFactPublishError::UnauthorizedProducer { .. })
        ));
        assert_eq!(
            registry.publish_authorized("producer.a", "producer.a", snapshot),
            Ok(PresentationFactPublishOutcome::Applied)
        );
        assert!(matches!(
            registry.remove_producer_authorized("producer.b", "producer.a"),
            Err(PresentationFactPublishError::UnauthorizedProducer { .. })
        ));
        assert_eq!(
            registry.remove_producer_authorized("producer.a", "producer.a"),
            Ok(true)
        );
    }

    #[test]
    fn replacement_revision_and_removal_are_retained() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        assert_eq!(
            registry.publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 1)]
                }
            ),
            Ok(PresentationFactPublishOutcome::Applied)
        );
        assert_eq!(
            registry.publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 0,
                    facts: Vec::new()
                }
            ),
            Ok(PresentationFactPublishOutcome::Stale)
        );
        assert_eq!(registry.facts_for_entity(&entity).len(), 1);
        assert!(registry.remove_producer("producer.a"));
        assert!(registry.facts_for_entity(&entity).is_empty());
    }

    #[test]
    fn priority_then_producer_then_key_is_deterministic() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        registry
            .publish(
                "producer.z",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "state", 5)],
                },
            )
            .unwrap();
        registry
            .publish(
                "producer.a",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![fact(entity.clone(), "activity", 5)],
                },
            )
            .unwrap();
        let facts = registry.facts_for_entity(&entity);
        assert_eq!(facts[0].0, "producer.a");
        assert_eq!(facts[1].0, "producer.z");
    }

    #[test]
    fn malformed_duplicate_and_oversized_facts_are_rejected() {
        let registry = PresentationFactRegistry::new();
        let entity = PresentationEntityRef::new("example.tasks", "task-1");
        let duplicate = fact(entity.clone(), "state", 1);
        assert!(matches!(
            registry.publish(
                "producer",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![duplicate.clone(), duplicate]
                }
            ),
            Err(PresentationFactPublishError::DuplicateFact { .. })
        ));
        let mut oversized = fact(entity, "state", 1);
        oversized.short_text = "x".repeat(DEFAULT_MAXIMUM_SHORT_TEXT_BYTES + 1);
        assert!(matches!(
            registry.publish(
                "producer",
                PresentationFactSnapshot {
                    revision: 1,
                    facts: vec![oversized]
                }
            ),
            Err(PresentationFactPublishError::ShortTextTooLong { .. })
        ));

        let mut control = fact(
            PresentationEntityRef::new("example.tasks", "task-control"),
            "state",
            1,
        );
        control.short_text = "working\u{001b}[31m".to_string();
        assert_eq!(
            registry.publish(
                "producer",
                PresentationFactSnapshot {
                    revision: 2,
                    facts: vec![control],
                },
            ),
            Err(PresentationFactPublishError::TerminalControlData)
        );
    }
}
