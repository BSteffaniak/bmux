//! Attachment bindings and per-attachment projections
//! (`tabs-personal-attachment/v1`, state kind `bmux.tabs/personal-view`).
//!
//! # Bindings
//!
//! Each attached client (keyed by its host-assigned client id) is bound to
//! one view of its owner: the bound view, a binding revision and the selected
//! member. Bindings are transient attachment-local state: never stored, never
//! replicated, and gone when the attachment is.
//!
//! A binding is created when a client attaches, by the refresh worker
//! ([`Attachments::refresh`]), or on the client's first
//! `tabs-personal-attachment` call, whichever comes first. A new binding is
//! bound to the owner's default view (`ensure-default`); the refresh then seeds
//! the attachment's active workspace once from the canonical order
//! (`initialize-workspace`). Every attach is a fresh attach, so no earlier
//! binding is resumed.
//!
//! # Owner without an invocation
//!
//! Inside an invocation the owner is resolved from the invoking client through
//! `clients-identity/v1` ([`super::resolve_caller`]). The refresh worker has no
//! invocation: it learns attached client ids from the host's neutral follow
//! state (`FollowStateHandle`: connected clients with a live attach stream) and
//! reads each one's principal from the host's `ClientPrincipalHandle`. The
//! server writes that map from the connection's own handshake before the
//! connection can send any request, keyed by the client id the server
//! assigned, and `clients-identity/v1.current-principal` reads the same map.
//! No request field ever names a client or an owner here.
//!
//! # Delivery
//!
//! Every binding's projection is published with
//! `WireEventSink::publish_retained_to` to that client only, deduplicated
//! against the last payload published to it. Bindings are retired, and their
//! retained value cleared, when the client's attach stream ends or the sink
//! reports the client disconnected.
use super::{
    Catalog, Command, LiveCatalog, Owner, OwnerResolution, PersonalViewError, Repository,
    resolve_caller,
};
use bmux_contexts_plugin_api::contexts_state::ContextSummary;
use bmux_plugin::{HostRuntimeApi, PluginStateRegistry};
use bmux_plugin_sdk::WireEventSink;
use bmux_tabs_plugin_api::personal_view::{
    PersonalViewMember, PersonalViewProjection, PersonalViewRemoval, PersonalViewWorkspace,
    STATE_KIND,
};
use bmux_tabs_plugin_api::tabs_personal_attachment::{PersonalAttachmentError, PersonalBinding};
use bmux_tabs_plugin_api::tabs_personal_commands::PersonalAck;
use bmux_tabs_plugin_api::tabs_personal_views::PersonalView;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use uuid::Uuid;

/// Largest encoded projection: the host retains at most 256 KiB per payload.
/// A larger projection is refused, never truncated.
pub const MAX_PROJECTION_BYTES: usize = 256 * 1024;

type Result<T> = std::result::Result<T, PersonalAttachmentError>;

/// What the host knows about attached clients, read without an invocation.
pub trait AttachmentHost: Send + Sync {
    /// Connected clients with a live attach stream, or `None` when the host
    /// cannot tell (then no binding is retired for lack of a stream).
    fn attached_clients(&self) -> Option<Vec<Uuid>>;
    /// Whether `client` has a live attach stream.
    fn is_attached(&self, client: Uuid) -> bool;
    /// The principal the server bound to `client` at its handshake.
    fn principal(&self, client: Uuid) -> Option<Uuid>;
    /// The client's selected tab (its contexts selection).
    fn selected_context(&self, client: Uuid) -> Option<Uuid>;
}

/// The production host: neutral state handles in the activation's registry.
pub struct RegistryHost<'a>(pub &'a PluginStateRegistry);

impl RegistryHost<'_> {
    fn follow(&self) -> Option<bmux_client_state::FollowStateHandle> {
        self.0
            .get::<bmux_client_state::FollowStateHandle>()
            .and_then(|handle| handle.read().ok().map(|handle| handle.clone()))
    }
}

impl AttachmentHost for RegistryHost<'_> {
    fn attached_clients(&self) -> Option<Vec<Uuid>> {
        let follow = self.follow()?;
        Some(
            follow
                .0
                .list_clients()
                .into_iter()
                .map(|client| client.id)
                .filter(|id| {
                    follow
                        .0
                        .attached_stream_session(bmux_session_models::ClientId(*id))
                        .is_some()
                })
                .collect(),
        )
    }

    fn is_attached(&self, client: Uuid) -> bool {
        self.follow().is_some_and(|follow| {
            follow
                .0
                .attached_stream_session(bmux_session_models::ClientId(client))
                .is_some()
        })
    }

    fn principal(&self, client: Uuid) -> Option<Uuid> {
        self.0
            .get::<bmux_client_state::ClientPrincipalHandle>()
            .and_then(|handle| handle.read().ok().map(|handle| handle.clone()))
            .and_then(|handle| handle.0.get(bmux_session_models::ClientId(client)))
            .filter(|principal| !principal.is_nil())
    }

    fn selected_context(&self, client: Uuid) -> Option<Uuid> {
        self.0
            .get::<bmux_context_state::ContextStateHandle>()
            .and_then(|handle| handle.read().ok().map(|handle| handle.clone()))
            .and_then(|handle| {
                handle
                    .0
                    .current_for_client(bmux_session_models::ClientId(client))
            })
            .map(|context| context.id)
    }
}

/// Selection of attached clients on their behalf, through the selection
/// authority (`contexts-client-selection-v1`): used when the tab an
/// attachment shows leaves what it arranges.
pub trait ClientSelection {
    /// Select `context` for `client`; its attach retargets to it.
    ///
    /// # Errors
    /// The selection authority refused or was unreachable.
    fn select_for(&self, client: Uuid, context: Uuid) -> std::result::Result<(), String>;
    /// Leave `client` with no selection; its attach stays connected with no
    /// input target until something selects a tab for it.
    ///
    /// # Errors
    /// The selection authority refused or was unreachable.
    fn clear_for(&self, client: Uuid) -> std::result::Result<(), String>;
}

/// The host wire sink published on an activation's registry, if any.
pub fn host_wire_sink(states: &PluginStateRegistry) -> Option<std::sync::Arc<dyn WireEventSink>> {
    states
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|sink| sink.read().ok().map(|sink| sink.0.clone()))
}

/// Everything one binding operation or refresh reads and writes.
pub struct Env<'a, C: ?Sized> {
    /// Storage, live contexts and the canonical order.
    pub caller: &'a C,
    pub states: Option<&'a PluginStateRegistry>,
    pub host: &'a dyn AttachmentHost,
    pub selection: &'a dyn ClientSelection,
    pub sink: Option<&'a dyn WireEventSink>,
    pub now_ms: u64,
}

/// An attachment's last removal of a tab from its bound view, which
/// `undo-remove-from-view` restores. Attachment-local and transient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub context_id: Uuid,
    pub view_id: Uuid,
    /// The name the attachment saw: its personal label, else the tab's name.
    pub name: String,
    /// The live tab that followed it in the arrangement, so undo restores
    /// its place.
    pub before: Option<Uuid>,
    /// Whether it was the attachment's selected tab, so undo selects it again.
    pub was_selected: bool,
    pub removed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    owner: Owner,
    view_id: Uuid,
    revision: u64,
    selected_member: Option<Uuid>,
    /// The workspace the attachment shows while it has no selection, after
    /// its arrangement there lost its last live tab.
    held_workspace: Option<Uuid>,
    last_removed: Option<Removal>,
    /// The last payload published to this client.
    published: Option<Vec<u8>>,
}

impl Binding {
    const fn to_wire(&self) -> PersonalBinding {
        PersonalBinding {
            view_id: self.view_id,
            revision: self.revision,
            selected_member: self.selected_member,
        }
    }

    /// Record a new view or selected member, advancing the revision only on a
    /// change.
    fn update(&mut self, view_id: Uuid, selected_member: Option<Uuid>) {
        if self.view_id != view_id || self.selected_member != selected_member {
            self.view_id = view_id;
            self.selected_member = selected_member;
            self.revision = self.revision.saturating_add(1);
        }
    }
}

/// Every attachment's binding, plus the wake signal of the refresh worker.
#[derive(Default)]
pub struct Attachments {
    bindings: Mutex<BTreeMap<Uuid, Binding>>,
    /// Serializes refreshes so publications for one client never reorder.
    refresh: Mutex<()>,
    wake: tokio::sync::Notify,
}

fn unavailable(reason: impl std::fmt::Display) -> PersonalAttachmentError {
    PersonalAttachmentError::Unavailable {
        reason: reason.to_string(),
    }
}

fn view_error(error: PersonalViewError) -> PersonalAttachmentError {
    match error {
        PersonalViewError::NoPrincipal => PersonalAttachmentError::NoPrincipal,
        PersonalViewError::NotFound => PersonalAttachmentError::NotFound,
        PersonalViewError::InvalidReference => PersonalAttachmentError::InvalidReference,
        other => unavailable(format!("{other:?}")),
    }
}

fn workspace_of(context: &ContextSummary) -> Uuid {
    crate::context_workspace_id(context)
}

/// The member of `view`'s arrangement for `workspace` that references the
/// live tab `selected`.
fn selected_member_of(
    view: &PersonalView,
    workspace: Uuid,
    selected: Option<Uuid>,
) -> Option<Uuid> {
    let selected = selected?;
    view.workspaces
        .iter()
        .find(|arrangement| arrangement.workspace_id == workspace)?
        .members
        .iter()
        .find(|member| member.context_id == selected)
        .map(|member| member.member_id)
}

/// Build one attachment's projection of `view`. Members whose tab is not live,
/// or no longer in the arrangement's workspace, are omitted (absent and
/// dangling references are retained in the view, never shown).
fn project(
    view: &PersonalView,
    binding_revision: u64,
    live: &BTreeMap<Uuid, &ContextSummary>,
    active_workspace: Uuid,
    selected: Option<Uuid>,
    last_removed: Option<&Removal>,
) -> PersonalViewProjection {
    PersonalViewProjection {
        last_removed: last_removed
            .filter(|removal| removal.view_id == view.view_id)
            .map(|removal| PersonalViewRemoval {
                context_id: removal.context_id,
                name: removal.name.clone(),
                removed_at_ms: removal.removed_at_ms,
            }),
        view_id: view.view_id,
        name: view.name.clone(),
        view_revision: view.revision,
        binding_revision,
        active_workspace_id: active_workspace,
        workspaces: view
            .workspaces
            .iter()
            .filter(|arrangement| arrangement.initialized)
            .map(|arrangement| PersonalViewWorkspace {
                workspace_id: arrangement.workspace_id,
                members: arrangement
                    .members
                    .iter()
                    .filter_map(|member| {
                        let context = live.get(&member.context_id)?;
                        (workspace_of(context) == arrangement.workspace_id)
                            .then_some((member, *context))
                    })
                    .enumerate()
                    .map(|(index, (member, context))| PersonalViewMember {
                        member_id: member.member_id,
                        context_id: member.context_id,
                        name: member
                            .label
                            .clone()
                            .or_else(|| context.name.clone())
                            .unwrap_or_else(|| format!("tab-{}", index + 1)),
                        active: selected == Some(member.context_id),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// The encoded projection, refused (never truncated) above the host limit.
fn encode(projection: &PersonalViewProjection) -> std::result::Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(projection).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_PROJECTION_BYTES {
        return Err(format!(
            "personal view projection is {} bytes, above the {MAX_PROJECTION_BYTES}-byte delivery limit",
            bytes.len()
        ));
    }
    Ok(bytes)
}

impl Attachments {
    /// Ask the refresh worker to run soon. Bursts coalesce.
    pub fn schedule(&self) {
        self.wake.notify_one();
    }

    /// Resolves after [`Self::schedule`].
    pub async fn woken(&self) {
        self.wake.notified().await;
    }

    /// The view a tab created by `client` joins, with the binding's owner.
    pub fn bound_view(&self, client: Option<Uuid>) -> Option<(Owner, Uuid)> {
        let client = client?;
        self.bindings
            .lock()
            .ok()?
            .get(&client)
            .map(|binding| (binding.owner, binding.view_id))
    }

    fn binding(&self, client: Uuid) -> Result<Option<Binding>> {
        Ok(self
            .bindings
            .lock()
            .map_err(|_| unavailable("attachment bindings poisoned"))?
            .get(&client)
            .cloned())
    }

    /// A fresh binding for `owner`: its default view, created when absent.
    fn new_binding<C: HostRuntimeApi + Sync>(env: &Env<'_, C>, owner: Owner) -> Result<Binding> {
        let ack = Repository::new(env.caller, env.states)
            .apply(
                owner,
                Uuid::new_v4(),
                &Command::EnsureDefault,
                &LiveCatalog::new(env.caller),
                env.now_ms,
            )
            .map_err(view_error)?;
        Ok(Binding {
            owner,
            view_id: ack.view_id,
            revision: 1,
            selected_member: None,
            held_workspace: None,
            last_removed: None,
            published: None,
        })
    }

    /// The client's binding, created for `owner` when it has none.
    fn bind<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Uuid,
        owner: Owner,
    ) -> Result<Binding> {
        if let Some(binding) = self.binding(client)? {
            return Ok(binding);
        }
        let fresh = Self::new_binding(env, owner)?;
        let binding = self
            .bindings
            .lock()
            .map_err(|_| unavailable("attachment bindings poisoned"))?
            .entry(client)
            .or_insert(fresh)
            .clone();
        self.schedule();
        Ok(binding)
    }

    /// The invoking attachment's binding, after resolving its owner from the
    /// invocation and refreshing its selected member.
    fn invoking_binding<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
    ) -> Result<(Uuid, Binding)> {
        let client = client
            .filter(|id| !id.is_nil())
            .ok_or(PersonalAttachmentError::NoAttachment)?;
        let owner = match resolve_caller(env.caller) {
            OwnerResolution::Owner(owner) => owner,
            OwnerResolution::NoPrincipal => return Err(PersonalAttachmentError::NoPrincipal),
            OwnerResolution::Unavailable(reason) => return Err(unavailable(reason)),
        };
        if self.binding(client)?.is_none() && !env.host.is_attached(client) {
            return Err(PersonalAttachmentError::NoAttachment);
        }
        let binding = self.bind(env, client, owner)?;
        // The principal is fixed for a connection; a mismatch means the
        // binding is not this caller's.
        if binding.owner != owner {
            return Err(PersonalAttachmentError::NoPrincipal);
        }
        let contexts = crate::list_contexts(env.caller).map_err(unavailable)?;
        let repository = Repository::new(env.caller, env.states);
        let view = match repository.get_view(owner, binding.view_id) {
            Ok(view) => view,
            // The bound view was deleted: the attachment falls back to the
            // default view.
            Err(PersonalViewError::NotFound) => {
                let view_id = Self::new_binding(env, owner)?.view_id;
                repository.get_view(owner, view_id).map_err(view_error)?
            }
            Err(error) => return Err(view_error(error)),
        };
        let selected = env.host.selected_context(client);
        let workspace = selected
            .and_then(|id| contexts.iter().find(|context| context.id == id))
            .map_or_else(Uuid::nil, workspace_of);
        let member = selected_member_of(&view, workspace, selected);
        let binding = self.commit(client, &binding, |current| {
            current.update(view.view_id, member);
        })?;
        Ok((client, binding))
    }

    /// Apply `change` to the client's current binding when it is still the
    /// one `seen` describes (same view); returns the binding after.
    fn commit(
        &self,
        client: Uuid,
        seen: &Binding,
        change: impl FnOnce(&mut Binding),
    ) -> Result<Binding> {
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| unavailable("attachment bindings poisoned"))?;
        let current = bindings
            .get_mut(&client)
            .ok_or(PersonalAttachmentError::NoAttachment)?;
        if current.owner != seen.owner {
            return Err(PersonalAttachmentError::NoPrincipal);
        }
        let before = current.revision;
        change(current);
        let after = current.clone();
        drop(bindings);
        if after.revision != before {
            self.schedule();
        }
        Ok(after)
    }

    /// `current-binding` for the invoking client.
    ///
    /// # Errors
    /// `no-attachment`, `no-principal`, or the repository failed.
    pub fn current_binding<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
    ) -> Result<PersonalBinding> {
        self.invoking_binding(env, client)
            .map(|(_, binding)| binding.to_wire())
    }

    /// `bind-view` for the invoking client: only a view of its own owner.
    ///
    /// # Errors
    /// `revision-conflict`, `not-found` for another owner's or an absent view.
    pub fn bind_view<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
        view_id: Uuid,
        expected_revision: u64,
    ) -> Result<PersonalBinding> {
        let (client, binding) = self.invoking_binding(env, client)?;
        check_revision(&binding, expected_revision)?;
        Repository::new(env.caller, env.states)
            .get_view(binding.owner, view_id)
            .map_err(view_error)?;
        let mut conflict = None;
        let binding = self.commit(client, &binding, |current| {
            if current.revision == expected_revision {
                // The refresh derives the selected member in the new view.
                current.update(view_id, None);
            } else {
                conflict = Some(current.revision);
            }
        })?;
        conflict.map_or_else(
            || Ok(binding.to_wire()),
            |current_revision| Err(PersonalAttachmentError::RevisionConflict { current_revision }),
        )
    }

    /// `select-member` for the invoking client: the member must be in the
    /// bound view's arrangement of the attachment's active workspace and
    /// reference a live tab there. `select` performs the selection through
    /// the attachment's ordinary tab selection path.
    ///
    /// # Errors
    /// `revision-conflict`, `invalid-reference`, or `unavailable` when the
    /// selection failed.
    pub fn select_member<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
        member_id: Uuid,
        expected_revision: u64,
        select: impl FnOnce(Uuid) -> std::result::Result<(), String>,
    ) -> Result<PersonalBinding> {
        let (client, binding) = self.invoking_binding(env, client)?;
        check_revision(&binding, expected_revision)?;
        let view = Repository::new(env.caller, env.states)
            .get_view(binding.owner, binding.view_id)
            .map_err(view_error)?;
        let (arrangement, member) = view
            .workspaces
            .iter()
            .find_map(|arrangement| {
                arrangement
                    .members
                    .iter()
                    .find(|member| member.member_id == member_id)
                    .map(|member| (arrangement.workspace_id, member.context_id))
            })
            .ok_or(PersonalAttachmentError::InvalidReference)?;
        let contexts = crate::list_contexts(env.caller).map_err(unavailable)?;
        let live_workspace = contexts
            .iter()
            .find(|context| context.id == member)
            .map(workspace_of)
            .ok_or(PersonalAttachmentError::InvalidReference)?;
        let active_workspace = env
            .host
            .selected_context(client)
            .and_then(|id| contexts.iter().find(|context| context.id == id))
            .map_or_else(Uuid::nil, workspace_of);
        if live_workspace != arrangement || arrangement != active_workspace {
            return Err(PersonalAttachmentError::InvalidReference);
        }
        select(member).map_err(unavailable)?;
        let binding = self.commit(client, &binding, |current| {
            if current.view_id == binding.view_id {
                current.update(current.view_id, Some(member_id));
            }
        })?;
        Ok(binding.to_wire())
    }

    /// Reconcile bindings with the attached clients and publish every
    /// changed projection to its own client. Returns what failed, per client.
    pub fn refresh<C: HostRuntimeApi + Sync>(&self, env: &Env<'_, C>) -> Vec<String> {
        let Ok(_refresh) = self.refresh.lock() else {
            return vec!["attachment refresh lock poisoned".into()];
        };
        let mut failures = Vec::new();
        let attached = env
            .host
            .attached_clients()
            .map(|clients| clients.into_iter().collect::<BTreeSet<_>>());
        self.retire(env, attached.as_ref());
        for client in attached.iter().flatten() {
            if self.binding(*client).ok().flatten().is_some()
                || env
                    .sink
                    .is_some_and(|sink| sink.is_connected(*client) == Some(false))
            {
                continue;
            }
            // A client without a principal has no views and no projection.
            let Some(owner) = env
                .host
                .principal(*client)
                .and_then(Owner::from_resolved_principal)
            else {
                continue;
            };
            if let Err(error) = self.bind(env, *client, owner) {
                failures.push(format!("binding {client}: {error:?}"));
            }
        }
        let clients: Vec<Uuid> = match self.bindings.lock() {
            Ok(bindings) => bindings.keys().copied().collect(),
            Err(_) => return vec!["attachment bindings poisoned".into()],
        };
        if clients.is_empty() {
            return failures;
        }
        let contexts = match crate::list_contexts(env.caller) {
            Ok(contexts) => contexts,
            Err(error) => {
                failures.push(format!("listing tabs: {error}"));
                return failures;
            }
        };
        for client in clients {
            if let Err(error) = self.publish(env, client, &contexts) {
                failures.push(format!("projection for {client}: {error}"));
            }
        }
        failures
    }

    /// Drop bindings whose client detached or disconnected, clearing what was
    /// retained for them.
    fn retire<C: ?Sized>(&self, env: &Env<'_, C>, attached: Option<&BTreeSet<Uuid>>) {
        let retired: Vec<Uuid> = {
            let Ok(mut bindings) = self.bindings.lock() else {
                return;
            };
            let gone: Vec<Uuid> = bindings
                .keys()
                .copied()
                .filter(|client| {
                    attached.is_some_and(|attached| !attached.contains(client))
                        || env
                            .sink
                            .is_some_and(|sink| sink.is_connected(*client) == Some(false))
                })
                .collect();
            for client in &gone {
                bindings.remove(client);
            }
            gone
        };
        for client in retired {
            // A disconnected client's retention is already gone; still clear
            // a detached one that stays connected.
            if let Some(sink) = env.sink
                && sink.is_connected(client) != Some(false)
                && let Err(error) = sink.clear_retained_to(client, STATE_KIND.as_str())
            {
                tracing::debug!(%client, %error, "clearing a retired personal view projection failed");
            }
        }
    }

    /// Recompute one client's projection and publish it when it changed.
    fn publish<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Uuid,
        contexts: &[ContextSummary],
    ) -> std::result::Result<(), String> {
        let Some(binding) = self.binding(client).map_err(|error| format!("{error:?}"))? else {
            return Ok(());
        };
        let repository = Repository::new(env.caller, env.states);
        let mut view_id = binding.view_id;
        let mut view = match repository.get_view(binding.owner, view_id) {
            Ok(view) => view,
            // The bound view was deleted: fall back to the default view.
            Err(PersonalViewError::NotFound) => {
                view_id = Self::new_binding(env, binding.owner)
                    .map_err(|error| format!("{error:?}"))?
                    .view_id;
                repository
                    .get_view(binding.owner, view_id)
                    .map_err(|error| format!("{error:?}"))?
            }
            Err(error) => return Err(format!("{error:?}")),
        };
        let live: BTreeMap<Uuid, &ContextSummary> = contexts
            .iter()
            .map(|context| (context.id, context))
            .collect();
        let selected = env
            .host
            .selected_context(client)
            .filter(|id| live.contains_key(id));
        // Without a selection the attachment keeps showing the workspace it
        // was emptied in.
        let active_workspace = selected.and_then(|id| live.get(&id)).map_or_else(
            || binding.held_workspace.unwrap_or_else(Uuid::nil),
            |context| workspace_of(context),
        );
        // Seed the attachment's active workspace once, from the canonical
        // order, on first access. Only a live selection names a workspace.
        if selected.is_some()
            && !view
                .workspaces
                .iter()
                .any(|arrangement| arrangement.workspace_id == active_workspace)
        {
            view = Self::initialize(env, binding.owner, view, active_workspace)?;
        }
        let member = selected_member_of(&view, active_workspace, selected);
        let mut updated = binding.clone();
        updated.update(view.view_id, member);
        let payload = encode(&project(
            &view,
            updated.revision,
            &live,
            active_workspace,
            selected,
            binding.last_removed.as_ref(),
        ))?;
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| "attachment bindings poisoned".to_string())?;
        let Some(current) = bindings.get_mut(&client) else {
            return Ok(());
        };
        if *current != binding {
            // An invocation changed the binding meanwhile; the next refresh
            // projects that.
            drop(bindings);
            self.schedule();
            return Ok(());
        }
        current.update(updated.view_id, updated.selected_member);
        if selected.is_some() {
            current.held_workspace = None;
        }
        if current.published.as_deref() == Some(payload.as_slice()) {
            return Ok(());
        }
        let sink = env.sink.ok_or("recipient-scoped delivery is unavailable")?;
        // Publishing only queues the value for this one recipient.
        sink.publish_retained_to(client, STATE_KIND.as_str().to_string(), payload.clone())
            .map_err(|error| error.to_string())?;
        current.published = Some(payload);
        drop(bindings);
        Ok(())
    }

    /// Seed `workspace` in `view`, retrying once on a concurrent edit.
    fn initialize<C: HostRuntimeApi + Sync>(
        env: &Env<'_, C>,
        owner: Owner,
        mut view: PersonalView,
        workspace: Uuid,
    ) -> std::result::Result<PersonalView, String> {
        let repository = Repository::new(env.caller, env.states);
        let catalog = LiveCatalog::new(env.caller);
        for _ in 0..2 {
            match repository.apply(
                owner,
                Uuid::new_v4(),
                &Command::InitializeWorkspace {
                    view_id: view.view_id,
                    expected_revision: view.revision,
                    workspace_id: workspace,
                },
                &catalog as &dyn Catalog,
                env.now_ms,
            ) {
                Ok(_) | Err(PersonalViewError::RevisionConflict { .. }) => {
                    view = repository
                        .get_view(owner, view.view_id)
                        .map_err(|error| format!("{error:?}"))?;
                    if view
                        .workspaces
                        .iter()
                        .any(|arrangement| arrangement.workspace_id == workspace)
                    {
                        return Ok(view);
                    }
                }
                Err(error) => return Err(format!("{error:?}")),
            }
        }
        Err("seeding the active workspace kept conflicting".into())
    }
}

/// An edit of the invoking attachment's bound view, by tab
/// (`tabs-personal-actions/v1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewEdit {
    /// Add the tab when absent.
    Add,
    /// Remove the tab when present.
    Remove,
    /// Move the member tab before the member tab `before`, or to the end.
    Move { before: Option<Uuid> },
    /// Add the tab when absent, before the member tab `before` when it is
    /// still one, else at the end (undoing a removal).
    Restore { before: Option<Uuid> },
}

/// Personal navigation (`next-tab`, `goto-tab`, ...) and tab-addressed view
/// edits.
impl Attachments {
    /// The order the caller's navigation walks in `workspace`: the context
    /// ids of its view's arrangement of that workspace, in view order
    /// (dangling and moved members included; the caller filters to live
    /// tabs of the workspace).
    ///
    /// An attached caller walks its bound view, seeding the workspace on
    /// first access. A caller with a principal but no attachment (a CLI
    /// command) walks its default view when that view already arranges the
    /// workspace. `None` means personal order does not apply (no client, no
    /// principal, no default view yet or an unarranged workspace for an
    /// unattached caller), and the caller falls back to the shared order.
    ///
    /// # Errors
    /// The view repository failed for a caller that has a view.
    pub fn navigation_order<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
        workspace: Uuid,
    ) -> Result<Option<Vec<Uuid>>> {
        let Some(client) = client.filter(|id| !id.is_nil()) else {
            return Ok(None);
        };
        if self.binding(client)?.is_none() && !env.host.is_attached(client) {
            let OwnerResolution::Owner(owner) = resolve_caller(env.caller) else {
                return Ok(None);
            };
            return match Repository::new(env.caller, env.states).default_view(owner) {
                Ok(view) => Ok(arrangement_order(&view, workspace)),
                Err(
                    PersonalViewError::NotInitialized
                    | PersonalViewError::NotFound
                    | PersonalViewError::NoPrincipal,
                ) => Ok(None),
                Err(error) => Err(view_error(error)),
            };
        }
        let (_, binding) = match self.invoking_binding(env, Some(client)) {
            Ok(found) => found,
            Err(PersonalAttachmentError::NoAttachment | PersonalAttachmentError::NoPrincipal) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let view = self.arranged_view(env, &binding, workspace)?;
        Ok(arrangement_order(&view, workspace))
    }

    /// The binding's view, with `workspace` seeded when it has no
    /// arrangement yet.
    fn arranged_view<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        binding: &Binding,
        workspace: Uuid,
    ) -> Result<PersonalView> {
        let view = Repository::new(env.caller, env.states)
            .get_view(binding.owner, binding.view_id)
            .map_err(view_error)?;
        if view
            .workspaces
            .iter()
            .any(|arrangement| arrangement.workspace_id == workspace)
        {
            return Ok(view);
        }
        let view = Self::initialize(env, binding.owner, view, workspace).map_err(unavailable)?;
        self.schedule();
        Ok(view)
    }

    /// Apply `edit` for `context` to the invoking attachment's bound view,
    /// resolving members and the current revision, and retrying when another
    /// attachment edited the view meanwhile.
    ///
    /// A removal is remembered as the invoking attachment's last removal
    /// (for undo), and every attachment bound to the view whose selected tab
    /// it was moves to the tab's neighbour in the arrangement (the next live
    /// member, else the previous one): `select` selects it for the invoking
    /// attachment, the selection authority for the others. When no live
    /// member remains, those attachments are left with no selection (no
    /// input target) while still showing that workspace.
    ///
    /// # Errors
    /// `no-attachment`, `no-principal`, `invalid-reference` (the tab is not
    /// live, or a moved tab or `before` is not a member of the tab's
    /// arrangement), or `unavailable`.
    pub fn edit_view<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
        context: Uuid,
        edit: ViewEdit,
        select: impl FnOnce(Uuid) -> std::result::Result<(), String>,
    ) -> Result<PersonalAck> {
        let (client, binding) = self.invoking_binding(env, client)?;
        let contexts = crate::list_contexts(env.caller).map_err(unavailable)?;
        let summary = contexts
            .iter()
            .find(|candidate| candidate.id == context)
            .ok_or(PersonalAttachmentError::InvalidReference)?;
        let workspace = workspace_of(summary);
        let live: BTreeSet<Uuid> = contexts
            .iter()
            .filter(|candidate| workspace_of(candidate) == workspace)
            .map(|candidate| candidate.id)
            .collect();
        let repository = Repository::new(env.caller, env.states);
        let catalog = LiveCatalog::new(env.caller);
        for _ in 0..4 {
            let view = self.arranged_view(env, &binding, workspace)?;
            let order = arrangement_order(&view, workspace).unwrap_or_default();
            let arranged = |tab: Uuid| {
                view.workspaces
                    .iter()
                    .find(|arrangement| arrangement.workspace_id == workspace)
                    .and_then(|arrangement| {
                        arrangement
                            .members
                            .iter()
                            .find(|member| member.context_id == tab)
                    })
            };
            let member = |tab: Uuid| arranged(tab).map(|member| member.member_id);
            let unchanged = PersonalAck {
                view_id: view.view_id,
                revision: view.revision,
                member_id: member(context),
            };
            let Some(command) = edit_command(edit, context, &view, &unchanged, member)? else {
                return Ok(unchanged);
            };
            let removal = (edit == ViewEdit::Remove).then(|| Removal {
                context_id: context,
                view_id: view.view_id,
                name: arranged(context)
                    .and_then(|member| member.label.clone())
                    .or_else(|| summary.name.clone())
                    .unwrap_or_else(|| "tab".into()),
                before: order
                    .iter()
                    .skip_while(|tab| **tab != context)
                    .skip(1)
                    .copied()
                    .find(|tab| live.contains(tab)),
                was_selected: env.host.selected_context(client) == Some(context),
                removed_at_ms: env.now_ms,
            });
            match repository.apply(
                binding.owner,
                Uuid::new_v4(),
                &command,
                &catalog as &dyn Catalog,
                env.now_ms,
            ) {
                Ok(ack) => {
                    if let Some(removal) = removal {
                        self.with_binding(client, |current| {
                            current.last_removed = Some(removal);
                        });
                        let removed = Displacement {
                            owner: binding.owner,
                            view_id: view.view_id,
                            workspace,
                            removed: context,
                            neighbour: removal_neighbour(&order, &live, context),
                        };
                        self.reselect_displaced(env, client, &removed, select)?;
                    }
                    self.schedule();
                    return Ok(ack);
                }
                Err(PersonalViewError::RevisionConflict { .. }) => {}
                Err(error) => return Err(view_error(error)),
            }
        }
        Err(unavailable("the view kept changing; try again"))
    }

    /// Apply `change` to `client`'s binding, when it has one.
    fn with_binding(&self, client: Uuid, change: impl FnOnce(&mut Binding)) {
        if let Ok(mut bindings) = self.bindings.lock()
            && let Some(binding) = bindings.get_mut(&client)
        {
            change(binding);
        }
    }

    /// Move every attachment bound to the displacement's view whose selected
    /// tab was removed: to the neighbour, or to no selection. The invoking
    /// attachment selects through `select`; a failure there is returned,
    /// failures for other attachments are logged.
    fn reselect_displaced<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        invoker: Uuid,
        displacement: &Displacement,
        select: impl FnOnce(Uuid) -> std::result::Result<(), String>,
    ) -> Result<()> {
        let displaced: Vec<Uuid> = self
            .bindings
            .lock()
            .map_err(|_| unavailable("attachment bindings poisoned"))?
            .iter()
            .filter(|(_, binding)| {
                binding.owner == displacement.owner && binding.view_id == displacement.view_id
            })
            .map(|(client, _)| *client)
            .filter(|client| env.host.selected_context(*client) == Some(displacement.removed))
            .collect();
        let mut select = Some(select);
        let mut outcome = Ok(());
        for client in displaced {
            let own = if client == invoker {
                select.take()
            } else {
                None
            };
            let result = self.move_displaced(env, client, displacement, own);
            match result {
                Err(reason) if client == invoker => outcome = Err(unavailable(reason)),
                Err(reason) => {
                    tracing::warn!(%client, %reason, "moving an attachment off a removed tab failed");
                }
                Ok(()) => {}
            }
        }
        outcome
    }

    /// Move one displaced attachment to the neighbour (through `own` when
    /// given), or leave it with no selection while it keeps showing the
    /// workspace.
    fn move_displaced<C: ?Sized>(
        &self,
        env: &Env<'_, C>,
        client: Uuid,
        displacement: &Displacement,
        own: Option<impl FnOnce(Uuid) -> std::result::Result<(), String>>,
    ) -> std::result::Result<(), String> {
        if let Some(neighbour) = displacement.neighbour {
            return own.map_or_else(
                || env.selection.select_for(client, neighbour),
                |select| select(neighbour),
            );
        }
        self.with_binding(client, |binding| {
            binding.held_workspace = Some(displacement.workspace);
        });
        env.selection.clear_for(client)
    }

    /// Before a deleted tab is pruned from the views: move every attachment
    /// that showed it to its neighbour in its own view, or leave it with no
    /// selection when that arrangement has no other live tab. An attachment
    /// already on another tab of its view stays there. Returns what failed.
    pub fn reconcile_deleted<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        deleted: Uuid,
    ) -> Vec<String> {
        let bindings: Vec<(Uuid, Binding)> = match self.bindings.lock() {
            Ok(bindings) => bindings
                .iter()
                .map(|(client, binding)| (*client, binding.clone()))
                .collect(),
            Err(_) => return vec!["attachment bindings poisoned".into()],
        };
        if bindings.is_empty() {
            return Vec::new();
        }
        let contexts = match crate::list_contexts(env.caller) {
            Ok(contexts) => contexts,
            Err(error) => return vec![format!("listing tabs: {error}")],
        };
        let repository = Repository::new(env.caller, env.states);
        let mut failures = Vec::new();
        for (client, binding) in bindings {
            let Ok(view) = repository.get_view(binding.owner, binding.view_id) else {
                continue;
            };
            let Some((workspace, member)) = view.workspaces.iter().find_map(|arrangement| {
                arrangement
                    .members
                    .iter()
                    .find(|member| member.context_id == deleted)
                    .map(|member| (arrangement.workspace_id, member.member_id))
            }) else {
                continue;
            };
            if binding.selected_member != Some(member) {
                continue;
            }
            let current = env.host.selected_context(client);
            let arranged_live = |tab: Uuid| {
                contexts.iter().any(|context| context.id == tab)
                    && view.workspaces.iter().any(|arrangement| {
                        arrangement
                            .members
                            .iter()
                            .any(|member| member.context_id == tab)
                    })
            };
            if current.is_some_and(|tab| tab != deleted && arranged_live(tab)) {
                continue;
            }
            let order = arrangement_order(&view, workspace).unwrap_or_default();
            let live: BTreeSet<Uuid> = contexts
                .iter()
                .filter(|context| workspace_of(context) == workspace)
                .map(|context| context.id)
                .collect();
            let displacement = Displacement {
                owner: binding.owner,
                view_id: binding.view_id,
                workspace,
                removed: deleted,
                neighbour: removal_neighbour(&order, &live, deleted),
            };
            if let Err(reason) = self.move_displaced(
                env,
                client,
                &displacement,
                None::<fn(Uuid) -> std::result::Result<(), String>>,
            ) {
                failures.push(format!(
                    "moving {client} off deleted tab {deleted}: {reason}"
                ));
            }
        }
        self.schedule();
        failures
    }

    /// Undo the invoking attachment's last removal: add the tab back where it
    /// was (before the tab that followed it, when that is still a member),
    /// after checking it still exists. Returns the removal undone; the caller
    /// selects the tab again when it was selected.
    ///
    /// # Errors
    /// `invalid-reference` when the tab no longer exists (the removal is
    /// forgotten), `unavailable` when there is nothing to undo in the bound
    /// view, or the edit failed.
    pub fn undo_removal<C: HostRuntimeApi + Sync>(
        &self,
        env: &Env<'_, C>,
        client: Option<Uuid>,
    ) -> Result<(Removal, PersonalAck)> {
        let (client, binding) = self.invoking_binding(env, client)?;
        let removal = binding
            .last_removed
            .filter(|removal| removal.view_id == binding.view_id)
            .ok_or_else(|| unavailable("nothing to undo in this view"))?;
        let forget = |this: &Self| {
            this.with_binding(client, |current| {
                if current.last_removed.as_ref() == Some(&removal) {
                    current.last_removed = None;
                }
            });
            this.schedule();
        };
        let exists = crate::list_contexts(env.caller)
            .map_err(unavailable)?
            .iter()
            .any(|context| context.id == removal.context_id);
        if !exists {
            forget(self);
            return Err(PersonalAttachmentError::InvalidReference);
        }
        let ack = self.edit_view(
            env,
            Some(client),
            removal.context_id,
            ViewEdit::Restore {
                before: removal.before,
            },
            |_| Ok(()),
        )?;
        forget(self);
        Ok((removal, ack))
    }
}

/// A tab leaving one view's arrangement, and where attachments showing it go.
struct Displacement {
    owner: Owner,
    view_id: Uuid,
    workspace: Uuid,
    removed: Uuid,
    neighbour: Option<Uuid>,
}

/// The repository command for `edit` of `context` in `view`, or `None` when
/// the view already has the outcome.
fn edit_command(
    edit: ViewEdit,
    context: Uuid,
    view: &PersonalView,
    unchanged: &PersonalAck,
    member: impl Fn(Uuid) -> Option<Uuid>,
) -> Result<Option<Command>> {
    let command = match edit {
        ViewEdit::Add | ViewEdit::Restore { .. } => {
            if unchanged.member_id.is_some() {
                return Ok(None);
            }
            let before = match edit {
                ViewEdit::Restore { before } => before.and_then(member),
                _ => None,
            };
            Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: context,
                before,
            }
        }
        ViewEdit::Remove => {
            let Some(member_id) = unchanged.member_id else {
                return Ok(None);
            };
            Command::RemoveTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id,
            }
        }
        ViewEdit::Move { before } => {
            let member_id = unchanged
                .member_id
                .ok_or(PersonalAttachmentError::InvalidReference)?;
            let before = match before {
                Some(tab) if tab == context => return Ok(None),
                Some(tab) => Some(member(tab).ok_or(PersonalAttachmentError::InvalidReference)?),
                None => None,
            };
            Command::MoveTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id,
                before,
            }
        }
    };
    Ok(Some(command))
}

/// The context ids of `view`'s arrangement of `workspace`, in view order, or
/// `None` when the view has not arranged that workspace.
fn arrangement_order(view: &PersonalView, workspace: Uuid) -> Option<Vec<Uuid>> {
    view.workspaces
        .iter()
        .find(|arrangement| arrangement.workspace_id == workspace && arrangement.initialized)
        .map(|arrangement| {
            arrangement
                .members
                .iter()
                .map(|member| member.context_id)
                .collect()
        })
}

/// The live member after `removed` in `order`, else the live one before it.
fn removal_neighbour(order: &[Uuid], live: &BTreeSet<Uuid>, removed: Uuid) -> Option<Uuid> {
    let position = order.iter().position(|tab| *tab == removed)?;
    order[position + 1..]
        .iter()
        .chain(order[..position].iter().rev())
        .copied()
        .find(|tab| live.contains(tab))
}

const fn check_revision(binding: &Binding, expected: u64) -> Result<()> {
    if binding.revision == expected {
        Ok(())
    } else {
        Err(PersonalAttachmentError::RevisionConflict {
            current_revision: binding.revision,
        })
    }
}

#[cfg(test)]
mod tests;
