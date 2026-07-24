//! Generic attach-provider data, continuity, and control contract.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

use bmux_attach_layout_protocol::AttachScene;

/// Boxed asynchronous attach-session operation.
pub type AttachSessionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AttachSessionError>> + Send + 'a>>;

/// Monotonic revision of the logical attach view/layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AttachViewRevision(pub u64);

/// Monotonic sequence of provider events, independent from view revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AttachDeltaSequence(pub u64);

/// Stable provider-defined output stream identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachStreamId(String);

impl AttachStreamId {
    /// Construct a non-empty stream identity without surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is empty, has surrounding whitespace,
    /// or contains control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, AttachSessionError> {
        let value = value.into();
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            return Err(AttachSessionError::InvalidStreamId { value });
        }
        Ok(Self(value))
    }

    /// Borrow the opaque stream identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Cursor in one generation of one output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachStreamCursor {
    pub stream_id: AttachStreamId,
    /// Neutral attach surface receiving this stream's terminal state.
    pub surface_id: uuid::Uuid,
    pub generation: u64,
    pub offset: u64,
}

/// Complete parser-repair snapshot for one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachStreamSnapshot {
    pub cursor: AttachStreamCursor,
    /// Full terminal/parser state bytes at `cursor.offset`.
    pub snapshot: Vec<u8>,
}

/// Provider-independent state needed to resume after a recoverable disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachResumeState {
    pub view_revision: AttachViewRevision,
    pub event_sequence: AttachDeltaSequence,
    pub streams: Vec<AttachStreamCursor>,
    /// Provider-private, bounded opaque resume token.
    pub provider_token: Vec<u8>,
}

/// Initial or repaired complete attach state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderSnapshot {
    pub view_revision: AttachViewRevision,
    pub event_sequence: AttachDeltaSequence,
    pub scene: AttachScene,
    pub streams: Vec<AttachStreamSnapshot>,
    pub resume: AttachResumeState,
}

/// One ordered provider delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderDelta {
    pub sequence: AttachDeltaSequence,
    pub base_view_revision: AttachViewRevision,
    pub view_revision: AttachViewRevision,
    pub changes: Vec<AttachProviderChange>,
    pub resume: AttachResumeState,
}

/// Atomic change carried by an ordered delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachProviderChange {
    /// Replace the neutral retained scene/layout.
    Scene(AttachScene),
    /// Append contiguous output bytes to one stream generation.
    StreamAppend {
        cursor: AttachStreamCursor,
        end_offset: u64,
        bytes: Vec<u8>,
    },
    /// Replace parser state after retention loss or execution-generation change.
    StreamRepair(AttachStreamSnapshot),
    /// Remove a stream from the attached view.
    StreamRemoved { stream_id: AttachStreamId },
    /// Provider-defined status text for diagnostics and degraded state.
    Status { message: String },
}

/// Recoverable or terminal provider disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderDisconnect {
    pub recoverable: bool,
    pub reason: String,
    pub resume: Option<AttachResumeState>,
    pub retry_after_ms: Option<u64>,
}

/// Ordered event emitted by a provider session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachProviderEvent {
    Delta(AttachProviderDelta),
    Disconnected(AttachProviderDisconnect),
    Detached,
}

/// Input command directed at a current stream generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderInput {
    pub command_sequence: u64,
    pub stream_id: AttachStreamId,
    pub generation: u64,
    pub payload: AttachInputPayload,
}

/// Generic terminal input payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachInputPayload {
    Bytes(Vec<u8>),
    Paste(Vec<u8>),
    Mouse(AttachMouseInput),
}

/// Generic mouse input independent of terminal encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseInput {
    pub x: u16,
    pub y: u16,
    pub button: AttachMouseButton,
    pub phase: AttachMousePhase,
    pub modifiers: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMousePhase {
    Press,
    Release,
    Move,
    Drag,
    Scroll,
}

/// Current client viewport and terminal geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachProviderViewport {
    pub command_sequence: u64,
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: Option<u32>,
    pub pixel_height: Option<u32>,
}

/// Generic action command; action vocabulary is owned by the provider contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderAction {
    pub command_id: String,
    pub action: String,
    pub arguments: Vec<String>,
}

/// Acknowledgement for input, viewport, and action operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachProviderAck {
    pub command_id: Option<String>,
    pub accepted: bool,
    pub message: Option<String>,
}

/// Idempotent detach result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachDetachOutcome {
    Detached,
    AlreadyDetached,
}

/// Provider-session operation or continuity failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachSessionError {
    #[error("invalid attach stream id '{value}'")]
    InvalidStreamId { value: String },
    #[error("duplicate attach stream '{stream_id}'")]
    DuplicateStream { stream_id: String },
    #[error("unknown attach stream '{stream_id}'")]
    UnknownStream { stream_id: String },
    #[error("attach event sequence gap: expected {expected}, received {actual}")]
    SequenceGap { expected: u64, actual: u64 },
    #[error("attach view revision regressed from {current} to {received}")]
    RevisionRegression { current: u64, received: u64 },
    #[error("attach delta base revision mismatch: expected {expected}, received {actual}")]
    BaseRevisionMismatch { expected: u64, actual: u64 },
    #[error(
        "attach view revision changed from {current} to {received} without one scene replacement"
    )]
    InvalidRevisionChange { current: u64, received: u64 },
    #[error("attach delta contains multiple scene replacements")]
    MultipleSceneChanges,
    #[error("attach scene focus references a surface without an output stream")]
    FocusWithoutStream,
    #[error("stream '{stream_id}' references missing surface '{surface_id}'")]
    MissingSurface {
        stream_id: String,
        surface_id: uuid::Uuid,
    },
    #[error("stream '{stream_id}' changed surface identity")]
    SurfaceMismatch { stream_id: String },
    #[error("stream '{stream_id}' generation mismatch: current {current}, received {received}")]
    GenerationMismatch {
        stream_id: String,
        current: u64,
        received: u64,
    },
    #[error("stream '{stream_id}' generation regressed from {current} to {received}")]
    GenerationRegression {
        stream_id: String,
        current: u64,
        received: u64,
    },
    #[error("stream '{stream_id}' cursor gap: expected {expected}, received {actual}")]
    CursorGap {
        stream_id: String,
        expected: u64,
        actual: u64,
    },
    #[error("stream '{stream_id}' malformed range {start}..{end} for {bytes} bytes")]
    MalformedRange {
        stream_id: String,
        start: u64,
        end: u64,
        bytes: usize,
    },
    #[error("resume state does not match validated provider state: {reason}")]
    InvalidResume { reason: String },
    #[error("attach command sequence must increase beyond {current}, received {received}")]
    CommandSequenceRegression { current: u64, received: u64 },
    #[error("duplicate attach action command id '{command_id}'")]
    DuplicateCommand { command_id: String },
    #[error("attach session is detached")]
    Detached,
    #[error("attach provider operation failed: {reason}")]
    Provider { reason: String },
}

/// Object-safe generic attach session implemented by alternate providers.
pub trait AttachSession: std::fmt::Debug + Send + 'static {
    fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot>;
    /// Receive the next ordered event.
    ///
    /// The returned future must be cancellation-safe: dropping it before
    /// completion must not consume or skip an event.
    fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent>;
    fn send_input(
        &mut self,
        input: AttachProviderInput,
    ) -> AttachSessionFuture<'_, AttachProviderAck>;
    fn update_viewport(
        &mut self,
        viewport: AttachProviderViewport,
    ) -> AttachSessionFuture<'_, AttachProviderAck>;
    fn execute_action(
        &mut self,
        action: AttachProviderAction,
    ) -> AttachSessionFuture<'_, AttachProviderAck>;
    fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome>;
}

/// Stateful validator for provider control operations.
#[derive(Debug, Default)]
pub struct AttachControlValidator {
    last_command_sequence: u64,
    action_ids: BTreeSet<String>,
    detached: bool,
}

impl AttachControlValidator {
    /// Validate an input command's ordering and stream generation.
    ///
    /// # Errors
    ///
    /// Rejects detached sessions, non-increasing command sequences, unknown
    /// streams, and stale/future generations.
    pub fn validate_input(
        &mut self,
        input: &AttachProviderInput,
        continuity: &AttachContinuityValidator,
    ) -> Result<(), AttachSessionError> {
        self.ensure_attached()?;
        let current = continuity.streams.get(&input.stream_id).ok_or_else(|| {
            AttachSessionError::UnknownStream {
                stream_id: input.stream_id.as_str().to_string(),
            }
        })?;
        if input.generation != current.generation {
            return Err(AttachSessionError::GenerationMismatch {
                stream_id: input.stream_id.as_str().to_string(),
                current: current.generation,
                received: input.generation,
            });
        }
        self.advance_sequence(input.command_sequence)
    }

    /// Validate viewport update ordering.
    ///
    /// # Errors
    ///
    /// Rejects detached sessions and non-increasing command sequences.
    pub fn validate_viewport(
        &mut self,
        viewport: &AttachProviderViewport,
    ) -> Result<(), AttachSessionError> {
        self.ensure_attached()?;
        self.advance_sequence(viewport.command_sequence)
    }

    /// Validate one action ID for at-most-once dispatch.
    ///
    /// # Errors
    ///
    /// Rejects detached sessions, empty IDs, and duplicate IDs.
    pub fn validate_action(
        &mut self,
        action: &AttachProviderAction,
    ) -> Result<(), AttachSessionError> {
        self.ensure_attached()?;
        if action.command_id.is_empty() || !self.action_ids.insert(action.command_id.clone()) {
            return Err(AttachSessionError::DuplicateCommand {
                command_id: action.command_id.clone(),
            });
        }
        Ok(())
    }

    /// Transition to detached once; repeated calls are idempotent.
    #[must_use]
    pub const fn detach(&mut self) -> AttachDetachOutcome {
        if std::mem::replace(&mut self.detached, true) {
            AttachDetachOutcome::AlreadyDetached
        } else {
            AttachDetachOutcome::Detached
        }
    }

    const fn ensure_attached(&self) -> Result<(), AttachSessionError> {
        if self.detached {
            Err(AttachSessionError::Detached)
        } else {
            Ok(())
        }
    }

    const fn advance_sequence(&mut self, received: u64) -> Result<(), AttachSessionError> {
        if received <= self.last_command_sequence {
            return Err(AttachSessionError::CommandSequenceRegression {
                current: self.last_command_sequence,
                received,
            });
        }
        self.last_command_sequence = received;
        Ok(())
    }
}

/// Stateful continuity validator for provider snapshots and deltas.
#[derive(Debug, Default)]
pub struct AttachContinuityValidator {
    view_revision: AttachViewRevision,
    event_sequence: AttachDeltaSequence,
    streams: BTreeMap<AttachStreamId, AttachStreamCursor>,
    provider_token: Vec<u8>,
    scene: Option<AttachScene>,
    initialized: bool,
}

impl AttachContinuityValidator {
    /// Validate and install a complete snapshot.
    ///
    /// # Errors
    ///
    /// Rejects duplicate streams, inconsistent resume state, and malformed
    /// snapshot cursors.
    pub fn apply_snapshot(
        &mut self,
        snapshot: &AttachProviderSnapshot,
    ) -> Result<(), AttachSessionError> {
        let mut streams = BTreeMap::new();
        for stream in &snapshot.streams {
            if streams
                .insert(stream.cursor.stream_id.clone(), stream.cursor.clone())
                .is_some()
            {
                return Err(AttachSessionError::DuplicateStream {
                    stream_id: stream.cursor.stream_id.as_str().to_string(),
                });
            }
        }
        validate_scene_streams(&snapshot.scene, &streams)?;
        validate_resume(
            &snapshot.resume,
            snapshot.view_revision,
            snapshot.event_sequence,
            &streams,
        )?;
        self.view_revision = snapshot.view_revision;
        self.event_sequence = snapshot.event_sequence;
        self.streams = streams;
        self.scene = Some(snapshot.scene.clone());
        self.provider_token
            .clone_from(&snapshot.resume.provider_token);
        self.initialized = true;
        Ok(())
    }

    /// Validate and apply one ordered delta atomically.
    ///
    /// # Errors
    ///
    /// Rejects sequence/revision/cursor/generation violations or inconsistent
    /// resume state. State is unchanged on failure.
    pub fn apply_delta(&mut self, delta: &AttachProviderDelta) -> Result<(), AttachSessionError> {
        let expected_sequence = self.event_sequence.0.saturating_add(1);
        if !self.initialized || delta.sequence.0 != expected_sequence {
            return Err(AttachSessionError::SequenceGap {
                expected: expected_sequence,
                actual: delta.sequence.0,
            });
        }
        if delta.base_view_revision != self.view_revision {
            return Err(AttachSessionError::BaseRevisionMismatch {
                expected: self.view_revision.0,
                actual: delta.base_view_revision.0,
            });
        }
        if delta.view_revision < self.view_revision {
            return Err(AttachSessionError::RevisionRegression {
                current: self.view_revision.0,
                received: delta.view_revision.0,
            });
        }

        let scene_changes = delta
            .changes
            .iter()
            .filter(|change| matches!(change, AttachProviderChange::Scene(_)))
            .count();
        if scene_changes > 1 {
            return Err(AttachSessionError::MultipleSceneChanges);
        }
        let expected_revision = self
            .view_revision
            .0
            .saturating_add(u64::from(scene_changes == 1));
        if delta.view_revision.0 != expected_revision {
            return Err(AttachSessionError::InvalidRevisionChange {
                current: self.view_revision.0,
                received: delta.view_revision.0,
            });
        }

        let mut streams = self.streams.clone();
        let mut scene = self
            .scene
            .clone()
            .ok_or_else(|| AttachSessionError::InvalidResume {
                reason: "initialized continuity validator has no scene".to_string(),
            })?;
        for change in &delta.changes {
            apply_change(&mut streams, change)?;
            if let AttachProviderChange::Scene(replacement) = change {
                scene = replacement.clone();
            }
        }
        validate_scene_streams(&scene, &streams)?;
        validate_resume(&delta.resume, delta.view_revision, delta.sequence, &streams)?;
        self.streams = streams;
        self.scene = Some(scene);
        self.view_revision = delta.view_revision;
        self.event_sequence = delta.sequence;
        self.provider_token.clone_from(&delta.resume.provider_token);
        Ok(())
    }

    /// Current resumable state after successful validation.
    #[must_use]
    pub fn resume_state(&self) -> AttachResumeState {
        AttachResumeState {
            view_revision: self.view_revision,
            event_sequence: self.event_sequence,
            streams: self.streams.values().cloned().collect(),
            provider_token: self.provider_token.clone(),
        }
    }
}

fn apply_change(
    streams: &mut BTreeMap<AttachStreamId, AttachStreamCursor>,
    change: &AttachProviderChange,
) -> Result<(), AttachSessionError> {
    match change {
        AttachProviderChange::Scene(_) | AttachProviderChange::Status { .. } => Ok(()),
        AttachProviderChange::StreamRemoved { stream_id } => {
            streams.remove(stream_id);
            Ok(())
        }
        AttachProviderChange::StreamRepair(snapshot) => {
            if let Some(current) = streams.get(&snapshot.cursor.stream_id) {
                if snapshot.cursor.surface_id != current.surface_id {
                    return Err(AttachSessionError::SurfaceMismatch {
                        stream_id: snapshot.cursor.stream_id.as_str().to_string(),
                    });
                }
                if snapshot.cursor.generation < current.generation {
                    return Err(AttachSessionError::GenerationRegression {
                        stream_id: snapshot.cursor.stream_id.as_str().to_string(),
                        current: current.generation,
                        received: snapshot.cursor.generation,
                    });
                }
            }
            streams.insert(snapshot.cursor.stream_id.clone(), snapshot.cursor.clone());
            Ok(())
        }
        AttachProviderChange::StreamAppend {
            cursor,
            end_offset,
            bytes,
        } => {
            let current = streams.get(&cursor.stream_id).ok_or_else(|| {
                AttachSessionError::UnknownStream {
                    stream_id: cursor.stream_id.as_str().to_string(),
                }
            })?;
            if cursor.surface_id != current.surface_id {
                return Err(AttachSessionError::SurfaceMismatch {
                    stream_id: cursor.stream_id.as_str().to_string(),
                });
            }
            if cursor.generation < current.generation {
                return Err(AttachSessionError::GenerationRegression {
                    stream_id: cursor.stream_id.as_str().to_string(),
                    current: current.generation,
                    received: cursor.generation,
                });
            }
            if cursor.generation != current.generation {
                return Err(AttachSessionError::GenerationRegression {
                    stream_id: cursor.stream_id.as_str().to_string(),
                    current: current.generation,
                    received: cursor.generation,
                });
            }
            if cursor.offset != current.offset {
                return Err(AttachSessionError::CursorGap {
                    stream_id: cursor.stream_id.as_str().to_string(),
                    expected: current.offset,
                    actual: cursor.offset,
                });
            }
            let expected_end = cursor
                .offset
                .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            if expected_end != Some(*end_offset) {
                return Err(AttachSessionError::MalformedRange {
                    stream_id: cursor.stream_id.as_str().to_string(),
                    start: cursor.offset,
                    end: *end_offset,
                    bytes: bytes.len(),
                });
            }
            streams.insert(
                cursor.stream_id.clone(),
                AttachStreamCursor {
                    stream_id: cursor.stream_id.clone(),
                    surface_id: cursor.surface_id,
                    generation: cursor.generation,
                    offset: *end_offset,
                },
            );
            Ok(())
        }
    }
}

fn validate_scene_streams(
    scene: &AttachScene,
    streams: &BTreeMap<AttachStreamId, AttachStreamCursor>,
) -> Result<(), AttachSessionError> {
    let surface_ids = scene
        .surfaces
        .iter()
        .map(|surface| surface.id)
        .collect::<BTreeSet<_>>();
    for cursor in streams.values() {
        if !surface_ids.contains(&cursor.surface_id) {
            return Err(AttachSessionError::MissingSurface {
                stream_id: cursor.stream_id.as_str().to_string(),
                surface_id: cursor.surface_id,
            });
        }
    }
    let focused_surface = match scene.focus {
        bmux_attach_layout_protocol::AttachFocusTarget::None => None,
        bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id } => Some(pane_id),
        bmux_attach_layout_protocol::AttachFocusTarget::Surface { surface_id } => Some(surface_id),
    };
    if focused_surface
        .is_some_and(|focused| !streams.values().any(|cursor| cursor.surface_id == focused))
    {
        return Err(AttachSessionError::FocusWithoutStream);
    }
    Ok(())
}

fn validate_resume(
    resume: &AttachResumeState,
    revision: AttachViewRevision,
    sequence: AttachDeltaSequence,
    streams: &BTreeMap<AttachStreamId, AttachStreamCursor>,
) -> Result<(), AttachSessionError> {
    if resume.view_revision != revision || resume.event_sequence != sequence {
        return Err(AttachSessionError::InvalidResume {
            reason: "revision or sequence mismatch".to_string(),
        });
    }
    let mut seen = BTreeSet::new();
    let resume_streams = resume
        .streams
        .iter()
        .map(|cursor| {
            if !seen.insert(cursor.stream_id.clone()) {
                return Err(AttachSessionError::DuplicateStream {
                    stream_id: cursor.stream_id.as_str().to_string(),
                });
            }
            Ok((cursor.stream_id.clone(), cursor.clone()))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    if &resume_streams != streams {
        return Err(AttachSessionError::InvalidResume {
            reason: "stream cursors do not match validated state".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    };
    use uuid::Uuid;

    fn stream_id() -> AttachStreamId {
        AttachStreamId::new("surface-1").unwrap()
    }

    fn scene() -> AttachScene {
        AttachScene {
            session_id: Uuid::nil(),
            focus: AttachFocusTarget::Surface {
                surface_id: Uuid::nil(),
            },
            surfaces: vec![AttachSurface {
                id: Uuid::nil(),
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(Uuid::nil()),
            }],
        }
    }

    fn cursor(offset: u64) -> AttachStreamCursor {
        AttachStreamCursor {
            stream_id: stream_id(),
            surface_id: Uuid::nil(),
            generation: 1,
            offset,
        }
    }

    fn snapshot() -> AttachProviderSnapshot {
        AttachProviderSnapshot {
            view_revision: AttachViewRevision(2),
            event_sequence: AttachDeltaSequence(7),
            scene: scene(),
            streams: vec![AttachStreamSnapshot {
                cursor: cursor(5),
                snapshot: b"state".to_vec(),
            }],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(2),
                event_sequence: AttachDeltaSequence(7),
                streams: vec![cursor(5)],
                provider_token: b"token".to_vec(),
            },
        }
    }

    #[test]
    fn snapshot_and_contiguous_output_delta_advance_resume_state() {
        let mut validator = AttachContinuityValidator::default();
        validator.apply_snapshot(&snapshot()).unwrap();
        let delta = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(2),
            changes: vec![AttachProviderChange::StreamAppend {
                cursor: cursor(5),
                end_offset: 8,
                bytes: b"abc".to_vec(),
            }],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(2),
                event_sequence: AttachDeltaSequence(8),
                streams: vec![cursor(8)],
                provider_token: b"next".to_vec(),
            },
        };
        validator.apply_delta(&delta).unwrap();
        let resume = validator.resume_state();
        assert_eq!(resume.streams, vec![cursor(8)]);
        assert_eq!(resume.provider_token, b"next");
    }

    #[test]
    fn sequence_gap_and_revision_mismatch_are_rejected_atomically() {
        let mut validator = AttachContinuityValidator::default();
        validator.apply_snapshot(&snapshot()).unwrap();
        let before = validator.resume_state();
        let delta = AttachProviderDelta {
            sequence: AttachDeltaSequence(9),
            base_view_revision: AttachViewRevision(1),
            view_revision: AttachViewRevision(1),
            changes: Vec::new(),
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&delta),
            Err(AttachSessionError::SequenceGap { .. })
        ));
        assert_eq!(validator.resume_state(), before);
    }

    #[test]
    fn cursor_gap_and_malformed_range_are_rejected() {
        let mut validator = AttachContinuityValidator::default();
        validator.apply_snapshot(&snapshot()).unwrap();
        let mut delta = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(2),
            changes: vec![AttachProviderChange::StreamAppend {
                cursor: cursor(6),
                end_offset: 7,
                bytes: b"x".to_vec(),
            }],
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&delta),
            Err(AttachSessionError::CursorGap { .. })
        ));
        if let AttachProviderChange::StreamAppend {
            cursor, end_offset, ..
        } = &mut delta.changes[0]
        {
            cursor.offset = 5;
            *end_offset = 99;
        }
        assert!(matches!(
            validator.apply_delta(&delta),
            Err(AttachSessionError::MalformedRange { .. })
        ));
    }

    #[test]
    fn view_revision_changes_require_exactly_one_scene_replacement() {
        let mut validator = AttachContinuityValidator::default();
        validator.apply_snapshot(&snapshot()).unwrap();
        let no_scene = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(3),
            changes: vec![AttachProviderChange::Status {
                message: "changed".to_string(),
            }],
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&no_scene),
            Err(AttachSessionError::InvalidRevisionChange { .. })
        ));

        let stale_scene = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(2),
            changes: vec![AttachProviderChange::Scene(scene())],
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&stale_scene),
            Err(AttachSessionError::InvalidRevisionChange { .. })
        ));

        let multiple = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(3),
            changes: vec![
                AttachProviderChange::Scene(scene()),
                AttachProviderChange::Scene(scene()),
            ],
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&multiple),
            Err(AttachSessionError::MultipleSceneChanges)
        ));
    }

    #[test]
    fn repair_allows_generation_advance_but_not_regression() {
        let mut validator = AttachContinuityValidator::default();
        validator.apply_snapshot(&snapshot()).unwrap();
        let repaired = AttachStreamCursor {
            stream_id: stream_id(),
            surface_id: Uuid::nil(),
            generation: 2,
            offset: 10,
        };
        let delta = AttachProviderDelta {
            sequence: AttachDeltaSequence(8),
            base_view_revision: AttachViewRevision(2),
            view_revision: AttachViewRevision(3),
            changes: vec![
                AttachProviderChange::Scene(scene()),
                AttachProviderChange::StreamRepair(AttachStreamSnapshot {
                    cursor: repaired.clone(),
                    snapshot: b"full".to_vec(),
                }),
            ],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(3),
                event_sequence: AttachDeltaSequence(8),
                streams: vec![repaired],
                provider_token: Vec::new(),
            },
        };
        validator.apply_delta(&delta).unwrap();

        let regressed = AttachProviderDelta {
            sequence: AttachDeltaSequence(9),
            base_view_revision: AttachViewRevision(3),
            view_revision: AttachViewRevision(3),
            changes: vec![AttachProviderChange::StreamRepair(AttachStreamSnapshot {
                cursor: cursor(11),
                snapshot: Vec::new(),
            })],
            resume: AttachResumeState::default(),
        };
        assert!(matches!(
            validator.apply_delta(&regressed),
            Err(AttachSessionError::GenerationRegression { .. })
        ));
    }

    #[test]
    fn control_validator_orders_commands_fences_generations_and_detaches_idempotently() {
        let mut continuity = AttachContinuityValidator::default();
        continuity.apply_snapshot(&snapshot()).unwrap();
        let mut control = AttachControlValidator::default();
        let input = AttachProviderInput {
            command_sequence: 1,
            stream_id: stream_id(),
            generation: 1,
            payload: AttachInputPayload::Bytes(b"x".to_vec()),
        };
        control.validate_input(&input, &continuity).unwrap();
        assert!(matches!(
            control.validate_input(&input, &continuity),
            Err(AttachSessionError::CommandSequenceRegression { .. })
        ));
        let stale = AttachProviderInput {
            command_sequence: 2,
            generation: 0,
            ..input
        };
        assert!(matches!(
            control.validate_input(&stale, &continuity),
            Err(AttachSessionError::GenerationMismatch { .. })
        ));
        // Failed validation must not consume sequence 2.
        control
            .validate_viewport(&AttachProviderViewport {
                command_sequence: 2,
                columns: 80,
                rows: 24,
                pixel_width: None,
                pixel_height: None,
            })
            .unwrap();
        let action = AttachProviderAction {
            command_id: "action-1".to_string(),
            action: "focus-next".to_string(),
            arguments: Vec::new(),
        };
        control.validate_action(&action).unwrap();
        assert!(matches!(
            control.validate_action(&action),
            Err(AttachSessionError::DuplicateCommand { .. })
        ));
        assert_eq!(control.detach(), AttachDetachOutcome::Detached);
        assert_eq!(control.detach(), AttachDetachOutcome::AlreadyDetached);
        assert!(matches!(
            control.validate_viewport(&AttachProviderViewport {
                command_sequence: 3,
                columns: 80,
                rows: 24,
                pixel_width: None,
                pixel_height: None,
            }),
            Err(AttachSessionError::Detached)
        ));
    }

    #[test]
    fn duplicate_streams_and_inconsistent_resume_are_rejected() {
        let mut validator = AttachContinuityValidator::default();
        let mut duplicate = snapshot();
        duplicate.streams.push(duplicate.streams[0].clone());
        assert!(matches!(
            validator.apply_snapshot(&duplicate),
            Err(AttachSessionError::DuplicateStream { .. })
        ));

        let mut invalid = snapshot();
        invalid.resume.streams[0].offset = 4;
        assert!(matches!(
            validator.apply_snapshot(&invalid),
            Err(AttachSessionError::InvalidResume { .. })
        ));
    }
}
