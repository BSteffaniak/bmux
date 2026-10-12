//! Stable host-side contracts. These are standalone transport messages, not plugin IPC.
use crate::mirror::MirrorFrame;
use bmux_presentation::{Deck, NavigationAction, PresentationState};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractionMode {
    ReceiveOnly,
    SubmitOnly,
    Shared,
    Independent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scopes {
    pub notes: bool,
    pub navigate: bool,
    pub demo_input: bool,
    pub effects: bool,
}

/// A run ID changes on every host restart. Revisions never stand in for identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub run_id: String,
    pub deck_revision: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub cursor: Cursor,
    pub deck: Deck,
    pub state: PresentationState,
    /// Included only for a privately authenticated principal with the notes scope.
    pub notes: Option<String>,
    pub surfaces: Vec<MirrorFrame>,
    pub assets: Vec<AssetInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: String,
    pub media_type: String,
    pub bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SurfaceInput {
    pub surface_id: String,
    pub surface_revision: u64,
    pub x: u16,
    pub y: u16,
    pub input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Action {
    Navigate(NavigationAction),
    DemoInput(SurfaceInput),
    /// Executed on the owner, never by generic viewers. Not automatically replayed.
    Effect {
        name: String,
        input: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Submission {
    /// Strictly increasing per private credential, starting at 1. Retried IDs must
    /// carry the identical submission. Retention eviction never makes an ID fresh.
    pub action_id: u64,
    pub expected: Cursor,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acknowledgment {
    pub action_id: u64,
    pub cursor: Cursor,
    pub outcome: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Poll {
    Unchanged(Cursor),
    Snapshot {
        resync: bool,
        snapshot: Box<Snapshot>,
    },
}

// Deliberately no Debug: authentication secrets must not enter ordinary diagnostics.
#[derive(Serialize, Deserialize)]
pub enum Request {
    Authenticate {
        version: u16,
        credential: Option<String>,
        mode: InteractionMode,
    },
    /// Explicit bounded bootstrap/resync; allowed for submit-only participants.
    Snapshot,
    Poll {
        cursor: Option<Cursor>,
    },
    Submit(Submission),
    Asset {
        id: String,
        offset: usize,
        length: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Authenticated {
        participant: String,
        cursor: Cursor,
        mode: InteractionMode,
        next_action_id: Option<u64>,
    },
    Poll(Poll),
    Acknowledged(Acknowledgment),
    Asset {
        id: String,
        offset: usize,
        total: usize,
        bytes: Vec<u8>,
    },
    Error(HostError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum HostError {
    #[error("authentication required or invalid")]
    Authentication,
    #[error("operation not authorized")]
    Unauthorized,
    #[error("unsupported protocol version")]
    Version,
    #[error("resource limit exceeded")]
    Limit,
    #[error("stale input; fetch a fresh snapshot")]
    Stale,
    #[error("resource not found")]
    NotFound,
    #[error("invalid content")]
    Invalid,
    #[error("action identity reused with different payload")]
    ActionConflict,
    #[error("action outcome no longer retained; do not replay; reconcile explicitly")]
    OutcomeEvicted,
    #[error("action ID must be the next principal sequence number")]
    ActionSequence,
    #[error("owner operation failed; outcome may be uncertain; do not replay")]
    EffectUncertain,
}
