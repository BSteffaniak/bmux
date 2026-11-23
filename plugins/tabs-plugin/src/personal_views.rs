//! Durable, owner-scoped personal tab views (`tabs-personal-views/v1`,
//! `tabs-personal-commands/v1`).
//!
//! A personal view is an arrangement of references to shared tabs. It never
//! decides a tab's lifetime: removing a reference leaves the tab running and
//! discoverable through the shared catalog.
//!
//! # Ownership
//!
//! The owner is the principal the host bound to the invoking client, resolved
//! through `clients-identity/v1.current-principal` ([`resolve_caller_owner`]).
//! No request carries an owner. Callers without a principal (including every
//! background task, which has no client) get `no-principal`; they never fall
//! back to the nil UUID or a connection id. Host-side code that already holds
//! a resolved owner uses [`Repository`] directly with an [`Owner`].
//!
//! View ids are scoped to their owner: one owner can never observe, collide
//! with or mutate another owner's view, so a foreign view id is `not-found`.
//!
//! # Storage layout
//!
//! Plugin storage offers only atomic whole-value replace per key, no listing
//! and no delete. The repository therefore keeps:
//!
//! - one document per owner under `tabs.personal.v1.<principal-uuid>`, holding
//!   the owner's views, default view and retained mutation outcomes. Every
//!   mutation and its idempotency record commit together in one write of this
//!   key, so a crash leaves either the old or the new document, never a
//!   partial mutation;
//! - an owner index under `tabs.personal-index.v1`, listing every owner that
//!   may have a document. Only deletion pruning and snapshots enumerate owners
//!   through it; requests address their own document directly.
//!
//! **Crash recovery between the two keys.** Before an owner's document is
//! written, the owner is added to the index (index first, document second),
//! and owners are never removed from the index. A crash between the writes
//! therefore leaves an index entry without a document, which every reader
//! treats as an owner with no views yet: the entry is a hint, and a missing
//! document is exactly the state before that owner's first commit. The
//! reverse (a document the index does not list) cannot be produced by these
//! writes; if it arises anyway (for example an externally copied file), the
//! next mutation by that owner re-adds the index entry before writing, and
//! until then the document is only invisible to pruning and snapshots.
//!
//! Every document and the index carry a schema version. An unknown version is
//! `unsupported-version` and an undecodable or inconsistent value is
//! `corrupt-state`; neither is ever replaced by an apparently empty state.
//!
//! # Deletion pruning
//!
//! Confirmed deletion (`ContextEvent::Closed`) removes the context from every
//! indexed owner's views ([`Repository::prune_deleted_context`]). The removal is
//! idempotent. A `Closed` missed while the server was down leaves a dangling
//! reference, which is retained like any merely absent tab: projection filters
//! references to tabs that are not live (matching `arrangements.rs`), so a
//! dangling id never becomes an interactive tab.
use crate::durable_storage::ArrangementMutation;
use bmux_plugin::{HostRuntimeApi, PluginStateRegistry, ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::{
    NativeLifecycleContext, PluginEventKind, StatefulPlugin, StatefulPluginError,
    StatefulPluginHandle, StatefulPluginResult, StatefulPluginSnapshot, StorageGetRequest,
    StorageSetRequest,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use bmux_tabs_plugin_api::tabs_personal_commands::PersonalAck;
use bmux_tabs_plugin_api::tabs_personal_views::{
    PersonalMember, PersonalView, PersonalViewError, PersonalViewSummary, PersonalWorkspace,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use uuid::Uuid;

/// Schema version of an owner document and of snapshot payloads.
pub const SCHEMA_VERSION: u32 = 1;
const INDEX_SCHEMA_VERSION: u32 = 1;
pub const MAX_VIEWS: usize = 64;
pub const MAX_WORKSPACES_PER_VIEW: usize = 256;
pub const MAX_MEMBERS_PER_VIEW: usize = 1024;
/// UTF-8 bytes per view name or member label.
pub const MAX_TEXT_BYTES: usize = 256;
/// One owner document, encoded. Storage values travel in one host transport
/// buffer of 1 MiB together with their request envelope, so the document
/// keeps a small reserve below that bound instead of failing at the host.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024 - 4096;
pub const MAX_OUTCOMES: usize = 4096;
/// Outcomes are retained at least this long; older ones are pruned on the
/// owner's next commit.
pub const OUTCOME_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const DEFAULT_VIEW_NAME: &str = "default";
const SNAPSHOT_ID: PluginEventKind = PluginEventKind::from_static("bmux.tabs/personal-views");

/// Serializes every personal-view read-modify-write, in process and across
/// processes sharing the installation.
static PERSONAL_VIEW_MUTATION: ArrangementMutation = ArrangementMutation::personal_views();

type Result<T> = std::result::Result<T, PersonalViewError>;

fn corrupt(reason: impl std::fmt::Display) -> PersonalViewError {
    PersonalViewError::CorruptState {
        reason: reason.to_string(),
    }
}

fn unavailable(reason: impl std::fmt::Display) -> PersonalViewError {
    PersonalViewError::StorageUnavailable {
        reason: reason.to_string(),
    }
}

fn limit(name: &str) -> PersonalViewError {
    PersonalViewError::LimitExceeded {
        limit: name.to_string(),
    }
}

/// A resolved, non-nil principal owning personal views.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Owner(Uuid);

impl Owner {
    /// Wrap a principal the host already resolved. The nil UUID is never an
    /// owner.
    pub fn from_resolved_principal(principal_id: Uuid) -> Option<Self> {
        (!principal_id.is_nil()).then_some(Self(principal_id))
    }
}

/// How resolving the invoking client's principal ended.
#[derive(Debug)]
pub enum OwnerResolution {
    Owner(Owner),
    /// The identity authority answered: this caller has no principal (no
    /// client, an unregistered client, or a nil principal).
    NoPrincipal,
    /// The identity authority could not be reached.
    Unavailable(String),
}

/// Resolve the owner from the host-bound caller only.
///
/// `current-principal` is not `@reentrant`. That only forbids re-entering a
/// plugin instance already live on the same call chain; this call goes from
/// the tabs instance to the clients instance, which never calls tabs, so it
/// is an ordinary nested call. The host keeps the original invoking client on
/// nested calls, so clients resolves the same caller tabs is serving.
pub fn resolve_caller(caller: &(impl ServiceCaller + Sync)) -> OwnerResolution {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    match bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_identity::client::current_principal(&mut client),
    ) {
        Ok(Ok(principal)) => Owner::from_resolved_principal(principal.principal_id)
            .map_or(OwnerResolution::NoPrincipal, OwnerResolution::Owner),
        Ok(Err(_)) => OwnerResolution::NoPrincipal,
        Err(error) => OwnerResolution::Unavailable(error.to_string()),
    }
}

/// The invoking client's owner, or `no-principal`.
pub fn resolve_caller_owner(caller: &(impl ServiceCaller + Sync)) -> Result<Owner> {
    match resolve_caller(caller) {
        OwnerResolution::Owner(owner) => Ok(owner),
        OwnerResolution::NoPrincipal | OwnerResolution::Unavailable(_) => {
            Err(PersonalViewError::NoPrincipal)
        }
    }
}

// ----- Durable model -----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerDocument {
    schema_version: u32,
    owner: Uuid,
    default_view: Option<Uuid>,
    views: Vec<StoredView>,
    /// Successful mutations, oldest first.
    outcomes: Vec<StoredOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredView {
    view_id: Uuid,
    name: String,
    revision: u64,
    workspaces: Vec<StoredWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWorkspace {
    workspace_id: Uuid,
    initialized: bool,
    members: Vec<StoredMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMember {
    member_id: Uuid,
    context_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOutcome {
    mutation_id: Uuid,
    /// Truncated SHA-256 of the canonical command payload.
    fingerprint: String,
    recorded_at_ms: u64,
    view_id: Uuid,
    revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    member_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerIndex {
    schema_version: u32,
    owners: BTreeSet<Uuid>,
}

#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u32,
}

fn check_text(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(limit("text-bytes"));
    }
    Ok(())
}

impl OwnerDocument {
    const fn empty(owner: Owner) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            owner: owner.0,
            default_view: None,
            views: Vec::new(),
            outcomes: Vec::new(),
        }
    }

    fn decode(owner: Owner, bytes: &[u8]) -> Result<Self> {
        let probe: VersionProbe = serde_json::from_slice(bytes).map_err(corrupt)?;
        if probe.schema_version != SCHEMA_VERSION {
            return Err(PersonalViewError::UnsupportedVersion {
                schema_version: probe.schema_version,
            });
        }
        let document: Self = serde_json::from_slice(bytes).map_err(corrupt)?;
        document.validate(owner)?;
        Ok(document)
    }

    /// Reject anything no writer commits. Exceeded bounds in storage are
    /// corruption, not a reason to truncate.
    fn validate(&self, owner: Owner) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(PersonalViewError::UnsupportedVersion {
                schema_version: self.schema_version,
            });
        }
        if self.owner != owner.0 {
            return Err(corrupt("document belongs to another owner"));
        }
        if self.views.len() > MAX_VIEWS {
            return Err(corrupt("too many views"));
        }
        let mut views = BTreeSet::new();
        for view in &self.views {
            if !views.insert(view.view_id) {
                return Err(corrupt("duplicate view id"));
            }
            if view.name.len() > MAX_TEXT_BYTES || view.revision == 0 {
                return Err(corrupt("invalid view"));
            }
            view.validate()?;
        }
        if self.default_view.is_some_and(|id| !views.contains(&id)) {
            return Err(corrupt("default view does not exist"));
        }
        if self.outcomes.len() > MAX_OUTCOMES {
            return Err(corrupt("too many retained outcomes"));
        }
        let mut mutations = BTreeSet::new();
        for outcome in &self.outcomes {
            if !mutations.insert(outcome.mutation_id) || outcome.fingerprint.len() != 32 {
                return Err(corrupt("invalid retained outcome"));
            }
        }
        Ok(())
    }

    fn view(&self, view_id: Uuid) -> Result<&StoredView> {
        self.views
            .iter()
            .find(|view| view.view_id == view_id)
            .ok_or(PersonalViewError::NotFound)
    }

    fn view_mut(&mut self, view_id: Uuid) -> Result<&mut StoredView> {
        self.views
            .iter_mut()
            .find(|view| view.view_id == view_id)
            .ok_or(PersonalViewError::NotFound)
    }

    fn fresh_view_id(&self) -> Uuid {
        loop {
            let id = Uuid::new_v4();
            if !id.is_nil() && self.view(id).is_err() {
                return id;
            }
        }
    }
}

impl StoredView {
    fn validate(&self) -> Result<()> {
        if self.workspaces.len() > MAX_WORKSPACES_PER_VIEW {
            return Err(corrupt("too many workspace arrangements"));
        }
        if self.member_count() > MAX_MEMBERS_PER_VIEW {
            return Err(corrupt("too many members"));
        }
        let mut workspaces = BTreeSet::new();
        let mut members = BTreeSet::new();
        for workspace in &self.workspaces {
            if !workspaces.insert(workspace.workspace_id)
                || (!workspace.initialized && !workspace.members.is_empty())
            {
                return Err(corrupt("invalid workspace arrangement"));
            }
            let mut contexts = BTreeSet::new();
            for member in &workspace.members {
                if !members.insert(member.member_id) || !contexts.insert(member.context_id) {
                    return Err(corrupt("duplicate member or tab reference"));
                }
                if member
                    .label
                    .as_ref()
                    .is_some_and(|label| label.len() > MAX_TEXT_BYTES)
                {
                    return Err(corrupt("label too long"));
                }
            }
        }
        Ok(())
    }

    fn member_count(&self) -> usize {
        self.workspaces
            .iter()
            .map(|workspace| workspace.members.len())
            .sum()
    }

    fn workspace(&self, workspace_id: Uuid) -> Option<&StoredWorkspace> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
    }

    fn workspace_index(&self, workspace_id: Uuid) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| workspace.workspace_id == workspace_id)
    }

    /// `(workspace index, member index)` of a member.
    fn locate(&self, member_id: Uuid) -> Result<(usize, usize)> {
        self.workspaces
            .iter()
            .enumerate()
            .find_map(|(w, workspace)| {
                workspace
                    .members
                    .iter()
                    .position(|member| member.member_id == member_id)
                    .map(|m| (w, m))
            })
            .ok_or(PersonalViewError::InvalidReference)
    }

    fn fresh_member_id(&self) -> Uuid {
        loop {
            let id = Uuid::new_v4();
            if !id.is_nil() && self.locate(id).is_err() {
                return id;
            }
        }
    }

    const fn check_revision(&self, expected: u64) -> Result<()> {
        if self.revision == expected {
            Ok(())
        } else {
            Err(PersonalViewError::RevisionConflict {
                current_revision: self.revision,
            })
        }
    }

    fn bump(&mut self) -> Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| limit("revision"))?;
        Ok(())
    }

    /// Seed `workspace_id` once from the canonical shared order. Returns the
    /// arrangement's index. Never truncates: a seed exceeding the bounds
    /// fails with `limit-exceeded`.
    fn seed(&mut self, workspace_id: Uuid, catalog: &dyn Catalog) -> Result<usize> {
        if let Some(index) = self.workspace_index(workspace_id) {
            return Ok(index);
        }
        if self.workspaces.len() >= MAX_WORKSPACES_PER_VIEW {
            return Err(limit("workspace-arrangements"));
        }
        let mut seen = BTreeSet::new();
        let order: Vec<_> = catalog
            .canonical_order(workspace_id)?
            .into_iter()
            .filter(|id| seen.insert(*id))
            .collect();
        if self.member_count() + order.len() > MAX_MEMBERS_PER_VIEW {
            return Err(limit("members"));
        }
        let mut members = Vec::with_capacity(order.len());
        for context_id in order {
            let member_id = loop {
                let id = self.fresh_member_id();
                if members
                    .iter()
                    .all(|member: &StoredMember| member.member_id != id)
                {
                    break id;
                }
            };
            members.push(StoredMember {
                member_id,
                context_id,
                label: None,
            });
        }
        self.workspaces.push(StoredWorkspace {
            workspace_id,
            initialized: true,
            members,
        });
        Ok(self.workspaces.len() - 1)
    }

    fn to_wire(&self, owner: Uuid, default_view: Option<Uuid>) -> PersonalView {
        PersonalView {
            view_id: self.view_id,
            owner,
            name: self.name.clone(),
            schema_version: SCHEMA_VERSION,
            revision: self.revision,
            is_default: default_view == Some(self.view_id),
            workspaces: self
                .workspaces
                .iter()
                .map(|workspace| PersonalWorkspace {
                    workspace_id: workspace.workspace_id,
                    initialized: workspace.initialized,
                    members: workspace
                        .members
                        .iter()
                        .map(|member| PersonalMember {
                            member_id: member.member_id,
                            context_id: member.context_id,
                            label: member.label.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

const fn ack(view_id: Uuid, revision: u64, member_id: Option<Uuid>) -> PersonalAck {
    PersonalAck {
        view_id,
        revision,
        member_id,
    }
}

// ----- Commands -----

/// One owner-scoped mutation. Its serialized form is the canonical payload
/// whose fingerprint guards mutation-id reuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Command {
    EnsureDefault,
    CreateView {
        view_id: Uuid,
        name: String,
    },
    RenameView {
        view_id: Uuid,
        expected_revision: u64,
        name: String,
    },
    DeleteView {
        view_id: Uuid,
        expected_revision: u64,
    },
    SetDefaultView {
        view_id: Uuid,
    },
    InitializeWorkspace {
        view_id: Uuid,
        expected_revision: u64,
        workspace_id: Uuid,
    },
    AddTab {
        view_id: Uuid,
        expected_revision: u64,
        context_id: Uuid,
        before: Option<Uuid>,
    },
    RemoveTab {
        view_id: Uuid,
        expected_revision: u64,
        member_id: Uuid,
    },
    MoveTab {
        view_id: Uuid,
        expected_revision: u64,
        member_id: Uuid,
        before: Option<Uuid>,
    },
    SetLabel {
        view_id: Uuid,
        expected_revision: u64,
        member_id: Uuid,
        label: Option<String>,
    },
}

impl Command {
    fn fingerprint(&self) -> Result<String> {
        let canonical = serde_json::to_vec(self).map_err(corrupt)?;
        Ok(Sha256::digest(canonical).iter().take(16).fold(
            String::with_capacity(32),
            |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            },
        ))
    }

    /// Apply to `document`. On error the caller discards `document`, so a
    /// failed precondition never reaches storage.
    #[allow(clippy::too_many_lines)] // One arm per command keeps each rule beside its op.
    fn execute(&self, document: &mut OwnerDocument, catalog: &dyn Catalog) -> Result<PersonalAck> {
        match self {
            Self::EnsureDefault => {
                if let Some(view_id) = document.default_view {
                    let view = document.view(view_id)?;
                    return Ok(ack(view.view_id, view.revision, None));
                }
                if document.views.len() >= MAX_VIEWS {
                    return Err(limit("views"));
                }
                let view_id = document.fresh_view_id();
                document.views.push(StoredView {
                    view_id,
                    name: DEFAULT_VIEW_NAME.into(),
                    revision: 1,
                    workspaces: Vec::new(),
                });
                document.default_view = Some(view_id);
                Ok(ack(view_id, 1, None))
            }
            Self::CreateView { view_id, name } => {
                check_text(name)?;
                if view_id.is_nil() {
                    return Err(PersonalViewError::InvalidReference);
                }
                if document.view(*view_id).is_ok() {
                    return Err(PersonalViewError::AlreadyExists);
                }
                if document.views.len() >= MAX_VIEWS {
                    return Err(limit("views"));
                }
                document.views.push(StoredView {
                    view_id: *view_id,
                    name: name.clone(),
                    revision: 1,
                    workspaces: Vec::new(),
                });
                Ok(ack(*view_id, 1, None))
            }
            Self::RenameView {
                view_id,
                expected_revision,
                name,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                check_text(name)?;
                if view.name != *name {
                    view.name.clone_from(name);
                    view.bump()?;
                }
                Ok(ack(*view_id, view.revision, None))
            }
            Self::DeleteView {
                view_id,
                expected_revision,
            } => {
                let view = document.view(*view_id)?;
                view.check_revision(*expected_revision)?;
                // The default must always name an existing view; choose
                // another default before deleting this one.
                if document.default_view == Some(*view_id) {
                    return Err(PersonalViewError::InvalidReference);
                }
                let revision = view.revision.saturating_add(1);
                document.views.retain(|view| view.view_id != *view_id);
                Ok(ack(*view_id, revision, None))
            }
            Self::SetDefaultView { view_id } => {
                let revision = document.view(*view_id)?.revision;
                document.default_view = Some(*view_id);
                Ok(ack(*view_id, revision, None))
            }
            Self::InitializeWorkspace {
                view_id,
                expected_revision,
                workspace_id,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                if view.workspace(*workspace_id).is_none() {
                    view.seed(*workspace_id, catalog)?;
                    view.bump()?;
                }
                Ok(ack(*view_id, view.revision, None))
            }
            Self::AddTab {
                view_id,
                expected_revision,
                context_id,
                before,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                let workspace_id = catalog
                    .live_workspace_of(*context_id)?
                    .ok_or(PersonalViewError::InvalidReference)?;
                let seeded = view.workspace(workspace_id).is_none();
                let index = view.seed(workspace_id, catalog)?;
                if let Some(existing) = view.workspaces[index]
                    .members
                    .iter()
                    .find(|member| member.context_id == *context_id)
                {
                    // Duplicate: the existing reference, unmoved.
                    let member_id = existing.member_id;
                    if seeded {
                        view.bump()?;
                    }
                    return Ok(ack(*view_id, view.revision, Some(member_id)));
                }
                let position = match before {
                    Some(before) => view.workspaces[index]
                        .members
                        .iter()
                        .position(|member| member.member_id == *before)
                        .ok_or(PersonalViewError::InvalidReference)?,
                    None => view.workspaces[index].members.len(),
                };
                if view.member_count() >= MAX_MEMBERS_PER_VIEW {
                    return Err(limit("members"));
                }
                let member_id = view.fresh_member_id();
                view.workspaces[index].members.insert(
                    position,
                    StoredMember {
                        member_id,
                        context_id: *context_id,
                        label: None,
                    },
                );
                view.bump()?;
                Ok(ack(*view_id, view.revision, Some(member_id)))
            }
            Self::RemoveTab {
                view_id,
                expected_revision,
                member_id,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                let (w, m) = view.locate(*member_id)?;
                // The arrangement stays initialized even when emptied.
                view.workspaces[w].members.remove(m);
                view.bump()?;
                Ok(ack(*view_id, view.revision, Some(*member_id)))
            }
            Self::MoveTab {
                view_id,
                expected_revision,
                member_id,
                before,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                let (w, m) = view.locate(*member_id)?;
                let members = &mut view.workspaces[w].members;
                if let Some(before) = before
                    && !members.iter().any(|member| member.member_id == *before)
                {
                    return Err(PersonalViewError::InvalidReference);
                }
                if *before == Some(*member_id) {
                    return Ok(ack(*view_id, view.revision, Some(*member_id)));
                }
                let original: Vec<_> = members.iter().map(|member| member.member_id).collect();
                let moving = members.remove(m);
                let position = before
                    .and_then(|before| members.iter().position(|member| member.member_id == before))
                    .unwrap_or(members.len());
                members.insert(position, moving);
                let changed = members
                    .iter()
                    .map(|member| member.member_id)
                    .ne(original.iter().copied());
                if changed {
                    view.bump()?;
                }
                Ok(ack(*view_id, view.revision, Some(*member_id)))
            }
            Self::SetLabel {
                view_id,
                expected_revision,
                member_id,
                label,
            } => {
                let view = document.view_mut(*view_id)?;
                view.check_revision(*expected_revision)?;
                if let Some(label) = label {
                    check_text(label)?;
                }
                let (w, m) = view.locate(*member_id)?;
                let member = &mut view.workspaces[w].members[m];
                if member.label != *label {
                    member.label.clone_from(label);
                    view.bump()?;
                }
                Ok(ack(*view_id, view.revision, Some(*member_id)))
            }
        }
    }
}

/// The shared tab catalog a mutation consults: which live tab belongs to
/// which workspace, and each workspace's canonical shared order.
pub trait Catalog {
    /// The workspace of a live tab, `None` when the tab is not live.
    ///
    /// # Errors
    /// The authority could not be read.
    fn live_workspace_of(&self, context_id: Uuid) -> Result<Option<Uuid>>;

    /// Live tabs of a workspace in the canonical shared order
    /// (`tabs.arrangements.v2`, projected over live contexts).
    ///
    /// # Errors
    /// The authority could not be read.
    fn canonical_order(&self, workspace_id: Uuid) -> Result<Vec<Uuid>>;
}

/// The production catalog: live contexts from the contexts plugin and the
/// canonical order through the existing arrangement helpers. Contexts are
/// listed at most once per mutation.
pub struct LiveCatalog<'a, C: ?Sized> {
    caller: &'a C,
    contexts: std::cell::OnceCell<Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary>>,
}

impl<'a, C: HostRuntimeApi + Sync> LiveCatalog<'a, C> {
    pub const fn new(caller: &'a C) -> Self {
        Self {
            caller,
            contexts: std::cell::OnceCell::new(),
        }
    }

    fn contexts(&self) -> Result<&[bmux_contexts_plugin_api::contexts_state::ContextSummary]> {
        if let Some(contexts) = self.contexts.get() {
            return Ok(contexts);
        }
        let contexts = super::list_contexts(self.caller).map_err(unavailable)?;
        Ok(self.contexts.get_or_init(|| contexts))
    }
}

impl<C: HostRuntimeApi + Sync> Catalog for LiveCatalog<'_, C> {
    fn live_workspace_of(&self, context_id: Uuid) -> Result<Option<Uuid>> {
        Ok(self
            .contexts()?
            .iter()
            .find(|context| context.id == context_id)
            .map(super::context_workspace_id))
    }

    fn canonical_order(&self, workspace_id: Uuid) -> Result<Vec<Uuid>> {
        let live = super::filter_contexts_for_workspace(self.contexts()?.to_vec(), workspace_id);
        let stored = super::get_stored_tab_order_ids_for_workspace(self.caller, workspace_id)
            .map_err(unavailable)?;
        Ok(super::project_tab_order_ids(stored, &live))
    }
}

/// What the creation hook did with a newly created tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreatedTabJoin {
    Joined {
        view_id: Uuid,
        member_id: Uuid,
    },
    /// A retry: the tab already has a member.
    AlreadyMember {
        view_id: Uuid,
        member_id: Uuid,
    },
    /// The caller has no principal, or no view to join.
    NoView,
    /// The view never arranged this workspace; seeding it later includes the
    /// tab from the canonical order.
    WorkspaceNotInitialized {
        view_id: Uuid,
    },
}

/// The view a tab created by `owner` joins. Phase 3 replaces this with the
/// creating attachment's bound view; today it is the owner's default view.
const fn creation_target_view(document: &OwnerDocument) -> Option<Uuid> {
    document.default_view
}

// ----- Repository -----

/// Owner-keyed access to personal views. It never resolves an owner itself,
/// so host-internal code (attachment binding, lifecycle observers) can call
/// it with an owner resolved elsewhere.
pub struct Repository<'a, S: ?Sized> {
    storage: &'a S,
    states: Option<&'a PluginStateRegistry>,
}

impl<'a, S: HostRuntimeApi + ?Sized> Repository<'a, S> {
    pub const fn new(storage: &'a S, states: Option<&'a PluginStateRegistry>) -> Self {
        Self { storage, states }
    }

    fn load(&self, owner: Owner) -> Result<Option<OwnerDocument>> {
        self.storage
            .storage_get(&StorageGetRequest::new(document_key(owner)))
            .map_err(unavailable)?
            .value
            .map(|bytes| OwnerDocument::decode(owner, &bytes))
            .transpose()
    }

    fn load_or_empty(&self, owner: Owner) -> Result<OwnerDocument> {
        Ok(self
            .load(owner)?
            .unwrap_or_else(|| OwnerDocument::empty(owner)))
    }

    fn read_index(&self) -> Result<OwnerIndex> {
        let Some(bytes) = self
            .storage
            .storage_get(&StorageGetRequest::new(index_key()))
            .map_err(unavailable)?
            .value
        else {
            return Ok(OwnerIndex {
                schema_version: INDEX_SCHEMA_VERSION,
                owners: BTreeSet::new(),
            });
        };
        let probe: VersionProbe = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if probe.schema_version != INDEX_SCHEMA_VERSION {
            return Err(PersonalViewError::UnsupportedVersion {
                schema_version: probe.schema_version,
            });
        }
        let index: OwnerIndex = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if index.owners.contains(&Uuid::nil()) {
            return Err(corrupt("owner index lists the nil principal"));
        }
        Ok(index)
    }

    fn write_index(&self, index: &OwnerIndex) -> Result<()> {
        let bytes = serde_json::to_vec(index).map_err(corrupt)?;
        self.storage
            .storage_set(&StorageSetRequest::new(index_key(), bytes))
            .map_err(unavailable)
    }

    /// Index first: an owner is listed before its document can exist.
    fn ensure_indexed(&self, owner: Owner) -> Result<()> {
        let mut index = self.read_index()?;
        if index.owners.insert(owner.0) {
            self.write_index(&index)?;
        }
        Ok(())
    }

    fn encode(document: &OwnerDocument) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(document).map_err(corrupt)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(limit("document-bytes"));
        }
        Ok(bytes)
    }

    /// One atomic replace of the owner's document, after indexing the owner.
    fn commit(&self, owner: Owner, document: &OwnerDocument) -> Result<()> {
        let bytes = Self::encode(document)?;
        self.ensure_indexed(owner)?;
        self.storage
            .storage_set(&StorageSetRequest::new(document_key(owner), bytes))
            .map_err(unavailable)?;
        self.mark_snapshot_dirty();
        Ok(())
    }

    fn mark_snapshot_dirty(&self) {
        if let Some(flag) = self
            .states
            .and_then(PluginStateRegistry::get::<bmux_snapshot_runtime::SnapshotDirtyFlagHandle>)
            && let Ok(flag) = flag.read()
        {
            flag.0.mark_dirty();
        }
    }

    /// The owner's views, without members. Never writes.
    ///
    /// # Errors
    /// Storage, version or corruption errors.
    pub fn list_views(&self, owner: Owner) -> Result<Vec<PersonalViewSummary>> {
        let Some(document) = self.load(owner)? else {
            return Ok(Vec::new());
        };
        Ok(document
            .views
            .iter()
            .map(|view| PersonalViewSummary {
                view_id: view.view_id,
                owner: owner.0,
                name: view.name.clone(),
                revision: view.revision,
                is_default: document.default_view == Some(view.view_id),
            })
            .collect())
    }

    /// One of the owner's views. Never writes.
    ///
    /// # Errors
    /// `not-found` for a view this owner does not have, whoever else has it.
    pub fn get_view(&self, owner: Owner, view_id: Uuid) -> Result<PersonalView> {
        let document = self.load(owner)?.ok_or(PersonalViewError::NotFound)?;
        Ok(document
            .view(view_id)?
            .to_wire(owner.0, document.default_view))
    }

    /// The owner's default view. Never writes.
    ///
    /// # Errors
    /// `not-initialized` until `ensure-default` (or `set-default-view`).
    pub fn default_view(&self, owner: Owner) -> Result<PersonalView> {
        let document = self.load(owner)?.ok_or(PersonalViewError::NotInitialized)?;
        let view_id = document
            .default_view
            .ok_or(PersonalViewError::NotInitialized)?;
        Ok(document
            .view(view_id)?
            .to_wire(owner.0, document.default_view))
    }

    /// Apply one idempotent mutation for `owner` at time `now_ms`.
    ///
    /// A recorded `mutation_id` replays its acknowledgement when the payload
    /// matches (before any revision is re-evaluated) and is a
    /// `mutation-conflict` otherwise. A new mutation commits together with
    /// its outcome in one document write before returning.
    ///
    /// # Errors
    /// Any precondition, bound, storage, version or corruption error; none of
    /// them changes storage.
    pub fn apply(
        &self,
        owner: Owner,
        mutation_id: Uuid,
        command: &Command,
        catalog: &dyn Catalog,
        now_ms: u64,
    ) -> Result<PersonalAck> {
        let fingerprint = command.fingerprint()?;
        let _guard = PERSONAL_VIEW_MUTATION.lock().map_err(unavailable)?;
        let mut document = self.load_or_empty(owner)?;
        if let Some(outcome) = document
            .outcomes
            .iter()
            .find(|outcome| outcome.mutation_id == mutation_id)
        {
            return if outcome.fingerprint == fingerprint {
                Ok(ack(outcome.view_id, outcome.revision, outcome.member_id))
            } else {
                Err(PersonalViewError::MutationConflict)
            };
        }
        let result = command.execute(&mut document, catalog)?;
        // Deterministic retention: drop only outcomes at least 24 h old, and
        // never evict a younger one to make room.
        document
            .outcomes
            .retain(|outcome| outcome.recorded_at_ms.saturating_add(OUTCOME_RETENTION_MS) > now_ms);
        if document.outcomes.len() >= MAX_OUTCOMES {
            return Err(limit("retained-outcomes"));
        }
        document.outcomes.push(StoredOutcome {
            mutation_id,
            fingerprint,
            recorded_at_ms: now_ms,
            view_id: result.view_id,
            revision: result.revision,
            member_id: result.member_id,
        });
        self.commit(owner, &document)?;
        Ok(result)
    }

    /// Add a tab its owner just created to the owner's creation target view
    /// ([`creation_target_view`]), at the end of its workspace arrangement.
    /// Idempotent by content: a retry finds the existing member. Joins only
    /// an initialized arrangement; an uninitialized one picks the tab up from
    /// the canonical order when it is seeded.
    ///
    /// # Errors
    /// Storage, bound, version or corruption errors.
    pub fn join_created_tab(
        &self,
        owner: Owner,
        context_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<CreatedTabJoin> {
        let _guard = PERSONAL_VIEW_MUTATION.lock().map_err(unavailable)?;
        let Some(mut document) = self.load(owner)? else {
            return Ok(CreatedTabJoin::NoView);
        };
        let Some(view_id) = creation_target_view(&document) else {
            return Ok(CreatedTabJoin::NoView);
        };
        let view = document.view_mut(view_id)?;
        let Some(index) = view.workspace_index(workspace_id) else {
            return Ok(CreatedTabJoin::WorkspaceNotInitialized { view_id });
        };
        if let Some(member) = view.workspaces[index]
            .members
            .iter()
            .find(|member| member.context_id == context_id)
        {
            return Ok(CreatedTabJoin::AlreadyMember {
                view_id,
                member_id: member.member_id,
            });
        }
        if view.member_count() >= MAX_MEMBERS_PER_VIEW {
            return Err(limit("members"));
        }
        let member_id = view.fresh_member_id();
        view.workspaces[index].members.push(StoredMember {
            member_id,
            context_id,
            label: None,
        });
        view.bump()?;
        self.commit(owner, &document)?;
        Ok(CreatedTabJoin::Joined { view_id, member_id })
    }

    /// Remove a confirmed-deleted tab from every indexed owner's views.
    /// Idempotent; owners already without the reference are not rewritten.
    /// One owner's failure does not stop the others.
    ///
    /// # Errors
    /// The index could not be read, or one or more owners failed (each
    /// listed with its error).
    pub fn prune_deleted_context(&self, context_id: Uuid) -> std::result::Result<usize, String> {
        let _guard = PERSONAL_VIEW_MUTATION
            .lock()
            .map_err(|error| format!("personal views unavailable: {error}"))?;
        let index = self
            .read_index()
            .map_err(|error| format!("personal view index: {error:?}"))?;
        let mut pruned = 0;
        let mut failures = Vec::new();
        for owner in index
            .owners
            .iter()
            .copied()
            .filter_map(Owner::from_resolved_principal)
        {
            let prune = || -> Result<bool> {
                let Some(mut document) = self.load(owner)? else {
                    return Ok(false);
                };
                let mut changed = false;
                for view in &mut document.views {
                    let before = view.member_count();
                    for workspace in &mut view.workspaces {
                        workspace
                            .members
                            .retain(|member| member.context_id != context_id);
                    }
                    if view.member_count() != before {
                        view.bump()?;
                        changed = true;
                    }
                }
                if changed {
                    self.commit(owner, &document)?;
                }
                Ok(changed)
            };
            match prune() {
                Ok(true) => pruned += 1,
                Ok(false) => {}
                Err(error) => failures.push(format!("{}: {error:?}", owner.0)),
            }
        }
        if failures.is_empty() {
            Ok(pruned)
        } else {
            Err(format!(
                "pruning deleted tab {context_id} from personal views failed for {}",
                failures.join(", ")
            ))
        }
    }

    fn snapshot_state(&self) -> Result<SnapshotState> {
        let _guard = PERSONAL_VIEW_MUTATION.lock().map_err(unavailable)?;
        let mut documents = Vec::new();
        for owner in self
            .read_index()?
            .owners
            .into_iter()
            .filter_map(Owner::from_resolved_principal)
        {
            if let Some(document) = self.load(owner)? {
                documents.push(document);
            }
        }
        Ok(SnapshotState {
            schema_version: SCHEMA_VERSION,
            documents,
        })
    }

    /// Replace all personal views with a validated snapshot: index first,
    /// then each document; indexed owners absent from the snapshot are reset
    /// to an empty document so the result equals the snapshot.
    fn restore_state(&self, state: &SnapshotState) -> Result<()> {
        let encoded = state
            .documents
            .iter()
            .map(|document| Ok((Owner(document.owner), Self::encode(document)?)))
            .collect::<Result<Vec<_>>>()?;
        let _guard = PERSONAL_VIEW_MUTATION.lock().map_err(unavailable)?;
        let mut index = self.read_index()?;
        let previous = index.owners.clone();
        index
            .owners
            .extend(state.documents.iter().map(|document| document.owner));
        if index.owners != previous {
            self.write_index(&index)?;
        }
        let restored: BTreeSet<_> = state
            .documents
            .iter()
            .map(|document| document.owner)
            .collect();
        for (owner, bytes) in encoded {
            self.storage
                .storage_set(&StorageSetRequest::new(document_key(owner), bytes))
                .map_err(unavailable)?;
        }
        for owner in previous
            .difference(&restored)
            .copied()
            .filter_map(Owner::from_resolved_principal)
        {
            if self
                .storage
                .storage_get(&StorageGetRequest::new(document_key(owner)))
                .map_err(unavailable)?
                .value
                .is_some()
            {
                let bytes = Self::encode(&OwnerDocument::empty(owner))?;
                self.storage
                    .storage_set(&StorageSetRequest::new(document_key(owner), bytes))
                    .map_err(unavailable)?;
            }
        }
        self.mark_snapshot_dirty();
        Ok(())
    }
}

fn document_key(owner: Owner) -> bmux_plugin_sdk::StorageKey {
    super::storage_key(&format!("tabs.personal.v1.{}", owner.0.hyphenated()))
}

fn index_key() -> bmux_plugin_sdk::StorageKey {
    bmux_plugin_sdk::storage_key!("tabs.personal-index.v1")
}

// ----- Caller-scoped service entry points -----

/// `list-views` for the invoking client's owner.
pub fn list_views_for_caller(
    caller: &(impl HostRuntimeApi + Sync),
) -> Result<Vec<PersonalViewSummary>> {
    let owner = resolve_caller_owner(caller)?;
    Repository::new(caller, None).list_views(owner)
}

/// `get-view` for the invoking client's owner.
pub fn get_view_for_caller(
    caller: &(impl HostRuntimeApi + Sync),
    view_id: Uuid,
) -> Result<PersonalView> {
    let owner = resolve_caller_owner(caller)?;
    Repository::new(caller, None).get_view(owner, view_id)
}

/// `default-view` for the invoking client's owner.
pub fn default_view_for_caller(caller: &(impl HostRuntimeApi + Sync)) -> Result<PersonalView> {
    let owner = resolve_caller_owner(caller)?;
    Repository::new(caller, None).default_view(owner)
}

/// One `tabs-personal-commands` operation for the invoking client's owner.
pub fn apply_for_caller(
    caller: &(impl HostRuntimeApi + Sync),
    states: &PluginStateRegistry,
    mutation_id: Uuid,
    command: &Command,
) -> Result<PersonalAck> {
    let owner = resolve_caller_owner(caller)?;
    Repository::new(caller, Some(states)).apply(
        owner,
        mutation_id,
        command,
        &LiveCatalog::new(caller),
        super::status::now_ms(),
    )
}

/// Creation hook: a tab created by a client with a principal joins that
/// principal's creation target view. Tabs created without a caller join no
/// view. The tab already exists and stays running whatever this returns.
///
/// # Errors
/// The identity authority was unreachable, or the view update failed.
pub fn join_created_tab_for_caller(
    caller: &(impl HostRuntimeApi + Sync),
    states: &PluginStateRegistry,
    context_id: Uuid,
    workspace_id: Uuid,
) -> std::result::Result<CreatedTabJoin, String> {
    let owner = match resolve_caller(caller) {
        OwnerResolution::Owner(owner) => owner,
        OwnerResolution::NoPrincipal => return Ok(CreatedTabJoin::NoView),
        OwnerResolution::Unavailable(error) => {
            return Err(format!("resolving the creating principal failed: {error}"));
        }
    };
    Repository::new(caller, Some(states))
        .join_created_tab(owner, context_id, workspace_id)
        .map_err(|error| format!("adding the new tab to its personal view failed: {error:?}"))
}

/// Remove a confirmed-deleted tab from every owner's views.
///
/// # Errors
/// See [`Repository::prune_deleted_context`].
pub fn prune_deleted_context(
    caller: &(impl HostRuntimeApi + ?Sized),
    states: &PluginStateRegistry,
    context_id: Uuid,
) -> std::result::Result<usize, String> {
    Repository::new(caller, Some(states)).prune_deleted_context(context_id)
}

// ----- Snapshot participant -----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotState {
    schema_version: u32,
    documents: Vec<OwnerDocument>,
}

fn decode_snapshot(snapshot: &StatefulPluginSnapshot) -> Result<SnapshotState> {
    if snapshot.version != 1 {
        return Err(PersonalViewError::UnsupportedVersion {
            schema_version: snapshot.version,
        });
    }
    let probe: VersionProbe = serde_json::from_slice(&snapshot.bytes).map_err(corrupt)?;
    if probe.schema_version != SCHEMA_VERSION {
        return Err(PersonalViewError::UnsupportedVersion {
            schema_version: probe.schema_version,
        });
    }
    let state: SnapshotState = serde_json::from_slice(&snapshot.bytes).map_err(corrupt)?;
    let mut owners = BTreeSet::new();
    for document in &state.documents {
        let owner = Owner::from_resolved_principal(document.owner)
            .ok_or_else(|| corrupt("snapshot document has the nil owner"))?;
        if !owners.insert(owner) {
            return Err(corrupt("snapshot repeats an owner"));
        }
        document.validate(owner)?;
    }
    Ok(state)
}

/// Register the `bmux.tabs/personal-views` snapshot participant.
///
/// # Errors
/// The stateful registry lock is poisoned.
pub fn activate(
    context: &NativeLifecycleContext,
    states: &PluginStateRegistry,
) -> std::result::Result<(), String> {
    let stateful = bmux_snapshot_runtime::get_or_init_stateful_registry(
        || states.get::<StatefulPluginRegistry>(),
        |fresh| {
            states.register_communal::<StatefulPluginRegistry>(fresh);
        },
    );
    stateful
        .write()
        .map_err(|_| "stateful registry lock poisoned")?
        .push(StatefulPluginHandle::new(PersonalViews {
            caller: TypedServiceCaller::from_lifecycle_context(context),
        }));
    Ok(())
}

struct PersonalViews {
    caller: TypedServiceCaller,
}

fn snapshot_failed(details: impl std::fmt::Debug) -> StatefulPluginError {
    StatefulPluginError::SnapshotFailed {
        plugin: SNAPSHOT_ID.as_str().into(),
        details: format!("{details:?}"),
    }
}

fn restore_failed(details: impl std::fmt::Debug) -> StatefulPluginError {
    StatefulPluginError::RestoreFailed {
        plugin: SNAPSHOT_ID.as_str().into(),
        details: format!("{details:?}"),
    }
}

fn snapshot_bytes(
    repository: &Repository<'_, impl HostRuntimeApi + ?Sized>,
) -> StatefulPluginResult<StatefulPluginSnapshot> {
    let state = repository.snapshot_state().map_err(snapshot_failed)?;
    let bytes = serde_json::to_vec(&state).map_err(snapshot_failed)?;
    Ok(StatefulPluginSnapshot::new(SNAPSHOT_ID, 1, bytes))
}

fn validate(snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<SnapshotState> {
    if snapshot.version != 1 {
        return Err(StatefulPluginError::UnsupportedVersion {
            plugin: SNAPSHOT_ID.as_str().into(),
            version: snapshot.version,
            expected: vec![1],
        });
    }
    decode_snapshot(snapshot).map_err(restore_failed)
}

impl StatefulPlugin for PersonalViews {
    fn id(&self) -> PluginEventKind {
        SNAPSHOT_ID
    }

    fn restore_dependencies(&self) -> Vec<PluginEventKind> {
        vec![PluginEventKind::from_static("bmux.contexts/context-state")]
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        snapshot_bytes(&Repository::new(&self.caller, None))
    }

    fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        validate(snapshot).map(|_| ())
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        let state = validate(&snapshot)?;
        Repository::new(&self.caller, None)
            .restore_state(&state)
            .map_err(restore_failed)
    }
}

#[cfg(test)]
mod tests;
