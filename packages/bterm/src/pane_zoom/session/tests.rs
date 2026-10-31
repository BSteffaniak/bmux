use super::*;

fn receipt(status: RequestStatus) -> RequestReceipt {
    Box::new(move || status)
}

fn request(binding: PaneViewBinding, target: u32, action: ZoomAction) -> PaneZoomRequest<u32> {
    PaneZoomRequest {
        binding,
        target,
        current_scale: 1.0,
        restore: None,
        action,
        base_size: 14.0,
    }
}

fn session() -> PaneZoomSession<u32> {
    let mut policy = ZoomConfiguration::default();
    policy.animation.duration_ms = 100;
    PaneZoomSession::new(policy, None).unwrap()
}

fn stored_session(root: &std::path::Path) -> (PreferenceStore, PaneZoomSession<u32>) {
    use crate::{
        configuration::{ConfigOptions, SourcePolicy},
        runtime_paths::BtermPaths,
    };
    let configuration = ConfigOptions {
        policy: SourcePolicy::Explicit,
        ..Default::default()
    }
    .capture_selected(
        BtermPaths::from_root(root.to_owned()).unwrap(),
        root.to_owned(),
    )
    .unwrap()
    .resolve()
    .unwrap();
    let store = PreferenceStore::for_configuration(&configuration, "test-runtime").unwrap();
    let state = PaneZoomSession::new(ZoomConfiguration::default(), Some(store.clone())).unwrap();
    (store, state)
}

fn wait_restore(session: &mut PaneZoomSession<u32>, current: Option<u32>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.loading.is_some() {
        session
            .poll_restore(|previous| {
                current
                    .filter(|value| value == previous)
                    .map_or(TargetRefresh::Gone, |target| TargetRefresh::Ready {
                        target,

                        scale: 1.0,
                    })
            })
            .unwrap();
        assert!(Instant::now() < deadline, "restore IO timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn real_restore_loads_matching_namespace_and_pane_then_revalidates_source() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut state) = stored_session(root.path());
    let token = PaneViewRestoreToken::for_committed_pane("named-view", "committed-pane").unwrap();
    store
        .save_pane(&token, ZoomPreferences { target_scale: 2.0 })
        .unwrap();
    let live = PaneViewBinding::new("attach/view", "committed-pane/execution-a").unwrap();
    state.observe(live.clone(), 11, 1.0, token.clone()).unwrap();
    wait_restore(&mut state, Some(11));
    assert_eq!(state.state.target_scale(&live), Some(2.0));
    let mut sent = Vec::new();
    state
        .tick(Instant::now(), |target, scale| {
            sent.push((*target, scale));
            Ok(true)
        })
        .unwrap();
    assert_eq!(sent, vec![(11, 2.0)]);
    // Explicit duplicate names reuse preferences, but live state is not shared.
    let (_, mut duplicate) = stored_session(root.path());
    duplicate
        .observe(live.clone(), 22, 1.0, token.clone())
        .unwrap();
    wait_restore(&mut duplicate, Some(22));
    assert_eq!(duplicate.state.target_scale(&live), Some(2.0));
    state
        .state
        .apply(&live, ZoomAction::SetScale(1.5), 14.0, Instant::now())
        .unwrap();
    assert_eq!(duplicate.state.target_scale(&live), Some(2.0));
    // New execution incarnation may retain presentation preferences only after
    // fresh live-source validation; old source anchors are never restored.
    let (_, mut restarted) = stored_session(root.path());
    let new_live = PaneViewBinding::new("new-attach/view", "committed-pane/execution-b").unwrap();
    restarted
        .observe(new_live.clone(), 44, 1.0, token.clone())
        .unwrap();
    wait_restore(&mut restarted, Some(44));
    assert_eq!(restarted.state.target_scale(&new_live), Some(2.0));
    // Replaced execution while reading: label and stored pane are insufficient.
    let (_, mut stale) = stored_session(root.path());
    stale.observe(live.clone(), 11, 1.0, token).unwrap();
    wait_restore(&mut stale, Some(33));
    assert_eq!(stale.state.target_scale(&live), None);
}

#[test]
fn restore_mismatch_and_late_io_do_not_replace_user_intent() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut state) = stored_session(root.path());
    let saved = PaneViewRestoreToken::for_committed_pane("named-view", "pane-a").unwrap();
    store
        .save_pane(&saved, ZoomPreferences { target_scale: 2.0 })
        .unwrap();
    let live = PaneViewBinding::new("attach/view", "pane-b/execution").unwrap();
    let other = PaneViewRestoreToken::for_committed_pane("named-view", "pane-b").unwrap();
    state.observe(live.clone(), 11, 1.0, other).unwrap();
    wait_restore(&mut state, Some(11));
    assert_eq!(state.state.target_scale(&live), Some(1.0));
    let (_, mut other_namespace) = stored_session(root.path());
    let unrelated = PaneViewRestoreToken::for_committed_pane("other-name", "pane-a").unwrap();
    other_namespace
        .observe(live.clone(), 11, 1.0, unrelated)
        .unwrap();
    wait_restore(&mut other_namespace, Some(11));
    assert_eq!(other_namespace.state.target_scale(&live), Some(1.0));
    let (_, mut late) = stored_session(root.path());
    let binding = PaneViewBinding::new("attach/view", "pane-a/execution").unwrap();
    late.observe(binding.clone(), 11, 1.0, saved.clone())
        .unwrap();
    late.action(
        PaneZoomRequest {
            binding: binding.clone(),
            target: 11,
            current_scale: 1.0,
            restore: Some(saved),
            action: ZoomAction::SetScale(1.25),
            base_size: 14.0,
        },
        Instant::now(),
    )
    .unwrap();
    wait_restore(&mut late, Some(11));
    assert_eq!(late.state.target_scale(&binding), Some(1.25));
}

#[test]
fn unnamed_window_has_no_restore_io_or_pane_writes() {
    let mut state = session();
    let binding = PaneViewBinding::new("ephemeral/view", "pane/source").unwrap();
    let token = PaneViewRestoreToken::for_committed_pane("not-enabled", "pane").unwrap();
    // No preference store is offered to an ephemeral standalone session.
    state.observe(binding.clone(), 11, 1.0, token).unwrap();
    assert!(state.loading.is_none());
    state
        .action(
            request(binding, 11, ZoomAction::SetScale(1.5)),
            Instant::now(),
        )
        .unwrap();
    assert!(state.pending.is_empty());
}

#[test]
fn remember_disabled_skips_real_restore_and_drops_pending_saves() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = stored_session(root.path());
    let token = PaneViewRestoreToken::for_committed_pane("explicit-view", "pane").unwrap();
    store
        .save_pane(&token, ZoomPreferences { target_scale: 2.0 })
        .unwrap();
    let policy = ZoomConfiguration {
        remember: false,
        ..ZoomConfiguration::default()
    };
    let mut state = PaneZoomSession::new(policy, Some(store)).unwrap();
    let binding = PaneViewBinding::new("attach/view", "pane/execution").unwrap();
    state
        .observe(binding.clone(), 11, 1.0, token.clone())
        .unwrap();
    assert!(state.loading.is_none());
    state
        .action(
            PaneZoomRequest {
                binding,
                target: 11,
                current_scale: 1.0,
                restore: Some(token),
                action: ZoomAction::SetScale(1.75),
                base_size: 14.0,
            },
            Instant::now(),
        )
        .unwrap();
    assert!(state.pending.is_empty());
}

#[test]
fn real_action_save_is_restored_by_another_named_view_session() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut state) = stored_session(root.path());
    let now = Instant::now();
    let token = PaneViewRestoreToken::for_committed_pane("explicit-view", "pane").unwrap();
    let live = PaneViewBinding::new("attach-a/view", "pane/execution").unwrap();
    state
        .action(
            PaneZoomRequest {
                binding: live,
                target: 11,
                current_scale: 1.0,
                restore: Some(token.clone()),
                action: ZoomAction::SetScale(1.75),
                base_size: 14.0,
            },
            now,
        )
        .unwrap();
    state.flush();
    assert_eq!(store.load_pane(&token).unwrap().unwrap().target_scale, 1.75);
    let (_, mut reopened) = stored_session(root.path());
    let new_live = PaneViewBinding::new("attach-b/view", "pane/execution").unwrap();
    reopened.observe(new_live.clone(), 22, 1.0, token).unwrap();
    wait_restore(&mut reopened, Some(22));
    assert_eq!(reopened.state.target_scale(&new_live), Some(1.75));
}

#[test]
fn bound_animation_coalesces_until_commit_then_uses_refreshed_token() {
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    let now = Instant::now();
    state
        .action(request(binding.clone(), 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let mid = now + Duration::from_millis(50);
    let mut sent = Vec::new();
    state
        .tick_bound(
            mid,
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 1.0,
            },
            |token, scale| {
                sent.push((*token, scale));
                Ok(receipt(RequestStatus::Applied))
            },
        )
        .unwrap();
    assert_eq!(sent.len(), 1);
    let first_scale = sent[0].1;
    assert_eq!(sent[0].0, 12);
    // A new action while an exact request is in-flight changes intent, not the queue.
    state
        .action(request(binding, 12, ZoomAction::SetScale(0.75)), mid)
        .unwrap();
    state
        .tick_bound(
            mid + Duration::from_millis(100),
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 1.0,
            },
            |_, _| panic!("must coalesce until sent sample is committed"),
        )
        .unwrap();
    assert!(state.needs_tick(mid + Duration::from_millis(100)));
    state
        .tick_bound(
            mid + Duration::from_millis(100),
            |_| TargetRefresh::Ready {
                target: 13,
                scale: first_scale,
            },
            |token, scale| {
                sent.push((*token, scale));
                Ok(receipt(RequestStatus::Applied))
            },
        )
        .unwrap();
    assert_eq!(sent[1], (13, 0.75));
    state
        .tick_bound(
            mid + Duration::from_millis(100),
            |_| TargetRefresh::Ready {
                target: 14,
                scale: 0.75,
            },
            |_, _| panic!("matching committed scale must not be resent"),
        )
        .unwrap();
    assert!(!state.needs_tick(mid + Duration::from_millis(100)));
}

#[test]
fn bound_reload_clamps_latest_intent_without_superseding_pending_output() {
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    let now = Instant::now();
    state
        .action(request(binding.clone(), 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let done = now + Duration::from_millis(100);
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 1.0,
            },
            |_, _| Ok(receipt(RequestStatus::Applied)),
        )
        .unwrap();
    let mut policy = ZoomConfiguration {
        max_scale: 1.5,
        ..ZoomConfiguration::default()
    };
    policy.animation.enabled = false;
    state.reconfigure(policy, done).unwrap();
    assert_eq!(state.state.target_scale(&binding), Some(1.5));
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 1.0,
            },
            |_, _| panic!("reload must not supersede in-flight output"),
        )
        .unwrap();
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 13,
                scale: 2.0,
            },
            |token, scale| {
                assert_eq!((*token, scale), (13, 1.5));
                Ok(receipt(RequestStatus::Applied))
            },
        )
        .unwrap();
}

#[test]
fn bound_source_loss_clears_state_and_pending_action_without_retargeting() {
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    let now = Instant::now();
    state
        .action(request(binding.clone(), 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    state
        .tick_bound(
            now,
            |_| TargetRefresh::Pending,
            |_, _| panic!("pending commit is not output authority"),
        )
        .unwrap();
    assert!(state.active.contains_key(&binding));
    state
        .tick_bound(
            now,
            |_| TargetRefresh::Gone,
            |_, _| panic!("invalid binding must never publish"),
        )
        .unwrap();
    assert!(!state.active.contains_key(&binding));
    assert_eq!(state.state.target_scale(&binding), None);
    assert!(!state.pending.contains_key(&binding));
}

#[test]
fn bound_final_sample_survives_queue_backpressure() {
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    let now = Instant::now();
    state
        .action(request(binding, 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let done = now + Duration::from_millis(100);
    assert!(
        state
            .tick_bound(
                done,
                |_| TargetRefresh::Ready {
                    target: 12,
                    scale: 1.0
                },
                |_, _| Err(anyhow::anyhow!("full"))
            )
            .is_err()
    );
    assert!(state.needs_tick(done));
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 13,
                scale: 1.0,
            },
            |token, scale| {
                assert_eq!((*token, scale), (13, 2.0));
                Ok(receipt(RequestStatus::Applied))
            },
        )
        .unwrap();
}

#[test]
fn animation_stays_bound_when_focus_target_changes() {
    let now = Instant::now();
    let mut session = session();
    let first = PaneViewBinding::new("view", "pane-a/source-a").unwrap();
    let second = PaneViewBinding::new("view", "pane-b/source-b").unwrap();
    session
        .action(request(first, 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    session
        .action(request(second, 22, ZoomAction::SetScale(0.75)), now)
        .unwrap();
    let mut sent = Vec::new();
    session
        .tick(now + Duration::from_millis(100), |target, scale| {
            sent.push((*target, scale));
            Ok(true)
        })
        .unwrap();
    assert_eq!(sent, vec![(11, 2.0), (22, 0.75)]);
    assert!(!session.animating(now + Duration::from_millis(100)));
}

#[test]
fn stale_target_is_retired_without_retargeting_and_release_does_not_bind() {
    let now = Instant::now();
    let mut session = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    session
        .action(request(binding.clone(), 11, ZoomAction::Consume), now)
        .unwrap();
    assert!(session.active.is_empty());
    session
        .action(request(binding, 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    session
        .tick(now + Duration::from_millis(50), |target, _| {
            assert_eq!(*target, 11);
            Ok(false)
        })
        .unwrap();
    assert!(session.active.is_empty());
    assert!(!session.animating(now + Duration::from_millis(50)));
}

#[test]
fn raster_cap_bounds_effective_scale_without_destroying_requested_intent() {
    let now = Instant::now();
    let mut session = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    session.set_raster_cap(1500, now).unwrap();
    session
        .action(request(binding.clone(), 11, ZoomAction::SetScale(4.0)), now)
        .unwrap();
    assert_eq!(session.state.target_scale(&binding), Some(1.5));
    session
        .reconfigure(ZoomConfiguration::default(), now)
        .unwrap();
    session
        .action(request(binding.clone(), 11, ZoomAction::SetScale(8.0)), now)
        .unwrap();
    assert_eq!(session.state.target_scale(&binding), Some(1.5));
    session.set_raster_cap(1250, now).unwrap();
    assert_eq!(session.state.target_scale(&binding), Some(1.25));
    assert_eq!(session.requested.target_scale(&binding), Some(3.0));
    session.set_raster_cap(8000, now).unwrap();
    assert_eq!(session.state.target_scale(&binding), Some(3.0));
}

#[test]
fn geometry_refresh_preserves_clamped_requested_intent_without_persistence() {
    let now = Instant::now();
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    state.set_raster_cap(1250, now).unwrap();
    state
        .action(request(binding.clone(), 11, ZoomAction::SetScale(2.5)), now)
        .unwrap();
    state
        .tick_bound(
            now,
            |_| TargetRefresh::Pending,
            |_, _| panic!("pending geometry must not send"),
        )
        .unwrap();
    state
        .tick_bound(
            now + Duration::from_millis(101),
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 1.0,
            },
            |target, _| {
                assert_eq!(*target, 12);
                Ok(Box::new(|| RequestStatus::Pending))
            },
        )
        .unwrap();
    assert_eq!(state.requested.target_scale(&binding), Some(2.5));
    assert_eq!(state.state.target_scale(&binding), Some(1.25));
    state
        .set_raster_cap(8000, now + Duration::from_millis(102))
        .unwrap();
    assert_eq!(state.state.target_scale(&binding), Some(2.5));
}

#[test]
fn pending_receipt_ignores_matching_output_and_applied_waits_for_its_sample() {
    use std::{cell::Cell, rc::Rc};
    let now = Instant::now();
    let done = now + Duration::from_millis(101);
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    state
        .action(request(binding.clone(), 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let status = Rc::new(Cell::new(RequestStatus::Pending));
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 11,
                scale: 1.0,
            },
            |_, _| {
                let status = Rc::clone(&status);
                Ok(Box::new(move || status.get()))
            },
        )
        .unwrap();
    state
        .action(request(binding, 11, ZoomAction::SetScale(0.75)), done)
        .unwrap();
    // Matching output alone cannot acknowledge a pending request.
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 12,
                scale: 2.0,
            },
            |_, _| panic!("receipt pending"),
        )
        .unwrap();
    status.set(RequestStatus::Applied);
    let later = done + Duration::from_millis(101);
    // Reducer Applied plus unrelated/new desired scale is not presentation.
    state
        .tick_bound(
            later,
            |_| TargetRefresh::Ready {
                target: 13,
                scale: 0.75,
            },
            |_, _| panic!("sent sample not committed"),
        )
        .unwrap();
    let mut sent = false;
    state
        .tick_bound(
            later,
            |_| TargetRefresh::Ready {
                target: 14,
                scale: 2.0,
            },
            |target, scale| {
                assert_eq!((*target, scale), (14, 0.75));
                sent = true;
                Ok(receipt(RequestStatus::Applied))
            },
        )
        .unwrap();
    assert!(sent);
    state
        .tick_bound(
            later,
            |_| TargetRefresh::Ready {
                target: 15,
                scale: 0.75,
            },
            |_, _| panic!("settled"),
        )
        .unwrap();
    assert!(!state.needs_tick(later));
}

#[test]
fn settled_bound_session_does_not_request_or_redraw() {
    let now = Instant::now();
    let mut state = session();
    let binding = PaneViewBinding::new("view", "pane").unwrap();
    state
        .action(request(binding, 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let done = now + Duration::from_millis(101);
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 11,
                scale: 1.0,
            },
            |_, _| Ok(receipt(RequestStatus::Applied)),
        )
        .unwrap();
    state
        .tick_bound(
            done,
            |_| TargetRefresh::Ready {
                target: 11,
                scale: 2.0,
            },
            |_, _| panic!("already committed"),
        )
        .unwrap();
    assert!(!state.needs_tick(done));
    state
        .tick_bound(
            done + MAX_WAIT,
            |_| TargetRefresh::Ready {
                target: 11,
                scale: 2.0,
            },
            |_, _| panic!("unrelated output"),
        )
        .unwrap();
    assert!(!state.needs_tick(done + MAX_WAIT));
}

#[test]
fn stalled_pending_and_in_flight_stop_polling_without_retiring_intent() {
    for pending in [true, false] {
        let now = Instant::now();
        let mut state = session();
        let binding = PaneViewBinding::new("view", "pane").unwrap();
        state
            .action(request(binding.clone(), 11, ZoomAction::SetScale(2.0)), now)
            .unwrap();
        let done = now + Duration::from_millis(101);
        state
            .tick_bound(
                done,
                |_| {
                    if pending {
                        TargetRefresh::Pending
                    } else {
                        TargetRefresh::Ready {
                            target: 11,
                            scale: 1.0,
                        }
                    }
                },
                |_, _| Ok(receipt(RequestStatus::Applied)),
            )
            .unwrap();
        assert!(!state.needs_tick(done + MAX_WAIT));
        assert_eq!(state.requested.target_scale(&binding), Some(2.0));
        state
            .action(
                request(binding, 11, ZoomAction::SetScale(0.75)),
                done + MAX_WAIT,
            )
            .unwrap();
        assert!(state.needs_tick(done + MAX_WAIT));
    }
}

#[test]
fn queue_and_dequeue_failures_have_a_finite_retry_budget() {
    for queue_failure in [true, false] {
        let now = Instant::now();
        let mut state = session();
        let binding = PaneViewBinding::new("view", "pane").unwrap();
        state
            .action(request(binding, 11, ZoomAction::SetScale(2.0)), now)
            .unwrap();
        let done = now + Duration::from_millis(101);
        let mut sends = 0;
        for revision in 1..=20 {
            let result = state.tick_bound(
                done,
                |_| TargetRefresh::Ready {
                    target: 11,
                    scale: 1.0,
                },
                |_, _| {
                    sends += 1;
                    if queue_failure {
                        Err(anyhow::anyhow!("full"))
                    } else {
                        Ok(receipt(RequestStatus::Rejected))
                    }
                },
            );
            assert_eq!(
                result.is_err(),
                queue_failure && revision <= u64::from(MAX_RETRIES)
            );
        }
        assert_eq!(sends, MAX_RETRIES);
        assert!(!state.needs_tick(done));
    }
}

#[test]
fn saved_requested_scale_survives_dpi_cap_and_independent_named_views() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut first) = stored_session(root.path());
    let token = PaneViewRestoreToken::for_committed_pane("same-name", "pane").unwrap();
    let binding = PaneViewBinding::new("first-instance", "pane/source").unwrap();
    let now = Instant::now();
    let mut action = request(binding.clone(), 11, ZoomAction::SetScale(2.5));
    action.restore = Some(token.clone());
    first.action(action, now).unwrap();
    first.set_raster_cap(1250, now).unwrap();
    first.flush();
    assert_eq!(store.load_pane(&token).unwrap().unwrap().target_scale, 2.5);
    let (_, mut second) = stored_session(root.path());
    second.set_raster_cap(1500, now).unwrap();
    let other = PaneViewBinding::new("second-instance", "pane/source").unwrap();
    second.observe(other.clone(), 22, 1.0, token).unwrap();
    wait_restore(&mut second, Some(22));
    assert_eq!(second.state.target_scale(&other), Some(1.5));
    assert_eq!(second.requested.target_scale(&other), Some(2.5));
    second.set_raster_cap(8000, now).unwrap();
    assert_eq!(second.state.target_scale(&other), Some(2.5));
    assert_eq!(first.state.target_scale(&binding), Some(1.25));
    assert_eq!(first.requested.target_scale(&binding), Some(2.5));
}

#[test]
fn invalidated_source_drops_queued_preferences_before_io_admission() {
    let root = tempfile::tempdir().unwrap();
    let (store, mut state) = stored_session(root.path());
    let token = PaneViewRestoreToken::for_committed_pane("named", "pane").unwrap();
    let binding = PaneViewBinding::new("instance", "pane/source-a").unwrap();
    let now = Instant::now();
    let mut action = request(binding.clone(), 11, ZoomAction::SetScale(2.0));
    action.restore = Some(token.clone());
    state.action(action, now).unwrap();
    state
        .tick_bound(
            now + Duration::from_secs(1),
            |_| TargetRefresh::Gone,
            |_, _| panic!("stale source"),
        )
        .unwrap();
    assert!(state.pending.is_empty());
    assert!(state.active.is_empty());
    assert_eq!(state.requested.target_scale(&binding), None);
    assert!(store.load_pane(&token).unwrap().is_none());
}

#[test]
fn asynchronous_io_timeouts_stop_idle_polling() {
    let root = tempfile::tempdir().unwrap();
    let (_, mut state) = stored_session(root.path());
    let token = PaneViewRestoreToken::for_committed_pane("named", "pane").unwrap();
    let binding = PaneViewBinding::new("instance", "pane/source").unwrap();
    let (_sender, receiver) = mpsc::channel();
    state.loading = Some(RestoreLoad {
        binding,
        target: 11,
        token,
        current_scale: 1.0,
        started: Instant::now() - MAX_WAIT,
        receiver,
    });
    state.poll_restore(|_| TargetRefresh::Pending).unwrap();
    assert!(!state.save_pending());
    let (_sender, receiver) = mpsc::channel();
    let now = Instant::now();
    state.saving = Some(receiver);
    state.saving_started = Some(now - MAX_WAIT);
    state.poll_save(now);
    assert!(!state.save_pending());
    assert_eq!(state.save_spawn_failures, MAX_RETRIES);
}

#[test]
fn failed_queue_admission_does_not_mark_sample_sent() {
    let now = Instant::now();
    let mut session = session();
    let binding = PaneViewBinding::new("view", "pane/source").unwrap();
    session
        .action(request(binding, 11, ZoomAction::SetScale(2.0)), now)
        .unwrap();
    let sample_time = now + Duration::from_millis(100);
    assert!(
        session
            .tick(sample_time, |_, _| Err(anyhow::anyhow!("full")))
            .is_err()
    );
    assert!(session.needs_tick(sample_time));
    let mut sent = false;
    session
        .tick(sample_time, |target, scale| {
            assert_eq!(*target, 11);
            assert_eq!(scale, 2.0);
            sent = true;
            Ok(true)
        })
        .unwrap();
    assert!(sent);
    session.clear();
    assert!(session.active.is_empty());
}
