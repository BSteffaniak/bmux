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
    /// Selections made on attachments' behalf: `(client, tab)`, `None` for
    /// a cleared selection.
    moved: Mutex<Vec<(Uuid, Option<Uuid>)>>,
    /// Follower to leader.
    follows: Mutex<BTreeMap<Uuid, Uuid>>,
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

impl FakeHost {
    fn take_moved(&self) -> Vec<(Uuid, Option<Uuid>)> {
        let mut moved = std::mem::take(&mut *self.moved.lock().unwrap());
        moved.sort();
        moved
    }
}

impl ClientSelection for FakeHost {
    fn select_for(&self, client: Uuid, context: Uuid) -> std::result::Result<(), String> {
        self.moved.lock().unwrap().push((client, Some(context)));
        self.select(client, context);
        Ok(())
    }

    fn clear_for(&self, client: Uuid) -> std::result::Result<(), String> {
        self.moved.lock().unwrap().push((client, None));
        self.selected.lock().unwrap().remove(&client);
        Ok(())
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

    fn following(&self, client: Uuid) -> Option<Uuid> {
        self.follows.lock().unwrap().get(&client).copied()
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
            selection: &self.host,
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

fn no_selection(_: Uuid) -> std::result::Result<(), String> {
    panic!("this edit must not select a tab")
}

#[test]
fn navigation_walks_each_callers_own_view_and_falls_back_without_one() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(2), Some(principal(2)), tab(1));
    fixture.refresh();
    let a = fixture.as_client(Some(1));
    let b = fixture.as_client(Some(2));
    let order = |caller: &MemoryHost, client: Option<Uuid>, workspace: Uuid| {
        fixture
            .attachments
            .navigation_order(&fixture.env(caller), client, workspace)
            .unwrap()
    };
    let seeded = vec![tab(1), tab(2), tab(3)];
    assert_eq!(order(&a, Some(client(1)), WS), Some(seeded.clone()));

    // A's reorder changes only A's order.
    fixture
        .attachments
        .edit_view(
            &fixture.env(&a),
            Some(client(1)),
            tab(3),
            ViewEdit::Move {
                before: Some(tab(1)),
            },
            no_selection,
        )
        .unwrap();
    assert_eq!(
        order(&a, Some(client(1)), WS),
        Some(vec![tab(3), tab(1), tab(2)])
    );
    assert_eq!(order(&b, Some(client(2)), WS), Some(seeded));

    // An attached caller's first visit to a workspace seeds it.
    fixture.add_tab(tab(9), None, OTHER_WS);
    assert_eq!(order(&a, Some(client(1)), OTHER_WS), Some(vec![tab(9)]));

    // An unattached caller (a CLI command) walks its default view where it
    // already arranges the workspace, and falls back elsewhere.
    assert_eq!(
        order(&a, Some(client(7)), WS),
        Some(vec![tab(3), tab(1), tab(2)])
    );
    let fresh_ws = id(0x5_0002);
    fixture.add_tab(tab(8), None, fresh_ws);
    assert_eq!(order(&a, Some(client(7)), fresh_ws), None);
    // No principal, or no client at all: the shared order applies.
    assert_eq!(order(&fixture.as_client(None), Some(client(8)), WS), None);
    assert_eq!(order(&a, None, WS), None);
    // A principal that never had a view: nothing is created by navigating.
    let c = fixture.as_client(Some(3));
    assert_eq!(order(&c, Some(client(9)), WS), None);
    assert_eq!(fixture.repo().list_views(owner(3)).unwrap().len(), 0);
}

#[test]
fn tab_addressed_edits_change_only_the_bound_view_and_move_the_editor_off_a_removed_tab() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.refresh();
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    let order = || {
        fixture
            .attachments
            .navigation_order(&env, Some(client(1)), WS)
            .unwrap()
            .unwrap()
    };
    let edit = |context, edit, select: &mut Vec<Uuid>| {
        fixture
            .attachments
            .edit_view(&env, Some(client(1)), context, edit, |tab| {
                select.push(tab);
                fixture.host.select(client(1), tab);
                Ok(())
            })
    };
    let mut selected = Vec::new();

    // Removing the selected tab selects the next member; the tab lives on.
    let removed = edit(tab(1), ViewEdit::Remove, &mut selected).unwrap();
    assert_eq!(selected, [tab(2)]);
    assert_eq!(order(), [tab(2), tab(3)]);
    assert!(
        crate::list_contexts(&fixture.storage)
            .unwrap()
            .iter()
            .any(|context| context.id == tab(1))
    );
    // Removing an absent tab changes nothing.
    let again = edit(tab(1), ViewEdit::Remove, &mut selected).unwrap();
    assert_eq!(again.revision, removed.revision);
    assert_eq!(again.member_id, None);
    // The last member's neighbour is the previous one.
    fixture.host.select(client(1), tab(3));
    selected.clear();
    edit(tab(3), ViewEdit::Remove, &mut selected).unwrap();
    assert_eq!(selected, [tab(2)]);
    assert_eq!(order(), [tab(2)]);

    // Adding appends once; a repeat is unchanged.
    let added = edit(tab(1), ViewEdit::Add, &mut selected).unwrap();
    assert_eq!(order(), [tab(2), tab(1)]);
    let repeat = edit(tab(1), ViewEdit::Add, &mut selected).unwrap();
    assert_eq!(
        (repeat.revision, repeat.member_id),
        (added.revision, added.member_id)
    );
    // Moving is by tab; `before` must be a member of the same arrangement.
    edit(
        tab(1),
        ViewEdit::Move {
            before: Some(tab(2)),
        },
        &mut selected,
    )
    .unwrap();
    assert_eq!(order(), [tab(1), tab(2)]);
    edit(tab(1), ViewEdit::Move { before: None }, &mut selected).unwrap();
    assert_eq!(order(), [tab(2), tab(1)]);
    assert_eq!(
        edit(
            tab(1),
            ViewEdit::Move {
                before: Some(tab(3))
            },
            &mut selected
        ),
        Err(PersonalAttachmentError::InvalidReference),
        "tab 3 is no longer in the view"
    );
    assert_eq!(
        edit(tab(3), ViewEdit::Move { before: None }, &mut selected),
        Err(PersonalAttachmentError::InvalidReference)
    );
    assert_eq!(
        edit(id(0xDEAD), ViewEdit::Add, &mut selected),
        Err(PersonalAttachmentError::InvalidReference),
        "only live tabs can be added"
    );
    assert_eq!(selected, [tab(2)], "only removing the selected tab selects");

    // Only an attachment edits a bound view.
    assert_eq!(
        fixture.attachments.edit_view(
            &env,
            Some(client(5)),
            tab(1),
            ViewEdit::Remove,
            no_selection
        ),
        Err(PersonalAttachmentError::NoAttachment)
    );
}

/// The projection most recently published to `client`, after a refresh.
fn projection_for(fixture: &Fixture, client: Uuid) -> PersonalViewProjection {
    fixture.refresh();
    fixture
        .sink
        .take()
        .into_iter()
        .rev()
        .find(|(recipient, _)| *recipient == client)
        .map(|(_, projection)| projection)
        .expect("a projection for the client")
}

#[test]
fn removal_moves_every_attachment_of_the_view_off_the_tab_and_empties_to_no_selection() {
    // Three tabs in a non-default workspace, so the held workspace shows.
    let fixture = Fixture::new(0);
    for n in 1..=3 {
        fixture.add_tab(tab(n), Some(&format!("t{n}")), OTHER_WS);
    }
    // Two attachments of one owner share its default view; another owner's
    // attachment shows the same tab in its own view.
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.host.attach(client(2), Some(principal(1)), tab(2));
    fixture.host.attach(client(3), Some(principal(2)), tab(2));
    fixture.refresh();
    fixture.sink.take();
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    let remove = |context, selected: &mut Vec<Uuid>| {
        fixture
            .attachments
            .edit_view(&env, Some(client(1)), context, ViewEdit::Remove, |tab| {
                selected.push(tab);
                fixture.host.select(client(1), tab);
                Ok(())
            })
            .unwrap()
    };
    let mut selected = Vec::new();

    // The invoker selects the next member itself; the other attachment of
    // the view is moved there through the selection authority; the other
    // owner's attachment is untouched.
    remove(tab(2), &mut selected);
    assert_eq!(selected, [tab(3)]);
    assert_eq!(fixture.host.take_moved(), [(client(2), Some(tab(3)))]);
    assert_eq!(fixture.host.selected_context(client(3)), Some(tab(2)));

    // The last member's neighbour is the previous one.
    selected.clear();
    remove(tab(3), &mut selected);
    assert_eq!(selected, [tab(1)]);
    assert_eq!(fixture.host.take_moved(), [(client(2), Some(tab(1)))]);

    // Removing the last live member leaves both with no selection.
    selected.clear();
    remove(tab(1), &mut selected);
    assert_eq!(selected, Vec::<Uuid>::new());
    assert_eq!(
        fixture.host.take_moved(),
        [(client(1), None), (client(2), None)]
    );
    // Removing a reference never terminates the tab.
    let live: Vec<Uuid> = crate::list_contexts(&fixture.storage)
        .unwrap()
        .iter()
        .map(|context| context.id)
        .collect();
    assert_eq!(live, [tab(1), tab(2), tab(3)]);

    // The emptied attachments keep showing that workspace, with the
    // invoker's last removal for undo; the other attachment has none.
    fixture.refresh();
    let published = fixture.sink.take();
    let latest = |client| {
        published
            .iter()
            .rev()
            .find(|(recipient, _)| *recipient == client)
            .map(|(_, projection)| projection.clone())
            .expect("a projection for the client")
    };
    let first = latest(client(1));
    assert_eq!(first.active_workspace_id, OTHER_WS);
    assert_eq!(members(&first, OTHER_WS), Vec::new());
    let removal = first.last_removed.expect("last removal");
    assert_eq!(
        (
            removal.context_id,
            removal.name.as_str(),
            removal.removed_at_ms
        ),
        (tab(1), "t1", NOW)
    );
    let second = latest(client(2));
    assert_eq!(second.active_workspace_id, OTHER_WS);
    assert_eq!(second.last_removed, None);
}

#[test]
fn undo_restores_the_last_removal_in_place_once_and_only_for_a_live_tab() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.refresh();
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    let order = || {
        fixture
            .attachments
            .navigation_order(&env, Some(client(1)), WS)
            .unwrap()
            .unwrap()
    };
    let remove = |context| {
        fixture
            .attachments
            .edit_view(&env, Some(client(1)), context, ViewEdit::Remove, |tab| {
                fixture.host.select(client(1), tab);
                Ok(())
            })
            .unwrap()
    };

    // Nothing to undo yet.
    assert!(matches!(
        fixture.attachments.undo_removal(&env, Some(client(1))),
        Err(PersonalAttachmentError::Unavailable { .. })
    ));

    remove(tab(2));
    assert_eq!(order(), [tab(1), tab(3)]);
    let (removal, _) = fixture
        .attachments
        .undo_removal(&env, Some(client(1)))
        .unwrap();
    // Back in its place, and it was the selected tab (the caller reselects).
    assert_eq!(order(), [tab(1), tab(2), tab(3)]);
    assert_eq!((removal.context_id, removal.was_selected), (tab(2), true));
    assert_eq!(projection_for(&fixture, client(1)).last_removed, None);
    // Only the last removal, once.
    assert!(
        fixture
            .attachments
            .undo_removal(&env, Some(client(1)))
            .is_err()
    );

    // A removed tab that was not selected, then closed: undo refuses and
    // forgets it.
    fixture.host.select(client(1), tab(1));
    remove(tab(3));
    fixture.remove_tab(tab(3));
    assert_eq!(
        fixture
            .attachments
            .undo_removal(&env, Some(client(1)))
            .map(|_| ()),
        Err(PersonalAttachmentError::InvalidReference)
    );
    assert!(matches!(
        fixture.attachments.undo_removal(&env, Some(client(1))),
        Err(PersonalAttachmentError::Unavailable { .. })
    ));
}

#[test]
fn deletion_moves_attachments_that_showed_the_tab_within_their_own_views() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.host.attach(client(2), Some(principal(1)), tab(2));
    fixture.host.attach(client(3), Some(principal(2)), tab(2));
    fixture.host.attach(client(4), Some(principal(2)), tab(1));
    fixture.refresh();
    // Owner 2 orders its view t2, t1, t3: its neighbour of t2 is t1.
    let b = fixture.repo().default_view(owner(2)).unwrap();
    fixture.apply(
        owner(2),
        &Command::MoveTab {
            view_id: b.view_id,
            expected_revision: b.revision,
            member_id: fixture.member(owner(2), b.view_id, tab(2)),
            before: Some(fixture.member(owner(2), b.view_id, tab(1))),
        },
    );
    fixture.refresh();
    fixture.sink.take();

    // The tab is closed for everyone. The contexts authority already moved
    // client 1 to another tab of its view (it stays); client 2 still names
    // the deleted tab, client 3 lost its selection.
    fixture.remove_tab(tab(2));
    fixture.host.select(client(1), tab(1));
    fixture.host.selected.lock().unwrap().remove(&client(3));
    let failures = fixture
        .attachments
        .reconcile_deleted(&fixture.env(&fixture.storage), tab(2));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        fixture.host.take_moved(),
        [(client(2), Some(tab(3))), (client(3), Some(tab(1)))]
    );

    // A deletion that leaves an attachment's arrangement without live tabs
    // leaves it with no selection; attachments on other tabs stay.
    fixture.refresh();
    fixture.sink.take();
    fixture.remove_tab(tab(1));
    fixture.remove_tab(tab(3));
    let failures = fixture
        .attachments
        .reconcile_deleted(&fixture.env(&fixture.storage), tab(3));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(fixture.host.take_moved(), [(client(2), None)]);
}

/// The latest projection published to `client` in `published`.
fn latest(published: &[(Uuid, PersonalViewProjection)], client: Uuid) -> PersonalViewProjection {
    published
        .iter()
        .rev()
        .find(|(recipient, _)| *recipient == client)
        .map(|(_, projection)| projection.clone())
        .expect("a projection for the client")
}

#[test]
fn one_owner_keeps_independent_arrangements_in_two_views_on_two_attachments() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(2), Some(principal(1)), tab(1));
    fixture.refresh();
    let default = fixture.sink.take()[0].1.view_id;
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);

    // A copy of the current view switches only the creating attachment.
    let work = fixture
        .attachments
        .create_view(&env, Some(client(1)), "work", true)
        .unwrap();
    assert_eq!(work.name, "work");
    assert!(!work.is_default);
    assert_eq!(
        fixture.attachments.bound_view(Some(client(1))),
        Some((owner(1), work.view_id))
    );
    assert_eq!(
        fixture.attachments.bound_view(Some(client(2))),
        Some((owner(1), default))
    );
    // Its selection is a tab of the copy, so it stays.
    assert_eq!(fixture.host.take_moved(), []);

    // Editing one view leaves the other alone.
    fixture
        .attachments
        .edit_view(&env, Some(client(1)), tab(2), ViewEdit::Remove, |_| Ok(()))
        .unwrap();
    fixture.refresh();
    let first = fixture.sink.only_to(client(1));
    assert_eq!(names(&first), ["t1", "t3"]);
    assert_eq!((first.name.as_str(), first.is_default), ("work", false));
    // The second attachment's (default) view did not change, so nothing was
    // republished to it.
    assert_eq!(
        fixture.view(owner(1), default).workspaces[0].members.len(),
        3
    );

    // Switching the second attachment shows the copy's arrangement there.
    fixture
        .attachments
        .switch_view(&env, Some(client(2)), work.view_id)
        .unwrap();
    fixture.refresh();
    assert_eq!(names(&fixture.sink.only_to(client(2))), ["t1", "t3"]);
}

#[test]
fn an_empty_view_starts_with_no_tabs_and_switching_moves_off_tabs_it_lacks() {
    let fixture = Fixture::new(2);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.refresh();
    fixture.sink.take();
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);

    // An empty view arranges every workspace with tabs as empty: switching
    // to it leaves the attachment with no input target in that workspace.
    let empty = fixture
        .attachments
        .create_view(&env, Some(client(1)), "scratch", false)
        .unwrap();
    assert!(
        empty
            .workspaces
            .iter()
            .all(|arrangement| { arrangement.initialized && arrangement.members.is_empty() })
    );
    assert_eq!(fixture.host.take_moved(), [(client(1), None)]);
    fixture.refresh();
    let projection = fixture.sink.only_to(client(1));
    assert_eq!(projection.view_id, empty.view_id);
    assert_eq!(names(&projection), Vec::<String>::new());

    // Adding a tab, then switching back: the default view holds the tab
    // again; a view without the selected tab moves to its first tab.
    fixture.host.select(client(1), tab(2));
    fixture
        .attachments
        .edit_view(&env, Some(client(1)), tab(2), ViewEdit::Add, |_| Ok(()))
        .unwrap();
    let default = fixture.repo().default_view(owner(1)).unwrap().view_id;
    fixture
        .attachments
        .switch_view(&env, Some(client(1)), default)
        .unwrap();
    assert_eq!(fixture.host.take_moved(), [], "t2 is in the default view");
    fixture.host.select(client(1), tab(1));
    fixture
        .attachments
        .switch_view(&env, Some(client(1)), empty.view_id)
        .unwrap();
    assert_eq!(fixture.host.take_moved(), [(client(1), Some(tab(2)))]);

    // Another owner's view is not found.
    let b = fixture.as_client(Some(2));
    let other = fixture.env(&b);
    fixture.host.attach(client(2), Some(principal(2)), tab(1));
    assert_eq!(
        fixture
            .attachments
            .switch_view(&other, Some(client(2)), empty.view_id),
        Err(PersonalAttachmentError::NotFound)
    );
}

#[test]
fn deleting_a_view_rebinds_its_attachments_to_the_default_and_refuses_the_default() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.host.attach(client(2), Some(principal(1)), tab(2));
    fixture.refresh();
    let default = fixture.sink.take()[0].1.view_id;
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    let work = fixture
        .attachments
        .create_view(&env, Some(client(1)), "work", true)
        .unwrap();
    fixture
        .attachments
        .switch_view(&env, Some(client(2)), work.view_id)
        .unwrap();
    // Both attachments show t2, which the work view then drops.
    fixture
        .attachments
        .edit_view(&env, Some(client(1)), tab(2), ViewEdit::Remove, |tab| {
            fixture.host.select(client(1), tab);
            Ok(())
        })
        .unwrap();
    fixture.host.take_moved();

    // The default view cannot be deleted; the rule is explicit.
    assert_eq!(
        fixture
            .attachments
            .delete_view(&env, Some(client(1)), Some(default)),
        Err(PersonalAttachmentError::InvalidReference)
    );

    // Deleting the bound view rebinds both attachments to the default view,
    // with a fresh projection each; tabs are untouched.
    assert_eq!(
        fixture.attachments.delete_view(&env, Some(client(1)), None),
        Ok(default)
    );
    for n in [1, 2] {
        assert_eq!(
            fixture.attachments.bound_view(Some(client(n))),
            Some((owner(1), default))
        );
    }
    assert_eq!(
        fixture.repo().get_view(owner(1), work.view_id),
        Err(PersonalViewError::NotFound)
    );
    assert_eq!(crate::list_contexts(&fixture.storage).unwrap().len(), 3);
    fixture.refresh();
    let published = fixture.sink.take();
    for n in [1, 2] {
        let projection = latest(&published, client(n));
        assert_eq!(projection.view_id, default);
        assert_eq!(names(&projection), ["t1", "t2", "t3"]);
    }

    // Renaming and making a view the default.
    let other = fixture
        .attachments
        .create_view(&env, None, "other", false)
        .unwrap();
    fixture
        .attachments
        .rename_view(&env, None, Some(other.view_id), "renamed")
        .unwrap();
    fixture
        .attachments
        .set_default_view(&env, None, Some(other.view_id))
        .unwrap();
    let now_default = fixture.repo().default_view(owner(1)).unwrap();
    assert_eq!(
        (now_default.view_id, now_default.name.as_str()),
        (other.view_id, "renamed")
    );
}

#[test]
fn labels_rename_a_tab_for_one_view_only_and_clear_back_to_the_shared_name() {
    let fixture = Fixture::new(2);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(2), Some(principal(2)), tab(1));
    fixture.refresh();
    fixture.sink.take();
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    fixture
        .attachments
        .set_label(&env, Some(client(1)), tab(1), Some("  mine  "))
        .unwrap();
    // Setting the label it has changes nothing.
    let before = fixture.repo().default_view(owner(1)).unwrap().revision;
    fixture
        .attachments
        .set_label(&env, Some(client(1)), tab(1), Some("mine"))
        .unwrap();
    assert_eq!(
        fixture.repo().default_view(owner(1)).unwrap().revision,
        before
    );
    fixture.refresh();
    let published = fixture.sink.take();
    let mine = latest(&published, client(1));
    assert_eq!(names(&mine), ["mine", "t2"]);
    assert_eq!(mine.workspaces[0].members[0].label.as_deref(), Some("mine"));
    assert!(
        published
            .iter()
            .all(|(recipient, _)| *recipient == client(1)),
        "another owner's view is untouched"
    );

    // A tab outside the view cannot be labelled; a blank label clears.
    fixture
        .attachments
        .edit_view(&env, Some(client(1)), tab(2), ViewEdit::Remove, |_| Ok(()))
        .unwrap();
    assert_eq!(
        fixture
            .attachments
            .set_label(&env, Some(client(1)), tab(2), Some("x")),
        Err(PersonalAttachmentError::InvalidReference)
    );
    fixture
        .attachments
        .set_label(&env, Some(client(1)), tab(1), Some(" "))
        .unwrap();
    fixture.refresh();
    let cleared = fixture.sink.only_to(client(1));
    assert_eq!(names(&cleared), ["t1"]);
    assert_eq!(cleared.workspaces[0].members[0].label, None);
}

#[test]
fn following_shows_the_leaders_tab_without_changing_the_view_and_ending_restores() {
    let fixture = Fixture::new(2);
    fixture.add_tab(tab(3), Some("elsewhere"), OTHER_WS);
    fixture.host.attach(client(1), Some(principal(1)), tab(1));
    fixture.host.attach(client(9), Some(principal(2)), tab(3));
    fixture.refresh();
    let view_id = latest(&fixture.sink.take(), client(1)).view_id;
    let a = fixture.as_client(Some(1));
    let env = fixture.env(&a);
    // The follower's view lacks t2, and has never arranged OTHER_WS.
    fixture
        .attachments
        .edit_view(&env, Some(client(1)), tab(2), ViewEdit::Remove, |_| Ok(()))
        .unwrap();
    fixture.refresh();
    fixture.sink.take();
    let arranged = fixture.view(owner(1), view_id);

    // A follow is recorded before it moves the follower's selection.
    fixture
        .host
        .follows
        .lock()
        .unwrap()
        .insert(client(1), client(9));
    fixture.host.select(client(1), tab(3));
    fixture.refresh();
    let following = fixture.sink.only_to(client(1));
    let follow = following.following.unwrap();
    assert_eq!(
        (follow.leader_client_id, follow.context_id, follow.in_view),
        (client(9), tab(3), false)
    );
    assert_eq!(follow.name, "elsewhere");
    // Shown, never arranged: the followed workspace is not seeded.
    assert_eq!(fixture.view(owner(1), view_id), arranged);

    // The leader's tab is closed for everyone while followed: the follow
    // decides what the follower shows; the view does not move it.
    fixture.host.select(client(1), tab(2));
    assert_eq!(
        fixture.attachments.reconcile_deleted(&env, tab(2)),
        Vec::<String>::new()
    );
    assert_eq!(fixture.host.take_moved(), []);

    // Ending the follow restores the follower's own selection.
    fixture.host.follows.lock().unwrap().clear();
    fixture.refresh();
    assert_eq!(fixture.host.take_moved(), [(client(1), Some(tab(1)))]);
    let restored = latest(&fixture.sink.take(), client(1));
    assert_eq!(restored.following, None);
    assert_eq!(active(&restored), [tab(1)]);
    assert_eq!(fixture.view(owner(1), view_id), arranged);
}

#[test]
fn a_follower_whose_own_tab_leaves_its_view_returns_to_the_neighbour() {
    let fixture = Fixture::new(3);
    fixture.host.attach(client(1), Some(principal(1)), tab(2));
    fixture.host.attach(client(2), Some(principal(1)), tab(2));
    fixture.host.attach(client(9), Some(principal(2)), tab(1));
    fixture.refresh();
    fixture.sink.take();
    fixture
        .host
        .follows
        .lock()
        .unwrap()
        .insert(client(1), client(9));
    fixture.host.select(client(1), tab(1));
    fixture.refresh();
    // The other attachment of the view removes the tab the follower will
    // return to: the follower is not moved now, but returns to t3 later.
    let c = fixture.as_client(Some(1));
    let env = fixture.env(&c);
    fixture
        .attachments
        .edit_view(&env, Some(client(2)), tab(2), ViewEdit::Remove, |tab| {
            fixture.host.select(client(2), tab);
            Ok(())
        })
        .unwrap();
    assert_eq!(fixture.host.take_moved(), []);
    fixture.host.follows.lock().unwrap().clear();
    fixture.refresh();
    assert_eq!(fixture.host.take_moved(), [(client(1), Some(tab(3)))]);
}
