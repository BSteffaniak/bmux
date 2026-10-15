//! Source-only lifecycle regressions: endpoints model an admitted running owner,
//! without starting a worker, connecting a runtime, or executing a playbook.
use super::*;
use std::sync::Arc;

pub(crate) struct Fixture {
    pub root: PathBuf,
    pub environment: DemoEnvironment,
}
impl Fixture {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!("presenter-reload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("deck.md"), "deck").unwrap();
        std::fs::write(root.join("demo.dsl"), "@include steps.dsl\n").unwrap();
        std::fs::write(root.join("steps.dsl"), "new-session\n").unwrap();
        let approval = Approval::prepare(&root.join("deck.md"), &deck("s", "old")).unwrap();
        let environment = DemoEnvironment {
            paths: bmux_config::ConfigPaths::new(
                root.clone(),
                root.clone(),
                root.clone(),
                root.clone(),
            ),
            approval,
            executable: std::env::current_exe().unwrap(),
            attempts_root: root.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        Self { root, environment }
    }
    pub fn running(&self) -> Running {
        let mut demos = LiveDemos::detached(&deck("s", "old"), &self.environment).unwrap();
        let entry = demos.entries.get_mut("demo").unwrap();
        entry.open = true;
        entry.snapshot.attempt = Some(bmux_playbook_plugin_api::playbook_types::Attempt {
            generation: 41,
            revision: 3,
            phase: "running".into(),
            step: 2,
            playback_speed: "normal".into(),
            current_action: "send-keys".into(),
            action_count: 4,
            typing_done: 0,
            typing_total: 0,
            viewport_cols: 80,
            viewport_rows: 24,
            paused: false,
            cleanup_acknowledged: false,
            root: String::new(),
            runtime_dir: String::new(),
            config_dir: String::new(),
            data_dir: String::new(),
            state_dir: String::new(),
            session_id: None,
            diagnostic: None,
        });
        let area = bmux_tui::geometry::Rect::new(0, 0, 2, 1);
        let prepared = bmux_tui::presentation::PresentationState::new(area)
            .prepare(bmux_tui::damage::Damage::Full, |_| {})
            .unwrap();
        let frame =
            bmux_web_renderer::BrowserFrame::from_prepared_in_generation("retained", 1, &prepared)
                .unwrap();
        let surface = bmux_tui::embedded_surface::RetainedSurface::new(
            bmux_tui::embedded_surface::SurfaceIdentity {
                owner: "demo".into(),
                generation: 41,
                revision: 1,
            },
            Arc::new(frame.buffer().clone()),
            Arc::new(frame.image_scene().clone()),
            bmux_tui::embedded_surface::SurfaceLimits {
                max_cells: 2,
                max_bytes: 4096,
            },
        )
        .unwrap();
        entry.snapshot.surface = Some(Arc::new(crate::playbook_capture::CaptureSnapshot {
            attempt_generation: 41,
            epoch: uuid::Uuid::new_v4(),
            session: uuid::Uuid::new_v4(),
            frame: Arc::new(frame),
            surface: Arc::new(surface),
        }));
        let (controller, updates, cancelled, stopped) =
            Controller::test_endpoint(entry.snapshot.clone());
        entry.controller = controller;
        Running {
            demos,
            updates,
            cancelled,
            stopped,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
pub(crate) struct Running {
    pub demos: LiveDemos,
    pub updates: tokio::sync::watch::Sender<LifecycleSnapshot>,
    pub cancelled: tokio::sync::watch::Receiver<u64>,
    pub stopped: tokio::sync::watch::Receiver<bool>,
}
pub(crate) fn deck(slide: &str, title: &str) -> Deck {
    bmux_presenter::DeckBuilder::new("reload")
        .slide(
            bmux_presenter::SlideBuilder::new(slide)
                .custom(
                    "demo",
                    "playbook-demo",
                    serde_json::json!({"playbook":"demo.dsl", "title":title}),
                )
                .build(),
        )
        .build()
        .unwrap()
}
pub(crate) fn empty() -> Deck {
    bmux_presenter::DeckBuilder::new("removed")
        .slide(bmux_presenter::SlideBuilder::new("s").build())
        .build()
        .unwrap()
}
#[tokio::test]
async fn detached_active_reload_retains_identity_surface_and_metadata_without_stop() {
    let fixture = Fixture::new();
    let mut running = fixture.running();
    let original = &running.demos.entries["demo"];
    let epoch = original.controller.owner_epoch();
    let surface = original.snapshot.surface.clone().unwrap();
    let mut next = running
        .demos
        .prepare_reload(&deck("moved", "renamed"))
        .unwrap();
    assert!(!running.demos.entries["demo"].controller.settled());
    assert_eq!(next.entries["demo"].snapshot.owner_epoch, epoch);
    assert!(Arc::ptr_eq(
        next.entries["demo"].snapshot.surface.as_ref().unwrap(),
        &surface
    ));
    running.demos.admit_reload(&next).unwrap();
    running.demos.commit_reload(&mut next);
    let retained = &next.entries["demo"];
    assert_eq!(retained.controller.owner_epoch(), epoch);
    assert_eq!(retained.snapshot.attempt.as_ref().unwrap().generation, 41);
    assert_eq!(retained.slide, "moved");
    assert_eq!(retained.config.title, "renamed");
    assert!(retained.open);
    assert_eq!(*running.cancelled.borrow(), 0);
    assert!(!*running.stopped.borrow());
    assert!(Arc::ptr_eq(
        retained.snapshot.surface.as_ref().unwrap(),
        &surface
    ));
}
#[tokio::test]
async fn changed_include_rejects_without_touching_original_or_granting_trust() {
    let fixture = Fixture::new();
    let running = fixture.running();
    let epoch = running.demos.entries["demo"].controller.owner_epoch();
    std::fs::write(
        fixture.root.join("steps.dsl"),
        "new-session\nsend-keys keys=changed\n",
    )
    .unwrap();
    assert!(running.demos.prepare_reload(&deck("s", "old")).is_err());
    assert_eq!(
        running.demos.entries["demo"].controller.owner_epoch(),
        epoch
    );
    assert_eq!(*running.cancelled.borrow(), 0);
    assert!(!*running.stopped.borrow());
}
#[tokio::test]
async fn hide_and_navigation_retain_attempt_and_pause_truth() {
    let fixture = Fixture::new();
    let mut running = fixture.running();
    let epoch = running.demos.entries["demo"].controller.owner_epoch();
    assert_eq!(
        crate::playbook_component::play_control(&running.demos.entries["demo"].snapshot),
        Control::Pause
    );
    running
        .updates
        .send_modify(|snapshot| snapshot.attempt.as_mut().unwrap().paused = true);
    running.demos.update();
    assert_eq!(
        crate::playbook_component::play_control(&running.demos.entries["demo"].snapshot),
        Control::Resume
    );
    running
        .demos
        .input_committed("demo", epoch, Some(41), "close")
        .unwrap();
    running.demos.leave("s").unwrap();
    let entry = &running.demos.entries["demo"];
    assert!(!entry.open);
    assert_eq!(entry.snapshot.attempt.as_ref().unwrap().generation, 41);
    assert_eq!(*running.cancelled.borrow(), 0);
    assert!(!*running.stopped.borrow());
}

#[tokio::test]
async fn explicit_leave_stop_policy_requests_authoritative_cancellation() {
    let fixture = Fixture::new();
    let mut running = fixture.running();
    running
        .demos
        .entries
        .get_mut("demo")
        .unwrap()
        .config
        .stop_on_leave = true;
    running.demos.leave("s").unwrap();
    assert!(!running.demos.entries["demo"].open);
    assert!(*running.cancelled.borrow() > 0);
}

#[tokio::test]
async fn discarded_candidate_and_removed_reference_retain_execution_owner() {
    let fixture = Fixture::new();
    let mut running = fixture.running();
    drop(
        running
            .demos
            .prepare_reload(&deck("s", "title only"))
            .unwrap(),
    );
    assert!(!*running.stopped.borrow());
    assert_eq!(*running.cancelled.borrow(), 0);
    let mut next = running.demos.prepare_reload(&empty()).unwrap();
    running.demos.admit_reload(&next).unwrap();
    assert_eq!(*running.cancelled.borrow(), 0);
    running.demos.commit_reload(&mut next);
    assert!(next.entries.is_empty());
    drop(running.demos);
    assert!(!*running.stopped.borrow());
    assert_eq!(next.retained.len(), 1);
    assert_eq!(
        next.retained[0]
            .1
            .snapshot
            .attempt
            .as_ref()
            .unwrap()
            .generation,
        41
    );
}
