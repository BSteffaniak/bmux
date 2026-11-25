use super::*;
use bmux_contexts_plugin_api::contexts_commands;
use bmux_contexts_plugin_api::contexts_state::ContextSummary;
use bmux_plugin_sdk::{ServiceKind, decode_service_message, encode_service_message};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Storage = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

/// A host for one invoking client: shared in-memory plugin storage, the
/// client's host-bound principal, live contexts, and storage fault injection.
#[derive(Clone)]
pub(super) struct MemoryHost {
    storage: Storage,
    /// `None`: the identity authority says this caller has no principal.
    principal: Option<Uuid>,
    identity_unreachable: bool,
    pub(super) contexts: Arc<Mutex<Vec<ContextSummary>>>,
    /// Storage `set` fails for keys starting with this prefix.
    fail_set_prefix: Arc<Mutex<Option<String>>>,
    sets: Arc<Mutex<Vec<String>>>,
}

impl MemoryHost {
    pub(super) fn new() -> Self {
        Self {
            storage: Arc::default(),
            principal: None,
            identity_unreachable: false,
            contexts: Arc::default(),
            fail_set_prefix: Arc::default(),
            sets: Arc::default(),
        }
    }

    /// The same installation, invoked by a client bound to `principal`.
    pub(super) fn client(&self, principal: Option<Uuid>) -> Self {
        Self {
            principal,
            ..self.clone()
        }
    }

    fn snapshot_storage(&self) -> BTreeMap<String, Vec<u8>> {
        self.storage.lock().unwrap().clone()
    }

    fn put(&self, key: &str, value: Vec<u8>) {
        self.storage.lock().unwrap().insert(key.into(), value);
    }

    fn fail_sets(&self, prefix: Option<&str>) {
        *self.fail_set_prefix.lock().unwrap() = prefix.map(Into::into);
    }

    fn add_context(&self, id: Uuid, workspace: Option<Uuid>) {
        let mut attributes = BTreeMap::new();
        if let Some(workspace) = workspace {
            attributes.insert("workspace".into(), workspace.to_string());
        }
        self.contexts.lock().unwrap().push(ContextSummary {
            id,
            name: None,
            attributes,
        });
    }
}

impl ServiceCaller for MemoryHost {
    fn call_service_raw(
        &self,
        _capability: &str,
        _kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let checked = bmux_plugin_sdk::service_contract::is_checked(&payload);
        let payload = if checked {
            let contract = <bmux_clients_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id)
                .or_else(|| <bmux_contexts_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id))
                .or_else(|| <bmux_workspaces_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface_id))
                .expect("generated contract");
            bmux_plugin_sdk::service_contract::decode_for(interface_id, contract, &payload)?
                .to_vec()
        } else {
            payload
        };
        let response = match (interface_id, operation) {
            ("clients-identity/v1", "current-principal") => {
                use bmux_clients_plugin_api::clients_identity::{IdentityError, LocalPrincipal};
                if self.identity_unreachable {
                    return Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "clients plugin missing",
                    });
                }
                let result: std::result::Result<LocalPrincipal, IdentityError> = self
                    .principal
                    .map(|principal_id| LocalPrincipal { principal_id })
                    .ok_or(IdentityError::NoCurrentClient);
                encode_service_message(&result)?
            }
            ("contexts-state", "list-contexts") => {
                encode_service_message(&self.contexts.lock().unwrap().clone())?
            }
            ("contexts-commands", "create-context") => {
                let request: contexts_commands::client::CreateContextRequest =
                    decode_service_message(&payload)?;
                let id = Uuid::new_v4();
                self.contexts.lock().unwrap().push(ContextSummary {
                    id,
                    name: request.name,
                    attributes: request.attributes,
                });
                let ok: std::result::Result<
                    contexts_commands::ContextAck,
                    contexts_commands::CreateContextError,
                > = Ok(contexts_commands::ContextAck {
                    id,
                    session_id: None,
                });
                encode_service_message(&ok)?
            }
            ("storage-query/v1", "get") => {
                let request: StorageGetRequest = decode_service_message(&payload)?;
                let value = self
                    .storage
                    .lock()
                    .unwrap()
                    .get(request.key.as_str())
                    .cloned();
                encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })?
            }
            ("storage-command/v1", "set") => {
                let request: StorageSetRequest = decode_service_message(&payload)?;
                if self
                    .fail_set_prefix
                    .lock()
                    .unwrap()
                    .as_deref()
                    .is_some_and(|prefix| request.key.as_str().starts_with(prefix))
                {
                    return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "injected storage failure".into(),
                    });
                }
                self.sets.lock().unwrap().push(request.key.to_string());
                self.storage
                    .lock()
                    .unwrap()
                    .insert(request.key.to_string(), request.value);
                encode_service_message(&())?
            }
            _ => {
                return Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "memory_host",
                });
            }
        };
        Ok(if checked {
            bmux_plugin_sdk::service_contract::encode_response(&response)
        } else {
            response
        })
    }

    fn execute_kernel_request(
        &self,
        _request: bmux_ipc::Request,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "memory_host_kernel",
        })
    }
}

/// A fixed shared catalog: live tab -> workspace, and canonical orders.
#[derive(Default)]
struct FakeCatalog {
    live: BTreeMap<Uuid, Uuid>,
    orders: BTreeMap<Uuid, Vec<Uuid>>,
}

impl FakeCatalog {
    fn with_workspace(workspace: Uuid, tabs: &[Uuid]) -> Self {
        let mut catalog = Self::default();
        catalog.set_workspace(workspace, tabs);
        catalog
    }

    fn set_workspace(&mut self, workspace: Uuid, tabs: &[Uuid]) {
        for tab in tabs {
            self.live.insert(*tab, workspace);
        }
        self.orders.insert(workspace, tabs.to_vec());
    }
}

impl Catalog for FakeCatalog {
    fn live_workspace_of(&self, context_id: Uuid) -> Result<Option<Uuid>> {
        Ok(self.live.get(&context_id).copied())
    }

    fn canonical_order(&self, workspace_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(self.orders.get(&workspace_id).cloned().unwrap_or_default())
    }
}

/// Stored bytes and the error class they must produce.
type Case = (&'static [u8], fn(&PersonalViewError) -> bool);

const NOW: u64 = 1_000_000_000;
const WS: Uuid = Uuid::from_u128(0x5_0000);

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn owner(n: u128) -> Owner {
    Owner::from_resolved_principal(id(0xA000 + n)).unwrap()
}

fn tabs(range: std::ops::Range<u128>) -> Vec<Uuid> {
    range.map(|n| id(0x7000 + n)).collect()
}

struct Fixture {
    host: MemoryHost,
    catalog: FakeCatalog,
    next_mutation: std::cell::Cell<u128>,
}

impl Fixture {
    fn new(catalog: FakeCatalog) -> Self {
        Self {
            host: MemoryHost::new(),
            catalog,
            next_mutation: std::cell::Cell::new(0x1_0000),
        }
    }

    fn repo(&self) -> Repository<'_, MemoryHost> {
        Repository::new(&self.host, None)
    }

    fn mutation(&self) -> Uuid {
        let next = self.next_mutation.get() + 1;
        self.next_mutation.set(next);
        id(next)
    }

    fn apply(&self, owner: Owner, command: &Command) -> Result<PersonalAck> {
        self.repo()
            .apply(owner, self.mutation(), command, &self.catalog, NOW)
    }

    fn ensure_default(&self, owner: Owner) -> PersonalAck {
        self.apply(owner, &Command::EnsureDefault).unwrap()
    }

    fn view(&self, owner: Owner, view_id: Uuid) -> PersonalView {
        self.repo().get_view(owner, view_id).unwrap()
    }

    fn members(&self, owner: Owner, view_id: Uuid, workspace: Uuid) -> Vec<Uuid> {
        self.view(owner, view_id)
            .workspaces
            .into_iter()
            .find(|arrangement| arrangement.workspace_id == workspace)
            .map(|arrangement| {
                arrangement
                    .members
                    .into_iter()
                    .map(|member| member.context_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn member_id(&self, owner: Owner, view_id: Uuid, context: Uuid) -> Uuid {
        self.view(owner, view_id)
            .workspaces
            .into_iter()
            .flat_map(|arrangement| arrangement.members)
            .find(|member| member.context_id == context)
            .unwrap()
            .member_id
    }

    fn initialize(&self, owner: Owner, view: &PersonalAck, workspace: Uuid) -> PersonalAck {
        self.apply(
            owner,
            &Command::InitializeWorkspace {
                view_id: view.view_id,
                expected_revision: view.revision,
                workspace_id: workspace,
            },
        )
        .unwrap()
    }
}

#[test]
fn owners_cannot_see_or_mutate_each_others_views() {
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &tabs(0..2)));
    let (a, b) = (owner(1), owner(2));
    let view = id(0xBEEF);
    let created = f
        .apply(
            a,
            &Command::CreateView {
                view_id: view,
                name: "work".into(),
            },
        )
        .unwrap();
    assert_eq!(created.revision, 1);
    let before = f
        .host
        .snapshot_storage()
        .get(&format!("tabs.personal.v1.{}", a.0))
        .cloned();

    assert_eq!(f.repo().list_views(b).unwrap().len(), 0);
    assert_eq!(f.repo().get_view(b, view), Err(PersonalViewError::NotFound));
    assert_eq!(
        f.repo().default_view(b),
        Err(PersonalViewError::NotInitialized)
    );
    for command in [
        Command::RenameView {
            view_id: view,
            expected_revision: 1,
            name: "mine".into(),
        },
        Command::DeleteView {
            view_id: view,
            expected_revision: 1,
        },
        Command::SetDefaultView { view_id: view },
        Command::InitializeWorkspace {
            view_id: view,
            expected_revision: 1,
            workspace_id: WS,
        },
    ] {
        assert_eq!(f.apply(b, &command), Err(PersonalViewError::NotFound));
    }
    // The same id is free in B's own namespace and reveals nothing about A.
    f.apply(
        b,
        &Command::CreateView {
            view_id: view,
            name: "b".into(),
        },
    )
    .unwrap();
    assert_eq!(
        f.host
            .snapshot_storage()
            .get(&format!("tabs.personal.v1.{}", a.0))
            .cloned(),
        before
    );
    let a_view = f.view(a, view);
    assert_eq!((a_view.owner, a_view.name.as_str()), (a.0, "work"));
    assert_eq!(f.view(b, view).owner, b.0);
}

#[test]
fn callers_without_a_principal_get_no_principal_and_write_nothing() {
    let host = MemoryHost::new();
    let states = PluginStateRegistry::new();
    let without = host.client(None);
    let nil = host.client(Some(Uuid::nil()));
    let mut unreachable = host.client(Some(id(1)));
    unreachable.identity_unreachable = true;
    for caller in [&without, &nil, &unreachable] {
        assert_eq!(
            list_views_for_caller(caller),
            Err(PersonalViewError::NoPrincipal)
        );
        assert_eq!(
            default_view_for_caller(caller),
            Err(PersonalViewError::NoPrincipal)
        );
        assert_eq!(
            get_view_for_caller(caller, id(2)),
            Err(PersonalViewError::NoPrincipal)
        );
        assert_eq!(
            apply_for_caller(caller, &states, id(3), &Command::EnsureDefault),
            Err(PersonalViewError::NoPrincipal)
        );
    }
    assert!(host.snapshot_storage().is_empty());
    assert!(Owner::from_resolved_principal(Uuid::nil()).is_none());

    // A principal resolves to its own owner.
    let with = host.client(Some(id(0xA1)));
    let ack = apply_for_caller(&with, &states, id(3), &Command::EnsureDefault).unwrap();
    assert_eq!(default_view_for_caller(&with).unwrap().owner, id(0xA1));
    assert_eq!(default_view_for_caller(&with).unwrap().view_id, ack.view_id);
}

#[test]
fn stale_revisions_conflict_without_mutating() {
    let f = Fixture::new(FakeCatalog::default());
    let a = owner(1);
    let view = f.ensure_default(a);
    let renamed = f
        .apply(
            a,
            &Command::RenameView {
                view_id: view.view_id,
                expected_revision: view.revision,
                name: "first".into(),
            },
        )
        .unwrap();
    assert_eq!(renamed.revision, 2);
    let stored = f.host.snapshot_storage();
    assert_eq!(
        f.apply(
            a,
            &Command::RenameView {
                view_id: view.view_id,
                expected_revision: 1,
                name: "second".into(),
            },
        ),
        Err(PersonalViewError::RevisionConflict {
            current_revision: 2
        })
    );
    assert_eq!(f.host.snapshot_storage(), stored);
    assert_eq!(f.view(a, view.view_id).name, "first");
}

#[test]
fn mutation_ids_replay_their_outcome_and_reject_different_payloads() {
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &tabs(0..3)));
    let a = owner(1);
    let view = f.ensure_default(a);
    let view = f.initialize(a, &view, WS);
    let first = tabs(0..3)[0];
    let member = f.member_id(a, view.view_id, first);
    let mutation = id(0x77);
    let remove = Command::RemoveTab {
        view_id: view.view_id,
        expected_revision: view.revision,
        member_id: member,
    };
    let acked = f
        .repo()
        .apply(a, mutation, &remove, &f.catalog, NOW)
        .unwrap();
    // Further edits move the revision on; the replay still returns the
    // recorded result instead of re-evaluating the stale expected revision.
    f.apply(
        a,
        &Command::RenameView {
            view_id: view.view_id,
            expected_revision: acked.revision,
            name: "later".into(),
        },
    )
    .unwrap();
    let stored = f.host.snapshot_storage();
    assert_eq!(
        f.repo().apply(a, mutation, &remove, &f.catalog, NOW + 5),
        Ok(acked)
    );
    assert_eq!(f.host.snapshot_storage(), stored);
    assert_eq!(
        f.repo().apply(
            a,
            mutation,
            &Command::RemoveTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id: f.member_id(a, view.view_id, tabs(0..3)[1]),
            },
            &f.catalog,
            NOW
        ),
        Err(PersonalViewError::MutationConflict)
    );
    assert_eq!(f.host.snapshot_storage(), stored);
    // Mutation ids are owner-scoped: another owner may use the same id.
    f.ensure_default(owner(2));
    assert!(
        f.repo()
            .apply(owner(2), mutation, &Command::EnsureDefault, &f.catalog, NOW)
            .is_ok()
    );
}

#[test]
fn view_name_label_and_view_count_bounds_reject_without_truncating() {
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &tabs(0..1)));
    let a = owner(1);
    let long = "é".repeat(MAX_TEXT_BYTES / 2 + 1);
    let fits = "x".repeat(MAX_TEXT_BYTES);
    assert_eq!(
        f.apply(
            a,
            &Command::CreateView {
                view_id: id(1),
                name: long.clone()
            }
        ),
        Err(limit("text-bytes"))
    );
    for n in 0..MAX_VIEWS as u128 {
        f.apply(
            a,
            &Command::CreateView {
                view_id: id(100 + n),
                name: fits.clone(),
            },
        )
        .unwrap();
    }
    let stored = f.host.snapshot_storage();
    assert_eq!(
        f.apply(
            a,
            &Command::CreateView {
                view_id: id(1),
                name: "one too many".into()
            }
        ),
        Err(limit("views"))
    );
    assert_eq!(f.apply(a, &Command::EnsureDefault), Err(limit("views")));
    assert_eq!(f.host.snapshot_storage(), stored);
    assert_eq!(f.repo().list_views(a).unwrap().len(), MAX_VIEWS);

    let view = PersonalAck {
        view_id: id(100),
        revision: 1,
        member_id: None,
    };
    let view = f.initialize(a, &view, WS);
    let member = f.member_id(a, view.view_id, tabs(0..1)[0]);
    assert_eq!(
        f.apply(
            a,
            &Command::SetLabel {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id: member,
                label: Some(long),
            }
        ),
        Err(limit("text-bytes"))
    );
    f.apply(
        a,
        &Command::SetLabel {
            view_id: view.view_id,
            expected_revision: view.revision,
            member_id: member,
            label: Some(fits.clone()),
        },
    )
    .unwrap();
    let label = f.view(a, view.view_id).workspaces[0].members[0]
        .label
        .clone();
    assert_eq!(label, Some(fits));
}

#[test]
fn workspace_and_member_bounds_reject_without_truncating() {
    let mut catalog = FakeCatalog::default();
    let many = tabs(0..(MAX_MEMBERS_PER_VIEW as u128 + 1));
    catalog.set_workspace(WS, &many);
    let small = id(0x5_0001);
    catalog.set_workspace(small, &tabs(5000..5002));
    let f = Fixture::new(catalog);
    let a = owner(1);
    let view = f.ensure_default(a);
    let stored = f.host.snapshot_storage();
    assert_eq!(
        f.apply(
            a,
            &Command::InitializeWorkspace {
                view_id: view.view_id,
                expected_revision: view.revision,
                workspace_id: WS,
            }
        ),
        Err(limit("members"))
    );
    assert_eq!(f.host.snapshot_storage(), stored);

    // Fill the view to exactly 1,024 members with one workspace short.
    let mut f = f;
    f.catalog
        .set_workspace(WS, &many[..MAX_MEMBERS_PER_VIEW - 1]);
    let view = f.initialize(a, &view, WS);
    let view = f.initialize(a, &view, id(0x9999)); // empty arrangement
    let extra = many[MAX_MEMBERS_PER_VIEW - 1];
    f.catalog.live.insert(extra, WS);
    let view = f
        .apply(
            a,
            &Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: extra,
                before: None,
            },
        )
        .unwrap();
    assert_eq!(
        f.view(a, view.view_id).workspaces[0].members.len(),
        MAX_MEMBERS_PER_VIEW
    );
    f.catalog.live.insert(many[MAX_MEMBERS_PER_VIEW], WS);
    assert_eq!(
        f.apply(
            a,
            &Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: many[MAX_MEMBERS_PER_VIEW],
                before: None,
            }
        ),
        Err(limit("members"))
    );
    assert_eq!(
        f.apply(
            a,
            &Command::InitializeWorkspace {
                view_id: view.view_id,
                expected_revision: view.revision,
                workspace_id: small,
            }
        ),
        Err(limit("members"))
    );

    // Workspace arrangements: 256 per view (empty ones count).
    let other = f
        .apply(
            a,
            &Command::CreateView {
                view_id: id(0xCAFE),
                name: "wide".into(),
            },
        )
        .unwrap();
    let mut other = other;
    for n in 0..MAX_WORKSPACES_PER_VIEW as u128 {
        other = f.initialize(a, &other, id(0x10_0000 + n));
    }
    assert_eq!(
        f.apply(
            a,
            &Command::InitializeWorkspace {
                view_id: other.view_id,
                expected_revision: other.revision,
                workspace_id: id(0x20_0000),
            }
        ),
        Err(limit("workspace-arrangements"))
    );
}

#[test]
fn retained_outcomes_are_bounded_and_only_expired_ones_are_pruned() {
    let f = Fixture::new(FakeCatalog::default());
    let a = owner(1);
    let view = f.ensure_default(a);
    let key = format!("tabs.personal.v1.{}", a.0);
    let mut document: OwnerDocument =
        serde_json::from_slice(&f.host.snapshot_storage()[&key]).unwrap();
    document.outcomes = (0..MAX_OUTCOMES as u128)
        .map(|n| StoredOutcome {
            mutation_id: id(0x30_0000 + n),
            fingerprint: "0".repeat(32),
            // The oldest is exactly at the retention boundary at `NOW`.
            recorded_at_ms: NOW - OUTCOME_RETENTION_MS + u64::try_from(n).unwrap(),
            view_id: view.view_id,
            revision: 1,
            member_id: None,
        })
        .collect();
    f.host.put(&key, serde_json::to_vec(&document).unwrap());
    let rename = |at: u64| {
        f.repo().apply(
            a,
            id(0x99),
            &Command::RenameView {
                view_id: view.view_id,
                expected_revision: view.revision,
                name: "renamed".into(),
            },
            &f.catalog,
            at,
        )
    };
    let stored = f.host.snapshot_storage();
    // One millisecond earlier nothing has expired: refuse, never evict.
    assert_eq!(rename(NOW - 1), Err(limit("retained-outcomes")));
    assert_eq!(f.host.snapshot_storage(), stored);
    // At `NOW` exactly the oldest outcome is 24 h old and is pruned.
    let ack = rename(NOW).unwrap();
    let document: OwnerDocument = serde_json::from_slice(&f.host.snapshot_storage()[&key]).unwrap();
    assert_eq!(document.outcomes.len(), MAX_OUTCOMES);
    assert!(
        document
            .outcomes
            .iter()
            .all(|o| o.mutation_id != id(0x30_0000))
    );
    assert_eq!(rename(NOW).unwrap(), ack);
}

#[test]
fn oversized_documents_are_refused_before_any_write() {
    let f = Fixture::new(FakeCatalog::default());
    let a = owner(1);
    let view = f.ensure_default(a);
    let key = format!("tabs.personal.v1.{}", a.0);
    let mut document: OwnerDocument =
        serde_json::from_slice(&f.host.snapshot_storage()[&key]).unwrap();
    // Valid but heavy: many views of long names and many outcomes.
    document.outcomes = (0..MAX_OUTCOMES as u128 - 96)
        .map(|n| StoredOutcome {
            mutation_id: id(0x40_0000 + n),
            fingerprint: "f".repeat(32),
            recorded_at_ms: NOW,
            view_id: id(u128::MAX - n),
            revision: u64::MAX,
            member_id: Some(id(u128::MAX - n)),
        })
        .collect();
    let mut views = vec![document.views[0].clone()];
    for n in 0..(MAX_VIEWS as u128 - 1) {
        views.push(StoredView {
            view_id: id(0x50_0000 + n),
            name: "n".repeat(MAX_TEXT_BYTES),
            revision: 1,
            workspaces: vec![StoredWorkspace {
                workspace_id: WS,
                initialized: true,
                members: (0..30)
                    .map(|m| StoredMember {
                        member_id: id(0x60_0000 + n * 100 + m),
                        context_id: id(0x70_0000 + m),
                        label: Some("l".repeat(MAX_TEXT_BYTES)),
                    })
                    .collect(),
            }],
        });
    }
    document.views = views;
    let bytes = serde_json::to_vec(&document).unwrap();
    assert!(bytes.len() > MAX_DOCUMENT_BYTES, "{}", bytes.len());
    f.host.put(&key, bytes);
    let stored = f.host.snapshot_storage();
    assert_eq!(
        f.repo().apply(
            a,
            id(0x123),
            &Command::RenameView {
                view_id: view.view_id,
                expected_revision: view.revision,
                name: "x".into(),
            },
            &f.catalog,
            NOW,
        ),
        Err(limit("document-bytes"))
    );
    assert_eq!(f.host.snapshot_storage(), stored);
}

#[test]
fn workspaces_seed_once_and_an_emptied_arrangement_stays_empty() {
    let order = tabs(0..3);
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &order));
    let a = owner(1);
    let view = f.ensure_default(a);
    assert_eq!(f.view(a, view.view_id).workspaces.len(), 0);
    // Queries never write, even for an uninitialized workspace.
    let stored = f.host.snapshot_storage();
    let _ = f.repo().default_view(a).unwrap();
    let _ = f.repo().list_views(a).unwrap();
    assert_eq!(f.host.snapshot_storage(), stored);

    let mut view = f.initialize(a, &view, WS);
    assert_eq!(f.members(a, view.view_id, WS), order);
    assert!(f.view(a, view.view_id).workspaces[0].initialized);
    for context in &order {
        view = f
            .apply(
                a,
                &Command::RemoveTab {
                    view_id: view.view_id,
                    expected_revision: view.revision,
                    member_id: f.member_id(a, view.view_id, *context),
                },
            )
            .unwrap();
    }
    let arrangement = f.view(a, view.view_id).workspaces[0].clone();
    assert!(arrangement.initialized && arrangement.members.is_empty());
    // Initializing again is a no-op; adding one tab adds only that tab.
    let again = f.initialize(a, &view, WS);
    assert_eq!(again.revision, view.revision);
    assert_eq!(f.members(a, view.view_id, WS).len(), 0);
    let added = f
        .apply(
            a,
            &Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: order[1],
                before: None,
            },
        )
        .unwrap();
    assert_eq!(f.members(a, added.view_id, WS), vec![order[1]]);
}

#[test]
fn adding_into_an_uninitialized_workspace_seeds_it_first() {
    let order = tabs(0..3);
    let mut catalog = FakeCatalog::with_workspace(WS, &order);
    let late = tabs(9..10)[0];
    catalog.live.insert(late, WS);
    let f = Fixture::new(catalog);
    let a = owner(1);
    let view = f.ensure_default(a);
    let added = f
        .apply(
            a,
            &Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: late,
                before: None,
            },
        )
        .unwrap();
    let mut expected = order.clone();
    expected.push(late);
    assert_eq!(f.members(a, view.view_id, WS), expected);
    assert_eq!(added.member_id, Some(f.member_id(a, view.view_id, late)));
    // A tab already in the canonical seed is found, not appended again.
    let view2 = f
        .apply(
            a,
            &Command::CreateView {
                view_id: id(0xF00),
                name: "two".into(),
            },
        )
        .unwrap();
    let found = f
        .apply(
            a,
            &Command::AddTab {
                view_id: view2.view_id,
                expected_revision: view2.revision,
                context_id: order[0],
                before: None,
            },
        )
        .unwrap();
    assert_eq!(f.members(a, view2.view_id, WS), order);
    assert_eq!(
        found.member_id,
        Some(f.member_id(a, view2.view_id, order[0]))
    );
    assert_eq!(found.revision, 2);
    // A tab that is not live is not a valid reference.
    assert_eq!(
        f.apply(
            a,
            &Command::AddTab {
                view_id: view2.view_id,
                expected_revision: found.revision,
                context_id: id(0xDEAD),
                before: None,
            }
        ),
        Err(PersonalViewError::InvalidReference)
    );
}

#[test]
fn duplicate_add_returns_the_existing_member_unmoved() {
    let order = tabs(0..3);
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &order));
    let a = owner(1);
    let view = f.ensure_default(a);
    let view = f.initialize(a, &view, WS);
    let existing = f.member_id(a, view.view_id, order[2]);
    let first_member = f.member_id(a, view.view_id, order[0]);
    let ack = f
        .apply(
            a,
            &Command::AddTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                context_id: order[2],
                before: Some(first_member),
            },
        )
        .unwrap();
    assert_eq!(ack.member_id, Some(existing));
    assert_eq!(ack.revision, view.revision);
    assert_eq!(f.members(a, view.view_id, WS), order);
}

#[test]
fn move_and_remove_edit_only_the_view() {
    let order = tabs(0..4);
    let other_ws = id(0x5_0002);
    let mut catalog = FakeCatalog::with_workspace(WS, &order);
    catalog.set_workspace(other_ws, &tabs(10..11));
    let f = Fixture::new(catalog);
    let a = owner(1);
    let view = f.ensure_default(a);
    let view = f.initialize(a, &view, WS);
    let view = f.initialize(a, &view, other_ws);
    let m = |n: usize| f.member_id(a, view.view_id, order[n]);
    let (m0, m1, m2, m3) = (m(0), m(1), m(2), m(3));
    let mv = |view: &PersonalAck, member, before| {
        f.apply(
            a,
            &Command::MoveTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id: member,
                before,
            },
        )
    };
    let view = mv(&view, m3, Some(m0)).unwrap();
    assert_eq!(
        f.members(a, view.view_id, WS),
        vec![order[3], order[0], order[1], order[2]]
    );
    let view = mv(&view, m3, None).unwrap();
    assert_eq!(f.members(a, view.view_id, WS), order);
    // Moving before itself, or to where it already is, changes nothing.
    assert_eq!(mv(&view, m1, Some(m1)).unwrap().revision, view.revision);
    assert_eq!(mv(&view, m0, Some(m1)).unwrap().revision, view.revision);
    // A position in another workspace or an unknown member is invalid.
    let foreign = f.member_id(a, view.view_id, tabs(10..11)[0]);
    assert_eq!(
        mv(&view, m1, Some(foreign)),
        Err(PersonalViewError::InvalidReference)
    );
    assert_eq!(
        mv(&view, id(0xBAD), None),
        Err(PersonalViewError::InvalidReference)
    );

    let removed = f
        .apply(
            a,
            &Command::RemoveTab {
                view_id: view.view_id,
                expected_revision: view.revision,
                member_id: m2,
            },
        )
        .unwrap();
    assert_eq!(removed.member_id, Some(m2));
    assert_eq!(
        f.members(a, view.view_id, WS),
        vec![order[0], order[1], order[3]]
    );
    // Member ids are stable identity across moves and removals.
    assert_eq!(f.member_id(a, view.view_id, order[3]), m3);
    assert_eq!(
        f.apply(
            a,
            &Command::RemoveTab {
                view_id: view.view_id,
                expected_revision: removed.revision,
                member_id: m2,
            }
        ),
        Err(PersonalViewError::InvalidReference)
    );
}

#[test]
fn the_default_view_cannot_be_deleted_until_another_is_default() {
    let f = Fixture::new(FakeCatalog::default());
    let a = owner(1);
    let default = f.ensure_default(a);
    // ensure-default is stable once a default exists.
    assert_eq!(f.ensure_default(a).view_id, default.view_id);
    assert_eq!(
        f.apply(
            a,
            &Command::DeleteView {
                view_id: default.view_id,
                expected_revision: default.revision,
            }
        ),
        Err(PersonalViewError::InvalidReference)
    );
    let other = f
        .apply(
            a,
            &Command::CreateView {
                view_id: id(0x2),
                name: "other".into(),
            },
        )
        .unwrap();
    assert_eq!(
        f.apply(
            a,
            &Command::CreateView {
                view_id: id(0x2),
                name: "again".into(),
            }
        ),
        Err(PersonalViewError::AlreadyExists)
    );
    f.apply(
        a,
        &Command::SetDefaultView {
            view_id: other.view_id,
        },
    )
    .unwrap();
    assert_eq!(f.repo().default_view(a).unwrap().view_id, other.view_id);
    f.apply(
        a,
        &Command::DeleteView {
            view_id: default.view_id,
            expected_revision: default.revision,
        },
    )
    .unwrap();
    assert_eq!(
        f.repo().get_view(a, default.view_id),
        Err(PersonalViewError::NotFound)
    );
    assert_eq!(f.repo().list_views(a).unwrap().len(), 1);
}

#[test]
fn confirmed_deletion_prunes_every_owner_idempotently() {
    let order = tabs(0..3);
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &order));
    let (a, b) = (owner(1), owner(2));
    let va = f.ensure_default(a);
    let va = f.initialize(a, &va, WS);
    let vb = f.ensure_default(b);
    let vb = f.initialize(b, &vb, WS);
    let untouched = owner(3);
    f.ensure_default(untouched);
    let states = PluginStateRegistry::new();

    assert_eq!(prune_deleted_context(&f.host, &states, order[1]), Ok(2));
    assert_eq!(f.members(a, va.view_id, WS), vec![order[0], order[2]]);
    assert_eq!(f.members(b, vb.view_id, WS), vec![order[0], order[2]]);
    assert_eq!(f.view(a, va.view_id).revision, va.revision + 1);
    let stored = f.host.snapshot_storage();
    assert_eq!(prune_deleted_context(&f.host, &states, order[1]), Ok(0));
    assert_eq!(f.host.snapshot_storage(), stored);
    // A merely absent tab is not pruned: only confirmation removes it.
    assert_eq!(f.members(a, va.view_id, WS), vec![order[0], order[2]]);
}

#[test]
fn an_index_entry_without_a_document_reads_as_an_owner_with_no_views() {
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &tabs(0..2)));
    let a = owner(1);
    // Crash between the index write and the document write.
    f.host.fail_sets(Some("tabs.personal.v1."));
    assert!(matches!(
        f.apply(a, &Command::EnsureDefault),
        Err(PersonalViewError::StorageUnavailable { .. })
    ));
    f.host.fail_sets(None);
    let storage = f.host.snapshot_storage();
    assert!(storage.contains_key("tabs.personal-index.v1"));
    assert!(!storage.contains_key(&format!("tabs.personal.v1.{}", a.0)));
    assert_eq!(f.repo().list_views(a).unwrap().len(), 0);
    assert_eq!(
        f.repo().default_view(a),
        Err(PersonalViewError::NotInitialized)
    );
    assert_eq!(
        prune_deleted_context(&f.host, &PluginStateRegistry::new(), id(1)),
        Ok(0)
    );
    assert_eq!(f.repo().snapshot_state().unwrap().documents.len(), 0);
    // The owner simply starts from scratch.
    let view = f.ensure_default(a);
    assert_eq!(f.repo().default_view(a).unwrap().view_id, view.view_id);
}

#[test]
fn a_document_missing_from_the_index_is_reindexed_by_its_next_commit() {
    let order = tabs(0..2);
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &order));
    let a = owner(1);
    let view = f.ensure_default(a);
    let view = f.initialize(a, &view, WS);
    // Simulate a document whose index entry was lost.
    f.host.put(
        "tabs.personal-index.v1",
        serde_json::to_vec(&OwnerIndex {
            schema_version: INDEX_SCHEMA_VERSION,
            owners: BTreeSet::new(),
        })
        .unwrap(),
    );
    let states = PluginStateRegistry::new();
    // Until reindexed it is invisible to pruning, but still fully readable.
    assert_eq!(prune_deleted_context(&f.host, &states, order[0]), Ok(0));
    assert_eq!(f.members(a, view.view_id, WS), order);
    let view = f
        .apply(
            a,
            &Command::RenameView {
                view_id: view.view_id,
                expected_revision: view.revision,
                name: "touched".into(),
            },
        )
        .unwrap();
    assert_eq!(prune_deleted_context(&f.host, &states, order[0]), Ok(1));
    assert_eq!(f.members(a, view.view_id, WS), vec![order[1]]);
}

#[test]
fn corrupt_or_unknown_state_is_reported_and_never_replaced() {
    let f = Fixture::new(FakeCatalog::default());
    let a = owner(1);
    let key = format!("tabs.personal.v1.{}", a.0);
    let cases: [Case; 4] = [
        (b"not json", |e| matches!(e, PersonalViewError::CorruptState { .. })),
        (
            br#"{"schema_version":2,"anything":true}"#,
            |e| matches!(e, PersonalViewError::UnsupportedVersion { schema_version: 2 }),
        ),
        (
            br#"{"schema_version":1,"owner":"00000000-0000-0000-0000-00000000a001","default_view":"00000000-0000-0000-0000-000000000009","views":[],"outcomes":[]}"#,
            |e| matches!(e, PersonalViewError::CorruptState { .. }),
        ),
        (
            br#"{"schema_version":1,"owner":"00000000-0000-0000-0000-00000000b001","default_view":null,"views":[],"outcomes":[]}"#,
            |e| matches!(e, PersonalViewError::CorruptState { .. }),
        ),
    ];
    for (bytes, expected) in cases {
        f.host.put(&key, bytes.to_vec());
        let stored = f.host.snapshot_storage();
        assert!(expected(&f.repo().list_views(a).unwrap_err()));
        assert!(expected(&f.repo().default_view(a).unwrap_err()));
        assert!(expected(&f.apply(a, &Command::EnsureDefault).unwrap_err()));
        assert_eq!(f.host.snapshot_storage(), stored);
    }
    // A corrupt index blocks commits and pruning explicitly.
    let b = owner(2);
    f.host.put("tabs.personal-index.v1", b"{".to_vec());
    assert!(matches!(
        f.apply(b, &Command::EnsureDefault),
        Err(PersonalViewError::CorruptState { .. })
    ));
    assert!(prune_deleted_context(&f.host, &PluginStateRegistry::new(), id(1)).is_err());
    f.host.put(
        "tabs.personal-index.v1",
        br#"{"schema_version":7,"owners":[]}"#.to_vec(),
    );
    assert!(matches!(
        f.apply(b, &Command::EnsureDefault),
        Err(PersonalViewError::UnsupportedVersion { schema_version: 7 })
    ));
}

#[test]
fn snapshots_round_trip_absent_references_and_outcomes() {
    let order = tabs(0..3);
    let f = Fixture::new(FakeCatalog::with_workspace(WS, &order));
    let (a, b) = (owner(1), owner(2));
    let view = f.ensure_default(a);
    let view = f.initialize(a, &view, WS);
    let mutation = id(0x5150);
    let label = Command::SetLabel {
        view_id: view.view_id,
        expected_revision: view.revision,
        member_id: f.member_id(a, view.view_id, order[0]),
        label: Some("mine".into()),
    };
    let labelled = f
        .repo()
        .apply(a, mutation, &label, &f.catalog, NOW)
        .unwrap();
    f.ensure_default(b);

    let snapshot = snapshot_bytes(&f.repo()).unwrap();
    assert_eq!(snapshot.id, SNAPSHOT_ID);
    assert_eq!(snapshot.version, 1);
    validate(&snapshot).unwrap();

    // Restore into an installation where none of the tabs are live and a
    // third owner exists that the snapshot does not know.
    let restored = Fixture::new(FakeCatalog::default());
    let c = owner(3);
    restored.ensure_default(c);
    let state = validate(&snapshot).unwrap();
    restored.repo().restore_state(&state).unwrap();
    assert_eq!(restored.view(a, view.view_id), f.view(a, view.view_id));
    assert_eq!(restored.members(a, view.view_id, WS), order);
    assert_eq!(restored.repo().list_views(b), f.repo().list_views(b));
    assert_eq!(restored.repo().list_views(c).unwrap().len(), 0);
    // Outcomes survive: the same mutation replays its recorded result.
    assert_eq!(
        restored
            .repo()
            .apply(a, mutation, &label, &restored.catalog, NOW + 1),
        Ok(labelled)
    );

    assert!(matches!(
        validate(&StatefulPluginSnapshot::new(SNAPSHOT_ID, 2, snapshot.bytes)),
        Err(StatefulPluginError::UnsupportedVersion { .. })
    ));
    assert!(validate(&StatefulPluginSnapshot::new(SNAPSHOT_ID, 1, b"{}".to_vec())).is_err());
    let mut duplicate = state;
    duplicate.documents.push(duplicate.documents[0].clone());
    assert!(
        validate(&StatefulPluginSnapshot::new(
            SNAPSHOT_ID,
            1,
            serde_json::to_vec(&duplicate).unwrap()
        ))
        .is_err()
    );
}

#[test]
fn created_tabs_join_the_creators_default_view_once() {
    let host = MemoryHost::new();
    let states = PluginStateRegistry::new();
    let existing = tabs(0..2);
    for tab in &existing {
        host.add_context(*tab, None);
    }
    let a = host.client(Some(id(0xA1)));
    let ensure = apply_for_caller(&a, &states, id(1), &Command::EnsureDefault).unwrap();
    let new_tab = id(0x7777);
    // Not yet arranged: the tab is picked up when the workspace is seeded.
    assert_eq!(
        join_created_tab_for_caller(&a, &states, new_tab, Uuid::nil(), None),
        Ok(CreatedTabJoin::WorkspaceNotInitialized {
            view_id: ensure.view_id
        })
    );
    let view = apply_for_caller(
        &a,
        &states,
        id(2),
        &Command::InitializeWorkspace {
            view_id: ensure.view_id,
            expected_revision: ensure.revision,
            workspace_id: Uuid::nil(),
        },
    )
    .unwrap();
    let Ok(CreatedTabJoin::Joined { member_id, .. }) =
        join_created_tab_for_caller(&a, &states, new_tab, Uuid::nil(), None)
    else {
        panic!("tab should join the default view");
    };
    // A retry never duplicates the member.
    assert_eq!(
        join_created_tab_for_caller(&a, &states, new_tab, Uuid::nil(), None),
        Ok(CreatedTabJoin::AlreadyMember {
            view_id: view.view_id,
            member_id
        })
    );
    let members: Vec<_> = default_view_for_caller(&a).unwrap().workspaces[0]
        .members
        .iter()
        .map(|member| member.context_id)
        .collect();
    assert_eq!(members, vec![existing[0], existing[1], new_tab]);
    // No caller principal, no view.
    assert_eq!(
        join_created_tab_for_caller(&host.client(None), &states, id(0x8888), Uuid::nil(), None),
        Ok(CreatedTabJoin::NoView)
    );
    // Another principal without views joins nothing.
    assert_eq!(
        join_created_tab_for_caller(
            &host.client(Some(id(0xB1))),
            &states,
            id(0x8888),
            Uuid::nil(),
            None
        ),
        Ok(CreatedTabJoin::NoView)
    );
}

#[test]
fn a_failed_view_update_never_fails_the_created_tab_and_a_retry_does_not_duplicate() {
    let host = MemoryHost::new();
    let runtime = crate::TabRuntimeStateHandle::default();
    let a = host.client(Some(id(0xA1)));
    let states = PluginStateRegistry::new();
    let ensure = apply_for_caller(&a, &states, id(1), &Command::EnsureDefault).unwrap();
    apply_for_caller(
        &a,
        &states,
        id(2),
        &Command::InitializeWorkspace {
            view_id: ensure.view_id,
            expected_revision: ensure.revision,
            workspace_id: Uuid::nil(),
        },
    )
    .unwrap();

    host.fail_sets(Some("tabs.personal.v1."));
    let (ack, joined) = crate::create_tab_joining_view(
        &a,
        &runtime,
        Some("kept".into()),
        &crate::cwd::DirectoryPolicy::RuntimeDefault,
        None,
        None,
    )
    .unwrap();
    assert!(ack.ok);
    assert!(joined.is_err(), "{joined:?}");
    let created = Uuid::parse_str(ack.id.as_deref().unwrap()).unwrap();
    // The tab exists and stays discoverable.
    assert!(
        host.contexts
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.id == created)
    );
    assert_eq!(
        default_view_for_caller(&a).unwrap().workspaces[0]
            .members
            .len(),
        0
    );

    host.fail_sets(None);
    let Ok(CreatedTabJoin::Joined { member_id, .. }) =
        join_created_tab_for_caller(&a, &states, created, Uuid::nil(), None)
    else {
        panic!("retry should join");
    };
    assert!(matches!(
        join_created_tab_for_caller(&a, &states, created, Uuid::nil(), None),
        Ok(CreatedTabJoin::AlreadyMember { member_id: again, .. }) if again == member_id
    ));
    assert_eq!(
        default_view_for_caller(&a).unwrap().workspaces[0]
            .members
            .len(),
        1
    );

    // A successful creation joins directly.
    let (ack, joined) = crate::create_tab_joining_view(
        &a,
        &runtime,
        None,
        &crate::cwd::DirectoryPolicy::RuntimeDefault,
        None,
        None,
    )
    .unwrap();
    let second = Uuid::parse_str(ack.id.as_deref().unwrap()).unwrap();
    assert!(matches!(joined, Ok(CreatedTabJoin::Joined { .. })));
    let members: Vec<_> = default_view_for_caller(&a).unwrap().workspaces[0]
        .members
        .iter()
        .map(|member| member.context_id)
        .collect();
    assert_eq!(members, vec![created, second]);

    // Created without a caller principal: joins no view.
    let (_, joined) = crate::create_tab_joining_view(
        &host.client(None),
        &runtime,
        None,
        &crate::cwd::DirectoryPolicy::RuntimeDefault,
        None,
        None,
    )
    .unwrap();
    assert_eq!(joined, Ok(CreatedTabJoin::NoView));
    assert_eq!(
        default_view_for_caller(&a).unwrap().workspaces[0]
            .members
            .len(),
        2
    );
}

#[test]
fn live_catalog_seeds_from_the_canonical_shared_order() {
    let host = MemoryHost::new();
    let states = PluginStateRegistry::new();
    let ws = id(0x5_0003);
    let live = tabs(0..3);
    for tab in &live {
        host.add_context(*tab, Some(ws));
    }
    host.add_context(id(0x1234), None); // another workspace
    // The canonical order keeps an absent reference and omits one live tab.
    let state = crate::arrangements::State {
        orders: BTreeMap::from([(ws, vec![live[2], id(0xAB5), live[0]])]),
        pending: BTreeMap::new(),
    };
    host.put("tabs.arrangements.v2", serde_json::to_vec(&state).unwrap());
    let a = host.client(Some(id(0xA1)));
    let ensure = apply_for_caller(&a, &states, id(1), &Command::EnsureDefault).unwrap();
    apply_for_caller(
        &a,
        &states,
        id(2),
        &Command::InitializeWorkspace {
            view_id: ensure.view_id,
            expected_revision: ensure.revision,
            workspace_id: ws,
        },
    )
    .unwrap();
    let members: Vec<_> = default_view_for_caller(&a).unwrap().workspaces[0]
        .members
        .iter()
        .map(|member| member.context_id)
        .collect();
    assert_eq!(members, vec![live[2], live[0], live[1]]);
}
