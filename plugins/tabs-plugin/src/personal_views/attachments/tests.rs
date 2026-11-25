use super::*;
use crate::personal_views::tests::MemoryHost;
use crate::personal_views::{CreatedTabJoin, join_created_tab_for_caller};
use bmux_plugin_sdk::WireEventSinkError;

const NOW: u64 = 1_000_000_000;
const WS: Uuid = Uuid::nil();
const OTHER_WS: Uuid = Uuid::from_u128(0x5_0001);

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn principal(n: u128) -> Uuid {
    id(0xA000 + n)
}

fn owner(n: u128) -> Owner {
    Owner::from_resolved_principal(principal(n)).unwrap()
}

/// Attached clients, their principals and selections, as the host sees them.
#[derive(Default)]
struct FakeHost {
    attached: Mutex<BTreeSet<Uuid>>,
    principals: Mutex<BTreeMap<Uuid, Uuid>>,
    selected: Mutex<BTreeMap<Uuid, Uuid>>,
}

impl FakeHost {
    fn attach(&self, client: Uuid, principal: Option<Uuid>, selected: Uuid) {
        self.attached.lock().unwrap().insert(client);
        if let Some(principal) = principal {
            self.principals.lock().unwrap().insert(client, principal);
        }
        self.select(client, selected);
    }

    fn detach(&self, client: Uuid) {
        self.attached.lock().unwrap().remove(&client);
    }

    fn select(&self, client: Uuid, context: Uuid) {
        self.selected.lock().unwrap().insert(client, context);
    }
}

impl AttachmentHost for FakeHost {
    fn attached_clients(&self) -> Option<Vec<Uuid>> {
        Some(self.attached.lock().unwrap().iter().copied().collect())
    }

    fn is_attached(&self, client: Uuid) -> bool {
        self.attached.lock().unwrap().contains(&client)
    }

    fn principal(&self, client: Uuid) -> Option<Uuid> {
        self.principals.lock().unwrap().get(&client).copied()
    }

    fn selected_context(&self, client: Uuid) -> Option<Uuid> {
        self.selected.lock().unwrap().get(&client).copied()
    }
}

/// Records every recipient-scoped publication. A broadcast is a test failure:
/// owner-scoped projections must never reach every client.
#[derive(Default)]
struct CapturingSink {
    retained: Mutex<Vec<(Uuid, String, Vec<u8>)>>,
    cleared: Mutex<Vec<(Uuid, String)>>,
    disconnected: Mutex<BTreeSet<Uuid>>,
}

impl CapturingSink {
    fn take(&self) -> Vec<(Uuid, PersonalViewProjection)> {
        std::mem::take(&mut *self.retained.lock().unwrap())
            .into_iter()
            .map(|(recipient, kind, payload)| {
                assert_eq!(kind, STATE_KIND.as_str());
                (recipient, serde_json::from_slice(&payload).unwrap())
            })
            .collect()
    }

    fn only_to(&self, recipient: Uuid) -> PersonalViewProjection {
        let published = self.take();
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(published[0].0, recipient);
        published.into_iter().next().unwrap().1
    }
}

impl WireEventSink for CapturingSink {
    fn publish(&self, event: bmux_ipc::Event) -> std::result::Result<(), WireEventSinkError> {
        panic!("personal projections must never be broadcast: {event:?}");
    }

    fn is_connected(&self, recipient: Uuid) -> Option<bool> {
        Some(!self.disconnected.lock().unwrap().contains(&recipient))
    }

    fn publish_retained_to(
        &self,
        recipient: Uuid,
        kind: String,
        payload: Vec<u8>,
    ) -> std::result::Result<(), WireEventSinkError> {
        self.retained
            .lock()
            .unwrap()
            .push((recipient, kind, payload));
        Ok(())
    }

    fn clear_retained_to(
        &self,
        recipient: Uuid,
        kind: &str,
    ) -> std::result::Result<(), WireEventSinkError> {
        self.cleared
            .lock()
            .unwrap()
            .push((recipient, kind.to_string()));
        Ok(())
    }
}

struct Fixture {
    storage: MemoryHost,
    host: FakeHost,
    sink: CapturingSink,
    attachments: Attachments,
}

impl Fixture {
    /// Live tabs `1..=count` (named `t1`, ...) in the default workspace.
    fn new(count: u128) -> Self {
        let fixture = Self {
            storage: MemoryHost::new(),
            host: FakeHost::default(),
            sink: CapturingSink::default(),
            attachments: Attachments::default(),
        };
        for n in 1..=count {
            fixture.add_tab(tab(n), Some(&format!("t{n}")), WS);
        }
        fixture
    }

    fn add_tab(&self, id: Uuid, name: Option<&str>, workspace: Uuid) {
        let mut attributes = BTreeMap::new();
        if !workspace.is_nil() {
            attributes.insert("workspace".into(), workspace.to_string());
        }
        self.storage.contexts.lock().unwrap().push(ContextSummary {
            id,
            name: name.map(Into::into),
            attributes,
        });
    }

    fn remove_tab(&self, id: Uuid) {
        self.storage
            .contexts
            .lock()
            .unwrap()
            .retain(|context| context.id != id);
    }

    fn move_tab_to(&self, id: Uuid, workspace: Uuid) {
        for context in self.storage.contexts.lock().unwrap().iter_mut() {
            if context.id == id {
                context
                    .attributes
                    .insert("workspace".into(), workspace.to_string());
            }
        }
    }

    fn env<'a>(&'a self, caller: &'a MemoryHost) -> Env<'a, MemoryHost> {
        Env {
            caller,
            states: None,
            host: &self.host,
            sink: Some(&self.sink),
            now_ms: NOW,
        }
    }

    fn refresh(&self) {
        let failures = self.attachments.refresh(&self.env(&self.storage));
        assert!(failures.is_empty(), "{failures:?}");
    }

    fn repo(&self) -> Repository<'_, MemoryHost> {
        Repository::new(&self.storage, None)
    }

    fn apply(&self, owner: Owner, command: &Command) -> super::super::PersonalAck {
        self.repo()
            .apply(
                owner,
                Uuid::new_v4(),
                command,
                &LiveCatalog::new(&self.storage),
                NOW,
            )
            .unwrap()
    }

    fn view(&self, owner: Owner, view_id: Uuid) -> PersonalView {
        self.repo().get_view(owner, view_id).unwrap()
    }

    fn member(&self, owner: Owner, view_id: Uuid, context: Uuid) -> Uuid {
        self.view(owner, view_id)
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.members)
            .find(|member| member.context_id == context)
            .unwrap()
            .member_id
    }

    /// Invoked as `client`, bound by the host to principal `n`.
    fn as_client(&self, n: Option<u128>) -> MemoryHost {
        self.storage.client(n.map(principal))
    }
}

fn tab(n: u128) -> Uuid {
    id(0x7000 + n)
}

fn client(n: u128) -> Uuid {
    id(0xC000 + n)
}

/// `(context, name, active)` of each projected member of `workspace`.
fn members(projection: &PersonalViewProjection, workspace: Uuid) -> Vec<(Uuid, String, bool)> {
    projection
        .workspaces
        .iter()
        .find(|arrangement| arrangement.workspace_id == workspace)
        .map(|arrangement| {
            arrangement
                .members
                .iter()
                .map(|member| (member.context_id, member.name.clone(), member.active))
                .collect()
        })
        .unwrap_or_default()
}

fn names(projection: &PersonalViewProjection) -> Vec<String> {
    members(projection, WS)
        .into_iter()
        .map(|(_, name, _)| name)
        .collect()
}

fn active(projection: &PersonalViewProjection) -> Vec<Uuid> {
    members(projection, WS)
        .into_iter()
        .filter_map(|(context, _, active)| active.then_some(context))
        .collect()
}

#[test]
fn attached_clients_bind_to_their_default_view_and_retire_on_detach_or_disconnect() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(2), Some(principal(2)), tab(2));
    // Attached without a principal: no views, so no binding or projection.
    fixture.host.attach(client(3), None, tab(1));
    fixture.refresh();

    let mut published = fixture.sink.take();
    published.sort_by_key(|(recipient, _)| *recipient);
    assert_eq!(published.len(), 2, "{published:?}");
    let (a, b) = (&published[0], &published[1]);
    assert_eq!((a.0, b.0), (client(1), client(2)));
    // Each owner's own default view, seeded once from the canonical order.
    let a_default = fixture.repo().default_view(owner(1)).unwrap();
    let b_default = fixture.repo().default_view(owner(2)).unwrap();
    assert_eq!(a.1.view_id, a_default.view_id);
    assert_eq!(b.1.view_id, b_default.view_id);
    assert_ne!(a.1.view_id, b.1.view_id);
    assert_eq!(names(&a.1), ["t1", "t2", "t3"]);
    assert_eq!(active(&a.1), [tab(1)]);
    assert_eq!(active(&b.1), [tab(2)]);
    assert_eq!(
        fixture.attachments.bound_view(Some(client(1))),
        Some((owner(1), a_default.view_id))
    );
    assert_eq!(fixture.attachments.bound_view(Some(client(3))), None);

    // Nothing changed: nothing is republished.
    fixture.refresh();
    assert_eq!(fixture.sink.take(), []);

    // Detaching retires the binding and clears what was retained for it.
    fixture.host.detach(client(1));
    fixture.refresh();
    assert_eq!(fixture.attachments.bound_view(Some(client(1))), None);
    assert_eq!(
        *fixture.sink.cleared.lock().unwrap(),
        [(client(1), STATE_KIND.as_str().to_string())]
    );
    assert_eq!(fixture.sink.take(), []);

    // A disconnected client is retired too; its retention is already gone.
    fixture.sink.disconnected.lock().unwrap().insert(client(2));
    fixture.refresh();
    assert_eq!(fixture.attachments.bound_view(Some(client(2))), None);
    assert_eq!(fixture.sink.cleared.lock().unwrap().len(), 1);

    // A fresh attach of the same owner reuses its view, never reseeding it.
    fixture.apply(
        owner(1),
        &Command::RemoveTab {
            view_id: a_default.view_id,
            expected_revision: a_default.revision,
            member_id: fixture.member(owner(1), a_default.view_id, tab(3)),
        },
    );
    fixture.host.attach(client(4), Some(principal(1)), tab(2));
    fixture.refresh();
    let fresh = fixture.sink.only_to(client(4));
    assert_eq!(fresh.view_id, a_default.view_id);
    assert_eq!(names(&fresh), ["t1", "t2"]);
    assert_eq!(fresh.binding_revision, 2, "the selected member was derived");
}

#[test]
fn projections_omit_dangling_and_moved_members_and_prefer_labels() {
    let fixture = Fixture::new(3);
    fixture.add_tab(tab(4), None, WS);
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.refresh();
    let view_id = fixture.sink.only_to(client(1)).view_id;
    let view = fixture.view(owner(1), view_id);
    fixture.apply(
        owner(1),
        &Command::SetLabel {
            view_id,
            expected_revision: view.revision,
            member_id: fixture.member(owner(1), view_id, tab(2)),
            label: Some("mine".into()),
        },
    );
    // t1 is deleted while no deletion was observed (a dangling reference),
    // and t3 now lives in another workspace.
    fixture.remove_tab(tab(1));
    fixture.move_tab_to(tab(3), OTHER_WS);
    fixture.refresh();
    let projection = fixture.sink.only_to(client(1));
    assert_eq!(
        members(&projection, WS),
        [
            (tab(2), "mine".to_string(), true),
            // Unnamed tabs take their position in the projected arrangement.
            (tab(4), "tab-2".to_string(), false),
        ]
    );
    // Both references are retained in the view itself.
    assert_eq!(
        fixture.view(owner(1), view_id).workspaces[0].members.len(),
        4
    );
    assert_eq!(projection.active_workspace_id, WS);
    assert_eq!(projection.view_revision, view.revision + 1);
}

#[test]
fn attachments_sharing_a_view_see_its_edits_with_their_own_active_tab() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(2), Some(principal(1)), tab(3));
    fixture.host.attach(client(3), Some(principal(2)), tab(1));
    fixture.refresh();
    let mut published = fixture.sink.take();
    published.sort_by_key(|(recipient, _)| *recipient);
    let view_id = published[0].1.view_id;
    assert_eq!(
        published[1].1.view_id, view_id,
        "one owner, one default view"
    );
    assert_eq!(active(&published[0].1), [tab(1)]);
    assert_eq!(active(&published[1].1), [tab(3)]);

    // One attachment's edit (here through the repository, as any caller of
    // the owner) reaches every attachment bound to that view, and only them.
    let view = fixture.view(owner(1), view_id);
    fixture.apply(
        owner(1),
        &Command::MoveTab {
            view_id,
            expected_revision: view.revision,
            member_id: fixture.member(owner(1), view_id, tab(3)),
            before: Some(fixture.member(owner(1), view_id, tab(1))),
        },
    );
    fixture.refresh();
    let mut published = fixture.sink.take();
    published.sort_by_key(|(recipient, _)| *recipient);
    let recipients: Vec<_> = published.iter().map(|(recipient, _)| *recipient).collect();
    assert_eq!(recipients, [client(1), client(2)]);
    for (_, projection) in &published {
        assert_eq!(names(projection), ["t3", "t1", "t2"]);
    }
    assert_eq!(active(&published[0].1), [tab(1)]);
    assert_eq!(active(&published[1].1), [tab(3)]);

    // A selection change republishes only to the attachment that moved.
    fixture.host.select(client(2), tab(2));
    fixture.refresh();
    let projection = fixture.sink.only_to(client(2));
    assert_eq!(active(&projection), [tab(2)]);
}

#[test]
fn bind_view_accepts_only_the_callers_own_view_at_the_current_revision() {
    let fixture = Fixture::new(2);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    let work = id(0xBEEF);
    fixture.apply(
        owner(1),
        &Command::CreateView {
            view_id: work,
            name: "work".into(),
        },
    );
    let foreign = fixture.apply(owner(2), &Command::EnsureDefault).view_id;
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);

    // The first attachment call creates the binding lazily.
    let binding = fixture
        .attachments
        .current_binding(&env, Some(client(1)))
        .unwrap();
    let default = fixture.repo().default_view(owner(1)).unwrap().view_id;
    assert_eq!(binding.view_id, default);
    assert_eq!(binding.selected_member, None, "not seeded yet");

    assert_eq!(
        fixture
            .attachments
            .bind_view(&env, Some(client(1)), foreign, binding.revision),
        Err(PersonalAttachmentError::NotFound),
        "another owner's view is indistinguishable from an absent one"
    );
    assert_eq!(
        fixture
            .attachments
            .bind_view(&env, Some(client(1)), work, binding.revision + 1),
        Err(PersonalAttachmentError::RevisionConflict {
            current_revision: binding.revision
        })
    );
    let bound = fixture
        .attachments
        .bind_view(&env, Some(client(1)), work, binding.revision)
        .unwrap();
    assert_eq!(bound.view_id, work);
    assert_eq!(bound.revision, binding.revision + 1);
    assert_eq!(
        fixture.attachments.bound_view(Some(client(1))),
        Some((owner(1), work))
    );
    fixture.refresh();
    let projection = fixture.sink.only_to(client(1));
    assert_eq!(projection.view_id, work);
    assert_eq!(projection.name, "work");
    assert_eq!(names(&projection), ["t1", "t2"], "seeded on first access");

    // No attach stream, or no principal: no binding.
    let b = fixture.as_client(Some(2));
    assert_eq!(
        fixture
            .attachments
            .current_binding(&fixture.env(&b), Some(client(2))),
        Err(PersonalAttachmentError::NoAttachment)
    );
    assert_eq!(
        fixture.attachments.current_binding(&env, None),
        Err(PersonalAttachmentError::NoAttachment)
    );
    let nobody = fixture.as_client(None);
    assert_eq!(
        fixture
            .attachments
            .current_binding(&fixture.env(&nobody), Some(client(1))),
        Err(PersonalAttachmentError::NoPrincipal)
    );
}

#[test]
fn select_member_checks_membership_then_selects_through_the_tab_switch() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.refresh();
    fixture.sink.take();
    // Another owner's view of the same tabs.
    let foreign_view = fixture.apply(owner(2), &Command::EnsureDefault);
    fixture.apply(
        owner(2),
        &Command::InitializeWorkspace {
            view_id: foreign_view.view_id,
            expected_revision: foreign_view.revision,
            workspace_id: WS,
        },
    );
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    let binding = fixture
        .attachments
        .current_binding(&env, Some(client(1)))
        .unwrap();
    let view_id = binding.view_id;
    let t2 = fixture.member(owner(1), view_id, tab(2));
    let t3 = fixture.member(owner(1), view_id, tab(3));
    let foreign = fixture.member(owner(2), foreign_view.view_id, tab(2));
    let never = |_: Uuid| -> std::result::Result<(), String> {
        panic!("a rejected member must not be selected")
    };

    assert_eq!(
        fixture
            .attachments
            .select_member(&env, Some(client(1)), foreign, binding.revision, never),
        Err(PersonalAttachmentError::InvalidReference),
        "another owner's member is not in this view"
    );
    assert_eq!(
        fixture
            .attachments
            .select_member(&env, Some(client(1)), t2, binding.revision + 1, never),
        Err(PersonalAttachmentError::RevisionConflict {
            current_revision: binding.revision
        })
    );
    fixture.remove_tab(tab(3));
    assert_eq!(
        fixture
            .attachments
            .select_member(&env, Some(client(1)), t3, binding.revision, never),
        Err(PersonalAttachmentError::InvalidReference),
        "a dangling member's tab is not live"
    );
    assert_eq!(
        fixture
            .attachments
            .select_member(&env, Some(client(1)), t2, binding.revision, |_| Err(
                "selection refused".into()
            )),
        Err(PersonalAttachmentError::Unavailable {
            reason: "selection refused".into()
        })
    );

    let mut selected = Vec::new();
    let after = fixture
        .attachments
        .select_member(&env, Some(client(1)), t2, binding.revision, |context| {
            selected.push(context);
            fixture.host.select(client(1), context);
            Ok(())
        })
        .unwrap();
    assert_eq!(selected, [tab(2)]);
    assert_eq!(after.selected_member, Some(t2));
    assert_eq!(after.revision, binding.revision + 1);
    fixture.refresh();
    let projection = fixture.sink.only_to(client(1));
    assert_eq!(active(&projection), [tab(2)]);
    assert_eq!(projection.binding_revision, after.revision);
}

#[test]
fn oversized_projections_are_refused_and_never_truncated() {
    let fixture = Fixture::new(0);
    let long = "x".repeat(1024);
    for n in 1..=300 {
        fixture.add_tab(tab(n), Some(&long), WS);
    }
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    let failures = fixture.attachments.refresh(&fixture.env(&fixture.storage));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("delivery limit"), "{failures:?}");
    assert_eq!(fixture.sink.take(), []);
}

#[test]
fn created_tabs_join_the_creating_attachments_bound_view() {
    let fixture = Fixture::new(1);
    let work = id(0xBEEF);
    let default = fixture.apply(owner(1), &Command::EnsureDefault).view_id;
    fixture.apply(
        owner(1),
        &Command::CreateView {
            view_id: work,
            name: "work".into(),
        },
    );
    for view_id in [default, work] {
        let view = fixture.view(owner(1), view_id);
        fixture.apply(
            owner(1),
            &Command::InitializeWorkspace {
                view_id,
                expected_revision: view.revision,
                workspace_id: WS,
            },
        );
    }
    let states = PluginStateRegistry::new();
    let a = fixture.as_client(Some(1));
    let in_view = |view_id: Uuid, context: Uuid| {
        fixture
            .view(owner(1), view_id)
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.members)
            .any(|member| member.context_id == context)
    };

    // Bound to "work": the new tab joins it, not the default view.
    fixture.add_tab(tab(2), None, WS);
    let joined =
        join_created_tab_for_caller(&a, &states, tab(2), WS, Some((owner(1), work))).unwrap();
    assert!(matches!(joined, CreatedTabJoin::Joined { view_id, .. } if view_id == work));
    assert!(in_view(work, tab(2)) && !in_view(default, tab(2)));

    // An attachment-less caller (a CLI connection) joins the default view,
    // and so does a binding made for another owner.
    fixture.add_tab(tab(3), None, WS);
    join_created_tab_for_caller(&a, &states, tab(3), WS, None).unwrap();
    fixture.add_tab(tab(4), None, WS);
    join_created_tab_for_caller(&a, &states, tab(4), WS, Some((owner(2), work))).unwrap();
    assert!(in_view(default, tab(3)) && in_view(default, tab(4)));
    assert!(!in_view(work, tab(3)) && !in_view(work, tab(4)));

    // A bound view deleted meanwhile falls back to the default view.
    let view = fixture.view(owner(1), work);
    fixture.apply(
        owner(1),
        &Command::DeleteView {
            view_id: work,
            expected_revision: view.revision,
        },
    );
    fixture.add_tab(tab(5), None, WS);
    join_created_tab_for_caller(&a, &states, tab(5), WS, Some((owner(1), work))).unwrap();
    assert!(in_view(default, tab(5)));
}
