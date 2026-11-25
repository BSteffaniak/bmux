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
    PersonalViewMember, PersonalViewProjection, PersonalViewWorkspace, STATE_KIND,
};
use bmux_tabs_plugin_api::tabs_personal_attachment::{PersonalAttachmentError, PersonalBinding};
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
    pub sink: Option<&'a dyn WireEventSink>,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    owner: Owner,
    view_id: Uuid,
    revision: u64,
    selected_member: Option<Uuid>,
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
) -> PersonalViewProjection {
    PersonalViewProjection {
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
        let active_workspace = selected
            .and_then(|id| live.get(&id))
            .map_or_else(Uuid::nil, |context| workspace_of(context));
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
