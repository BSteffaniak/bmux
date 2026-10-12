use crate::{
    mirror::MirrorFrame,
    protocol::{
        Acknowledgment, Action, AssetInfo, Cursor, HostError, InteractionMode, PROTOCOL_VERSION,
        Poll, Request, Response, Scopes, Snapshot, Submission, SurfaceInput,
    },
};
use bmux_presentation::{Deck, NavigationAction, PresentationState, reduce};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Limits {
    pub message_bytes: usize,
    pub deck_bytes: usize,
    pub asset_bytes: usize,
    pub total_asset_bytes: usize,
    pub assets: usize,
    pub frame_bytes: usize,
    pub canvas_cells: usize,
    pub surfaces: usize,
    pub participants: usize,
    pub credentials: usize,
    pub action_bytes: usize,
    pub requests_per_second: u32,
    pub retained_actions: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            message_bytes: 4 * 1024 * 1024,
            deck_bytes: 512 * 1024,
            asset_bytes: 8 * 1024 * 1024,
            total_asset_bytes: 32 * 1024 * 1024,
            assets: 128,
            frame_bytes: 512 * 1024,
            canvas_cells: 16_384,
            surfaces: 4,
            participants: 64,
            credentials: 64,
            action_bytes: 4096,
            requests_per_second: 30,
            retained_actions: 128,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct HostConfig {
    pub public_audience: bool,
    pub limits: Limits,
}

/// A credential is never exported in snapshots, responses, Debug output, or resume cursors.
pub struct Credential {
    pub secret: String,
    pub scopes: Scopes,
    pub expires_at: Instant,
}
impl Credential {
    pub fn generate(scopes: Scopes, lifetime: Duration) -> Self {
        Self {
            secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            scopes,
            expires_at: Instant::now() + lifetime,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Asset {
    pub id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

pub struct OwnerContext<'a> {
    pub participant: &'a str,
    /// Shared calls use one owner instance; independent calls use credential-local instances.
    /// The instance identity survives live-run reconnect; participant is connection-local.
    pub instance: &'a str,
    pub state: &'a PresentationState,
    pub cursor: &'a Cursor,
}
#[derive(Default)]
pub struct OwnerUpdate {
    pub outcome: String,
    pub surfaces: Vec<MirrorFrame>,
}

/// Implementations are linked trusted Rust, not viewer-supplied executable code.
/// Calls are serialized by Host's mutable borrow. Do not block on network IO here.
/// Effects run once on the owner, never during snapshot or polling. Failed calls are
/// uncertain and their consumed revision must not be retried automatically.
pub trait Owner: Send {
    fn scene(&mut self, _context: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        Ok(Vec::new())
    }
    fn demo_input(
        &mut self,
        context: OwnerContext<'_>,
        input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError>;
    fn effect(
        &mut self,
        context: OwnerContext<'_>,
        name: &str,
        input: &str,
    ) -> Result<OwnerUpdate, HostError>;
    fn disconnect(&mut self, _participant: &str) {}
}

struct Participant {
    grant: Option<String>,
    mode: InteractionMode,
    state: PresentationState,
    revision: u64,
    surfaces: BTreeMap<String, MirrorFrame>,
    window: Instant,
    requests: u32,
}

struct IndependentCache {
    state: PresentationState,
    revision: u64,
    surfaces: BTreeMap<String, MirrorFrame>,
}
#[derive(Default)]
struct PrincipalActions {
    high_water: u64,
    outcomes: BTreeMap<u64, (Vec<u8>, Result<Acknowledgment, HostError>)>,
    rate_window: Option<(Instant, u32)>,
    independent: Option<IndependentCache>,
}

pub struct Host {
    config: HostConfig,
    run_id: String,
    deck_revision: u64,
    revision: u64,
    deck: Deck,
    public_deck: Deck,
    public_assets: BTreeMap<String, Asset>,
    state: PresentationState,
    surfaces: BTreeMap<String, MirrorFrame>,
    credentials: BTreeMap<String, Credential>,
    participants: BTreeMap<String, Participant>,
    actions: BTreeMap<String, PrincipalActions>,
    owner: Option<Box<dyn Owner>>,
}

impl Host {
    pub fn new(deck: Deck, assets: Vec<Asset>, config: HostConfig) -> Result<Self, HostError> {
        let assets = validate_bundle(&deck, assets, &config.limits)?;
        let (public_deck, public_assets) = crate::projection::project(&deck, &assets)?;
        if config.limits.message_bytes == 0
            || config.limits.message_bytes > 64 * 1024 * 1024
            || config.limits.requests_per_second == 0
            || config.limits.retained_actions > 1024
            || config.limits.credentials > 1024
            || config.limits.participants > 1024
            || config.limits.action_bytes > 64 * 1024
            || config.limits.canvas_cells > 65_536
            || config.limits.frame_bytes > 8 * 1024 * 1024
            || config.limits.surfaces > 64
            || config.limits.deck_bytes > 8 * 1024 * 1024
            || config.limits.assets > 1024
            || config.limits.asset_bytes > 64 * 1024 * 1024
            || config.limits.total_asset_bytes > 256 * 1024 * 1024
        {
            return Err(HostError::Invalid);
        }
        Ok(Self {
            run_id: Uuid::new_v4().to_string(),
            deck_revision: 1,
            revision: 1,
            state: PresentationState::new(&deck),
            deck,
            public_deck,
            public_assets,
            config,
            surfaces: BTreeMap::new(),
            credentials: BTreeMap::new(),
            participants: BTreeMap::new(),
            actions: BTreeMap::new(),
            owner: None,
        })
    }
    pub fn limits(&self) -> &Limits {
        &self.config.limits
    }
    pub fn set_owner(&mut self, mut owner: Box<dyn Owner>) -> Result<(), HostError> {
        let revision = self.revision.checked_add(1).ok_or(HostError::Limit)?;
        let cursor = Cursor {
            run_id: self.run_id.clone(),
            deck_revision: self.deck_revision,
            revision,
        };
        let frames = project_scene(
            owner.as_mut(),
            &self.deck,
            &self.config.limits,
            "",
            "shared",
            &self.state,
            &cursor,
        )?;
        validate_surface_revisions(&self.surfaces, &frames)?;
        let mut independent_frames = BTreeMap::new();
        for (id, participant) in &self.participants {
            if participant.mode != InteractionMode::Independent {
                continue;
            }
            let cursor = Cursor {
                revision: participant
                    .revision
                    .checked_add(1)
                    .ok_or(HostError::Limit)?,
                ..cursor.clone()
            };
            let frames = project_scene(
                owner.as_mut(),
                &self.deck,
                &self.config.limits,
                id,
                participant
                    .grant
                    .as_deref()
                    .ok_or(HostError::Authentication)?,
                &participant.state,
                &cursor,
            )?;
            independent_frames.insert(id.clone(), frames);
        }
        for (id, frames) in independent_frames {
            let participant = self
                .participants
                .get_mut(&id)
                .ok_or(HostError::Authentication)?;
            participant.revision += 1;
            participant.surfaces = frames;
        }
        self.owner = Some(owner);
        self.surfaces = frames;
        self.revision = revision;
        Ok(())
    }
    pub fn add_credential(&mut self, credential: Credential) -> Result<String, HostError> {
        self.credentials
            .retain(|_, value| value.expires_at > Instant::now());
        self.actions
            .retain(|id, _| self.credentials.contains_key(id));
        if self.credentials.len() >= self.config.limits.credentials
            || credential.secret.len() < 32
            || credential.secret.len() > 256
            || credential.expires_at <= Instant::now()
            || self
                .credentials
                .values()
                .any(|value| constant_time_equal(&value.secret, &credential.secret))
        {
            return Err(HostError::Limit);
        }
        let id = Uuid::new_v4().to_string();
        self.actions.insert(id.clone(), PrincipalActions::default());
        self.credentials.insert(id.clone(), credential);
        Ok(id)
    }
    pub fn revoke_credential(&mut self, grant: &str) {
        self.credentials.remove(grant);
        self.actions.remove(grant);
    }

    pub fn authenticate(
        &mut self,
        version: u16,
        secret: Option<&str>,
        mode: InteractionMode,
    ) -> Result<String, HostError> {
        if version != PROTOCOL_VERSION {
            return Err(HostError::Version);
        }
        if self.participants.len() >= self.config.limits.participants {
            return Err(HostError::Limit);
        }
        let grant = match secret {
            Some(secret) if secret.len() <= 256 => self
                .credentials
                .iter()
                .find(|(_, value)| {
                    value.expires_at > Instant::now() && constant_time_equal(&value.secret, secret)
                })
                .map(|(id, _)| id.clone())
                .ok_or(HostError::Authentication)
                .map(Some)?,
            None if self.config.public_audience && mode == InteractionMode::ReceiveOnly => None,
            _ => return Err(HostError::Authentication),
        };
        if mode == InteractionMode::Independent
            && self.participants.values().any(|participant| {
                participant.mode == InteractionMode::Independent && participant.grant == grant
            })
        {
            return Err(HostError::Limit);
        }
        let retained = if mode == InteractionMode::Independent {
            grant
                .as_ref()
                .and_then(|id| self.actions.get_mut(id))
                .and_then(|ledger| ledger.independent.take())
        } else {
            None
        };
        let (state, revision, surfaces) = retained.map_or_else(
            || (self.state.clone(), 1, BTreeMap::new()),
            |cache| (cache.state, cache.revision, cache.surfaces),
        );
        let id = Uuid::new_v4().to_string();
        self.participants.insert(
            id.clone(),
            Participant {
                grant,
                mode,
                state,
                revision,
                surfaces,
                window: Instant::now(),
                requests: 0,
            },
        );
        if mode == InteractionMode::Independent
            && let Err(error) = self.refresh_scene(&id, true)
        {
            self.disconnect(&id);
            return Err(error);
        }
        Ok(id)
    }
    pub fn disconnect(&mut self, participant: &str) {
        if let Some(value) = self.participants.remove(participant) {
            if value.mode == InteractionMode::Independent
                && let Some(ledger) = value
                    .grant
                    .as_ref()
                    .and_then(|grant| self.actions.get_mut(grant))
            {
                ledger.independent = Some(IndependentCache {
                    state: value.state,
                    revision: value.revision,
                    surfaces: value.surfaces,
                });
            }
            if let Some(owner) = &mut self.owner {
                owner.disconnect(participant);
            }
        }
    }
    fn authorize(&self, id: &str) -> Result<Scopes, HostError> {
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        match &participant.grant {
            Some(grant) => self
                .credentials
                .get(grant)
                .filter(|value| value.expires_at > Instant::now())
                .map(|value| value.scopes.clone())
                .ok_or(HostError::Authentication),
            None if self.config.public_audience
                && participant.mode == InteractionMode::ReceiveOnly =>
            {
                Ok(Scopes::default())
            }
            None => Err(HostError::Authentication),
        }
    }
    fn rate(&mut self, id: &str) -> Result<(), HostError> {
        self.authorize(id)?;
        let participant = self
            .participants
            .get_mut(id)
            .ok_or(HostError::Authentication)?;
        if let Some(grant) = &participant.grant {
            let ledger = self
                .actions
                .get_mut(grant)
                .ok_or(HostError::Authentication)?;
            let (window, requests) = ledger.rate_window.get_or_insert((Instant::now(), 0));
            if window.elapsed() >= Duration::from_secs(1) {
                *window = Instant::now();
                *requests = 0;
            }
            if *requests >= self.config.limits.requests_per_second {
                return Err(HostError::Limit);
            }
            *requests += 1;
            return Ok(());
        }
        if participant.window.elapsed() >= Duration::from_secs(1) {
            participant.window = Instant::now();
            participant.requests = 0;
        }
        if participant.requests >= self.config.limits.requests_per_second {
            return Err(HostError::Limit);
        }
        participant.requests += 1;
        Ok(())
    }
    pub fn cursor(&self, id: &str) -> Result<Cursor, HostError> {
        self.authorize(id)?;
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        Ok(Cursor {
            run_id: self.run_id.clone(),
            deck_revision: self.deck_revision,
            revision: if participant.mode == InteractionMode::Independent {
                participant.revision
            } else {
                self.revision
            },
        })
    }
    pub fn next_action_id(&self, id: &str) -> Result<Option<u64>, HostError> {
        self.authorize(id)?;
        Ok(self
            .participants
            .get(id)
            .and_then(|participant| participant.grant.as_ref())
            .and_then(|grant| self.actions.get(grant))
            .and_then(|ledger| ledger.high_water.checked_add(1)))
    }
    pub fn snapshot(&mut self, id: &str) -> Result<Snapshot, HostError> {
        match self.project(id, None, true)? {
            Poll::Snapshot { snapshot, .. } => Ok(*snapshot),
            Poll::Unchanged(_) => Err(HostError::Invalid),
        }
    }
    pub fn poll(&mut self, id: &str, previous: Option<&Cursor>) -> Result<Poll, HostError> {
        self.project(id, previous, false)
    }
    fn project(
        &mut self,
        id: &str,
        previous: Option<&Cursor>,
        bootstrap: bool,
    ) -> Result<Poll, HostError> {
        self.rate(id)?;
        let scopes = self.authorize(id)?;
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        if participant.mode == InteractionMode::SubmitOnly && !bootstrap {
            return Err(HostError::Unauthorized);
        }
        let cursor = self.cursor(id)?;
        if previous == Some(&cursor) {
            return Ok(Poll::Unchanged(cursor));
        }
        let independent = participant.mode == InteractionMode::Independent;
        let state = if independent {
            &participant.state
        } else {
            &self.state
        };
        let surfaces = if independent {
            &participant.surfaces
        } else {
            &self.surfaces
        };
        let snapshot = Snapshot {
            cursor: cursor.clone(),
            deck: self.public_deck.clone(),
            state: state.clone(),
            notes: if scopes.notes {
                state
                    .current_slide(&self.deck)
                    .map(|slide| slide.notes.text.clone())
            } else {
                None
            },
            surfaces: surfaces
                .values()
                .filter(|frame| {
                    state
                        .slide_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == frame.slide_id)
                })
                .cloned()
                .collect(),
            assets: self
                .public_assets
                .values()
                .map(|asset| AssetInfo {
                    id: asset.id.clone(),
                    media_type: asset.media_type.clone(),
                    bytes: asset.bytes.len(),
                })
                .collect(),
        };
        if serde_json::to_vec(&snapshot)
            .map_err(|_| HostError::Invalid)?
            .len()
            > self.config.limits.message_bytes / 2
        {
            return Err(HostError::Limit);
        }
        // Full bounded snapshots repair missing history; there is no unbounded event log.
        Ok(Poll::Snapshot {
            resync: previous.is_none_or(|old| {
                old.run_id != cursor.run_id
                    || old.deck_revision != cursor.deck_revision
                    || old.revision > cursor.revision
            }),
            snapshot: Box::new(snapshot),
        })
    }

    pub fn submit(
        &mut self,
        id: &str,
        submission: Submission,
    ) -> Result<Acknowledgment, HostError> {
        self.rate(id)?;
        self.authorize(id)?;
        let bytes = serde_json::to_vec(&submission).map_err(|_| HostError::Invalid)?;
        if bytes.len() > self.config.limits.action_bytes {
            return Err(HostError::Limit);
        }
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        if participant.mode == InteractionMode::ReceiveOnly {
            return Err(HostError::Unauthorized);
        }
        let grant = participant.grant.clone().ok_or(HostError::Unauthorized)?;
        let ledger = self.actions.get(&grant).ok_or(HostError::Authentication)?;
        if let Some((original, result)) = ledger.outcomes.get(&submission.action_id) {
            return if original == &bytes {
                result.clone()
            } else {
                Err(HostError::ActionConflict)
            };
        }
        if submission.action_id <= ledger.high_water {
            return Err(HostError::OutcomeEvicted);
        }
        if ledger.high_water.checked_add(1) != Some(submission.action_id) {
            return Err(HostError::ActionSequence);
        }
        let action_id = submission.action_id;
        // Retain rejected/uncertain outcomes as well. Identity is consumed exactly once.
        let result = self.submit_fresh(id, submission);
        let ledger = self
            .actions
            .get_mut(&grant)
            .ok_or(HostError::Authentication)?;
        ledger.high_water = action_id;
        ledger.outcomes.insert(action_id, (bytes, result.clone()));
        while ledger.outcomes.len() > self.config.limits.retained_actions {
            ledger.outcomes.pop_first();
        }
        result
    }
    fn submit_fresh(
        &mut self,
        id: &str,
        submission: Submission,
    ) -> Result<Acknowledgment, HostError> {
        let scopes = self.authorize(id)?;
        if serde_json::to_vec(&submission)
            .map_err(|_| HostError::Invalid)?
            .len()
            > self.config.limits.action_bytes
        {
            return Err(HostError::Limit);
        }
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        if participant.mode == InteractionMode::ReceiveOnly {
            return Err(HostError::Unauthorized);
        }
        if submission.expected != self.cursor(id)? {
            return Err(HostError::Stale);
        }
        let independent = participant.mode == InteractionMode::Independent;
        match &submission.action {
            Action::Navigate(_) if !scopes.navigate => return Err(HostError::Unauthorized),
            Action::DemoInput(_) if !scopes.demo_input => return Err(HostError::Unauthorized),
            Action::Effect { .. } if !scopes.effects => return Err(HostError::Unauthorized),
            _ => {}
        }
        let state = if independent {
            &participant.state
        } else {
            &self.state
        };
        let surfaces = if independent {
            &participant.surfaces
        } else {
            &self.surfaces
        };
        if let Action::DemoInput(input) = &submission.action
            && (surfaces.get(&input.surface_id).is_none_or(|surface| {
                surface.revision != input.surface_revision
                    || input.x >= surface.width
                    || input.y >= surface.height
                    || state
                        .slide_id
                        .as_ref()
                        .is_none_or(|id| id.as_str() != surface.slide_id)
            }))
        {
            return Err(HostError::Stale);
        }
        if !matches!(&submission.action, Action::Navigate(_)) && self.owner.is_none() {
            return Err(HostError::NotFound);
        }
        // Consume ordering BEFORE external invocation, including uncertain failures.
        if independent {
            let participant = self
                .participants
                .get_mut(id)
                .ok_or(HostError::Authentication)?;
            participant.revision = participant
                .revision
                .checked_add(1)
                .ok_or(HostError::Limit)?;
        } else {
            self.revision = self.revision.checked_add(1).ok_or(HostError::Limit)?;
        }
        let outcome = match submission.action {
            Action::Navigate(action) => {
                if independent {
                    let participant = self
                        .participants
                        .get_mut(id)
                        .ok_or(HostError::Authentication)?;
                    reduce(&self.deck, &mut participant.state, action);
                    participant.surfaces.clear();
                } else {
                    reduce(&self.deck, &mut self.state, action);
                    self.surfaces.clear();
                }
                self.refresh_scene(id, independent)?;
                "navigation committed".to_owned()
            }
            action => {
                let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
                let state = if independent {
                    &participant.state
                } else {
                    &self.state
                };
                let cursor = self.cursor(id)?;
                let context = OwnerContext {
                    participant: id,
                    instance: if independent {
                        participant
                            .grant
                            .as_deref()
                            .ok_or(HostError::Authentication)?
                    } else {
                        "shared"
                    },
                    state,
                    cursor: &cursor,
                };
                let owner = self.owner.as_mut().ok_or(HostError::NotFound)?;
                let update = match action {
                    Action::DemoInput(input) => owner.demo_input(context, &input),
                    Action::Effect { name, input } => owner.effect(context, &name, &input),
                    Action::Navigate(_) => unreachable!(),
                }
                .map_err(|_| HostError::EffectUncertain)?;
                if update.outcome.len() > self.config.limits.action_bytes {
                    return Err(HostError::EffectUncertain);
                }
                let frames = self
                    .validate_frames(update.surfaces)
                    .map_err(|_| HostError::EffectUncertain)?;
                let old = if independent {
                    &self
                        .participants
                        .get(id)
                        .ok_or(HostError::Authentication)?
                        .surfaces
                } else {
                    &self.surfaces
                };
                validate_surface_revisions(old, &frames).map_err(|_| HostError::EffectUncertain)?;
                if independent {
                    self.participants
                        .get_mut(id)
                        .ok_or(HostError::Authentication)?
                        .surfaces = frames;
                } else {
                    self.surfaces = frames;
                }
                update.outcome
            }
        };
        Ok(Acknowledgment {
            action_id: submission.action_id,
            cursor: self.cursor(id)?,
            outcome,
        })
    }

    fn refresh_scene(&mut self, id: &str, independent: bool) -> Result<(), HostError> {
        let cursor = self.cursor(id)?;
        let participant = self.participants.get(id).ok_or(HostError::Authentication)?;
        let state = if independent {
            &participant.state
        } else {
            &self.state
        };
        let instance = if independent {
            participant
                .grant
                .as_deref()
                .ok_or(HostError::Authentication)?
        } else {
            "shared"
        };
        let frames = if let Some(owner) = &mut self.owner {
            project_scene(
                owner.as_mut(),
                &self.deck,
                &self.config.limits,
                id,
                instance,
                state,
                &cursor,
            )?
        } else {
            BTreeMap::new()
        };
        if independent {
            self.participants
                .get_mut(id)
                .ok_or(HostError::Authentication)?
                .surfaces = frames;
        } else {
            self.surfaces = frames;
        }
        Ok(())
    }
    fn validate_frames(
        &self,
        frames: Vec<MirrorFrame>,
    ) -> Result<BTreeMap<String, MirrorFrame>, HostError> {
        if frames.len() > self.config.limits.surfaces {
            return Err(HostError::Limit);
        }
        let mut result = BTreeMap::new();
        for frame in frames {
            frame.validate(
                self.config.limits.canvas_cells,
                self.config.limits.frame_bytes,
            )?;
            if !self
                .deck
                .slides
                .iter()
                .any(|slide| slide.id.as_str() == frame.slide_id)
            {
                return Err(HostError::Invalid);
            }
            if result.insert(frame.surface_id.clone(), frame).is_some() {
                return Err(HostError::Invalid);
            }
        }
        Ok(result)
    }
    /// Trusted owner commits a complete scene batch; failed validation changes nothing.
    pub fn commit_surfaces(
        &mut self,
        expected: &Cursor,
        independent: Option<&str>,
        frames: Vec<MirrorFrame>,
    ) -> Result<(), HostError> {
        let current = if let Some(id) = independent {
            self.cursor(id)?
        } else {
            Cursor {
                run_id: self.run_id.clone(),
                deck_revision: self.deck_revision,
                revision: self.revision,
            }
        };
        if &current != expected {
            return Err(HostError::Stale);
        }
        let frames = self.validate_frames(frames)?;
        let old = if let Some(id) = independent {
            &self
                .participants
                .get(id)
                .ok_or(HostError::Authentication)?
                .surfaces
        } else {
            &self.surfaces
        };
        validate_surface_revisions(old, &frames)?;
        if let Some(id) = independent {
            let participant = self
                .participants
                .get_mut(id)
                .ok_or(HostError::Authentication)?;
            if participant.mode != InteractionMode::Independent {
                return Err(HostError::Invalid);
            }
            participant.revision = participant
                .revision
                .checked_add(1)
                .ok_or(HostError::Limit)?;
            participant.surfaces = frames;
        } else {
            self.revision = self.revision.checked_add(1).ok_or(HostError::Limit)?;
            self.surfaces = frames;
        }
        Ok(())
    }
    /// Validate the complete replacement before making it visible. No effects replay.
    pub fn reload(&mut self, deck: Deck, assets: Vec<Asset>) -> Result<(), HostError> {
        self.reload_inner(deck, assets, None)
    }
    /// Prepare a replacement owner's complete scene set before swapping any host
    /// state. The old owner is never called or modified on this path.
    pub fn reload_with_owner(
        &mut self,
        deck: Deck,
        assets: Vec<Asset>,
        owner: Box<dyn Owner>,
    ) -> Result<(), HostError> {
        self.reload_inner(deck, assets, Some(owner))
    }
    fn reload_inner(
        &mut self,
        deck: Deck,
        assets: Vec<Asset>,
        mut replacement: Option<Box<dyn Owner>>,
    ) -> Result<(), HostError> {
        let assets = validate_bundle(&deck, assets, &self.config.limits)?;
        let (public_deck, public_assets) = crate::projection::project(&deck, &assets)?;
        let deck_revision = self.deck_revision.checked_add(1).ok_or(HostError::Limit)?;
        let revision = self.revision.checked_add(1).ok_or(HostError::Limit)?;
        if self
            .participants
            .values()
            .any(|participant| participant.revision == u64::MAX)
            || self.actions.values().any(|ledger| {
                ledger
                    .independent
                    .as_ref()
                    .is_some_and(|cache| cache.revision == u64::MAX)
            })
        {
            return Err(HostError::Limit);
        }
        let mut next_state = self.state.clone();
        reduce(&deck, &mut next_state, NavigationAction::Reconcile);
        let cursor = Cursor {
            run_id: self.run_id.clone(),
            deck_revision,
            revision,
        };
        let mut shared_frames = BTreeMap::new();
        let mut independent_frames = BTreeMap::new();
        let mut cached_frames = BTreeMap::new();
        if let Some(owner) = replacement.as_mut().or(self.owner.as_mut()) {
            shared_frames = project_scene(
                owner.as_mut(),
                &deck,
                &self.config.limits,
                "",
                "shared",
                &next_state,
                &cursor,
            )?;
            for (id, participant) in &self.participants {
                if participant.mode != InteractionMode::Independent {
                    continue;
                }
                let mut state = participant.state.clone();
                reduce(&deck, &mut state, NavigationAction::Reconcile);
                let cursor = Cursor {
                    revision: participant.revision + 1,
                    ..cursor.clone()
                };
                let frames = project_scene(
                    owner.as_mut(),
                    &deck,
                    &self.config.limits,
                    id,
                    participant
                        .grant
                        .as_deref()
                        .ok_or(HostError::Authentication)?,
                    &state,
                    &cursor,
                )?;
                independent_frames.insert(id.clone(), frames);
            }
            for (grant, ledger) in &self.actions {
                if let Some(cache) = &ledger.independent {
                    let mut state = cache.state.clone();
                    reduce(&deck, &mut state, NavigationAction::Reconcile);
                    let cursor = Cursor {
                        revision: cache.revision + 1,
                        ..cursor.clone()
                    };
                    cached_frames.insert(
                        grant.clone(),
                        project_scene(
                            owner.as_mut(),
                            &deck,
                            &self.config.limits,
                            "",
                            grant,
                            &state,
                            &cursor,
                        )?,
                    );
                }
            }
        }
        if let Some(owner) = replacement {
            self.owner = Some(owner);
        }
        self.state = next_state;
        for (id, participant) in &mut self.participants {
            reduce(&deck, &mut participant.state, NavigationAction::Reconcile);
            participant.revision += 1;
            participant.surfaces = independent_frames.remove(id).unwrap_or_default();
        }
        for (grant, ledger) in &mut self.actions {
            if let Some(cache) = &mut ledger.independent {
                reduce(&deck, &mut cache.state, NavigationAction::Reconcile);
                cache.revision += 1;
                cache.surfaces = cached_frames.remove(grant).unwrap_or_default();
            }
        }
        self.deck = deck;
        self.public_deck = public_deck;
        self.public_assets = public_assets;
        self.surfaces = shared_frames;
        self.deck_revision = deck_revision;
        self.revision = revision;
        Ok(())
    }
    pub fn asset(
        &mut self,
        id: &str,
        asset: &str,
        offset: usize,
        length: usize,
    ) -> Result<Response, HostError> {
        self.rate(id)?;
        // Explicit submit-only bootstrap also permits bounded authorized asset chunks.
        if length == 0 || length > self.config.limits.message_bytes / 8 {
            return Err(HostError::Limit);
        }
        let asset = self.public_assets.get(asset).ok_or(HostError::NotFound)?;
        if offset > asset.bytes.len() {
            return Err(HostError::Invalid);
        }
        let end = offset.saturating_add(length).min(asset.bytes.len());
        Ok(Response::Asset {
            id: asset.id.clone(),
            offset,
            total: asset.bytes.len(),
            bytes: asset.bytes[offset..end].to_vec(),
        })
    }
    /// Connection-scoped adapter. Authentication IDs are not accepted on the wire.
    pub fn handle(&mut self, participant: &mut Option<String>, request: Request) -> Response {
        let result = match request {
            Request::Authenticate {
                version,
                credential,
                mode,
            } => {
                if participant.is_some() {
                    Err(HostError::Unauthorized)
                } else {
                    self.authenticate(version, credential.as_deref(), mode)
                        .and_then(|id| {
                            let cursor = self.cursor(&id)?;
                            *participant = Some(id.clone());
                            let next_action_id = self.next_action_id(&id)?;
                            Ok(Response::Authenticated {
                                participant: id,
                                cursor,
                                mode,
                                next_action_id,
                            })
                        })
                }
            }
            request => match participant.as_deref() {
                None => Err(HostError::Authentication),
                Some(id) => match request {
                    Request::Snapshot => self.snapshot(id).map(|snapshot| {
                        Response::Poll(Poll::Snapshot {
                            resync: true,
                            snapshot: Box::new(snapshot),
                        })
                    }),
                    Request::Poll { cursor } => self.poll(id, cursor.as_ref()).map(Response::Poll),
                    Request::Submit(submission) => {
                        self.submit(id, submission).map(Response::Acknowledged)
                    }
                    Request::Asset {
                        id: asset,
                        offset,
                        length,
                    } => self.asset(id, &asset, offset, length),
                    Request::Authenticate { .. } => Err(HostError::Unauthorized),
                },
            },
        };
        result.unwrap_or_else(Response::Error)
    }
}

fn project_scene(
    owner: &mut dyn Owner,
    deck: &Deck,
    limits: &Limits,
    participant: &str,
    instance: &str,
    state: &PresentationState,
    cursor: &Cursor,
) -> Result<BTreeMap<String, MirrorFrame>, HostError> {
    let frames = owner
        .scene(OwnerContext {
            participant,
            instance,
            state,
            cursor,
        })
        .map_err(|_| HostError::Invalid)?;
    if frames.len() > limits.surfaces {
        return Err(HostError::Limit);
    }
    let mut result = BTreeMap::new();
    for frame in frames {
        frame.validate(limits.canvas_cells, limits.frame_bytes)?;
        if state
            .slide_id
            .as_ref()
            .is_none_or(|id| id.as_str() != frame.slide_id)
            || !deck
                .slides
                .iter()
                .any(|slide| slide.id.as_str() == frame.slide_id)
        {
            return Err(HostError::Invalid);
        }
        if result.insert(frame.surface_id.clone(), frame).is_some() {
            return Err(HostError::Invalid);
        }
    }
    Ok(result)
}

fn validate_surface_revisions(
    old: &BTreeMap<String, MirrorFrame>,
    new: &BTreeMap<String, MirrorFrame>,
) -> Result<(), HostError> {
    if new.iter().any(|(id, frame)| {
        old.get(id)
            .is_some_and(|previous| frame.revision <= previous.revision)
    }) {
        return Err(HostError::Stale);
    }
    Ok(())
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.as_bytes().get(index).copied().unwrap_or(0)
                ^ right.as_bytes().get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}
fn validate_bundle(
    deck: &Deck,
    assets: Vec<Asset>,
    limits: &Limits,
) -> Result<BTreeMap<String, Asset>, HostError> {
    if assets.len() > limits.assets {
        return Err(HostError::Limit);
    }
    deck.validate().map_err(|_| HostError::Invalid)?;
    let public_bytes = serde_json::to_vec(deck)
        .map_err(|_| HostError::Invalid)?
        .len();
    let notes_bytes = deck
        .slides
        .iter()
        .try_fold(0_usize, |sum, slide| {
            sum.checked_add(slide.notes.text.len())
        })
        .ok_or(HostError::Limit)?;
    if public_bytes
        .checked_add(notes_bytes)
        .ok_or(HostError::Limit)?
        > limits.deck_bytes
    {
        return Err(HostError::Limit);
    }
    let mut total = 0_usize;
    let mut result = BTreeMap::new();
    for asset in assets {
        if asset.id.is_empty()
            || asset.id.len() > 128
            || asset.media_type.len() > 128
            || asset.bytes.len() > limits.asset_bytes
        {
            return Err(HostError::Limit);
        }
        total = total
            .checked_add(asset.bytes.len())
            .ok_or(HostError::Limit)?;
        if total > limits.total_asset_bytes {
            return Err(HostError::Limit);
        }
        if result.insert(asset.id.clone(), asset).is_some() {
            return Err(HostError::Invalid);
        }
    }
    for reference in crate::projection::referenced_assets(deck) {
        if !result.contains_key(&reference) {
            return Err(HostError::NotFound);
        }
    }
    Ok(result)
}
