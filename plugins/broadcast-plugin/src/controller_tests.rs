use super::*;

#[test]
fn observer_failure_survives_unregister_without_affecting_other_sources() {
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    let other = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    controller.source(owner, &other, true).unwrap();
    {
        let mut state = controller.state.lock().unwrap();
        state.selected = Some(id.clone());
        state.local = "running";
    }
    controller
        .unregister_with_error(owner, &other, Some("observer_closed"))
        .unwrap();
    assert_eq!(controller.status().unwrap().local, "running");
    assert_eq!(controller.status().unwrap().error_code, None);
    controller
        .unregister_with_error(owner, &id, Some("observer_timeout"))
        .unwrap();
    let status = controller.status().unwrap();
    assert_eq!(status.local, "unavailable");
    assert_eq!(status.error_code.as_deref(), Some("observer_timeout"));
    assert!(controller.sources().unwrap().is_empty());
    controller.shutdown();
}

#[test]
fn stopped_observer_does_not_overwrite_stopped_status() {
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    controller.state.lock().unwrap().selected = Some(id.clone());
    controller
        .unregister_with_error(owner, &id, Some("observer_closed"))
        .unwrap();
    assert_eq!(controller.status().unwrap().local, "stopped");
    assert_eq!(controller.status().unwrap().error_code, None);
    controller.shutdown();
}

pub(super) fn frame(generation: &str, revision: u64) -> Vec<u8> {
    let scene =
        bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(0, 0, 2, 1));
    let prepared = scene
        .prepare(bmux_tui::damage::Damage::Full, |_| {})
        .unwrap();
    let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
        generation, revision, &prepared,
    )
    .unwrap();
    bmux_web_renderer::encode_binary(&frame).unwrap()
}

#[test]
fn semantic_duplicates_ignore_frame_revision_but_preserve_refresh_and_generation() {
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    {
        let mut state = controller.state.lock().unwrap();
        state.selected = Some(id.clone());
        state.local = "running";
    }
    let refresh = controller.source(owner, &id, false).unwrap().refresh;
    assert!(
        controller
            .publish(owner, &id, refresh, frame("scene", 1))
            .unwrap()
    );
    assert!(
        controller
            .publish(owner, &id, refresh, frame("scene", 2))
            .unwrap()
    );
    // Cross an actual delivery deadline: without a destination, the worker must
    // not dequeue and lose the sole repair. Duplicate suppression is independent
    // of how quickly the test thread inspects pending state.
    std::thread::sleep(Duration::from_secs_f64(2.0 / 60.0));
    {
        let state = controller.state.lock().unwrap();
        assert_eq!(state.suppressed, 1);
        assert_eq!(state.coalesced, 0);
        let retained =
            bmux_web_renderer::decode_binary(&state.pending.as_ref().unwrap().bytes, None).unwrap();
        assert_eq!(retained.revision(), 1);
    }
    {
        let mut state = controller.state.lock().unwrap();
        state.invalidate();
    }
    let refreshed = controller.source(owner, &id, false).unwrap().refresh;
    assert!(
        controller
            .publish(owner, &id, refreshed, frame("scene", 2))
            .unwrap()
    );
    assert!(
        !controller
            .publish(owner, &id, refreshed, frame("replacement", 1))
            .unwrap()
    );
    let replaced = controller.source(owner, &id, false).unwrap().refresh;
    assert!(replaced > refreshed);
    assert!(
        controller
            .publish(owner, &id, replaced, frame("replacement", 1))
            .unwrap()
    );
    assert_eq!(controller.delivery_counters().unwrap().suppressed, 1);
    controller.shutdown();
    assert_eq!(controller.delivery_counters().unwrap().suppressed, 1);
}

#[test]
fn content_key_preserves_interaction_and_generation_but_ignores_frame_revision() {
    let bytes = frame("scene", 1);
    let frame = bmux_web_renderer::decode_binary(&bytes, None).unwrap();
    let key = content_key(&frame).unwrap();
    let mut json = serde_json::to_value(&frame).unwrap();
    json["revision"] = serde_json::json!(2);
    let newer: bmux_web_renderer::BrowserFrame = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(key, content_key(&newer).unwrap());
    json["generation"] = serde_json::json!("replacement");
    let replacement: bmux_web_renderer::BrowserFrame = serde_json::from_value(json).unwrap();
    assert_ne!(key, content_key(&replacement).unwrap());
    let mut json = serde_json::to_value(&frame).unwrap();
    json["selection_text_available"] =
        serde_json::json!(!json["selection_text_available"].as_bool().unwrap());
    let changed: bmux_web_renderer::BrowserFrame = serde_json::from_value(json).unwrap();
    assert_ne!(key, content_key(&changed).unwrap());
}

#[test]
fn websocket_lifecycle_refresh_fencing_and_shutdown() {
    let mut server = super::recovery_tests::TestRelay::new(false);
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    let relay = server.relay.take().unwrap();
    assert_eq!(
        controller
            .control(Control::Start(Box::new(relay), id.clone()))
            .unwrap()
            .local,
        "running"
    );
    server.event("start");
    let refresh = controller.source(owner, &id, false).unwrap().refresh;
    assert!(
        controller
            .publish(owner, &id, refresh, frame("scene", 1))
            .unwrap()
    );
    server.event("frame");
    assert_eq!(controller.control(Control::Pause).unwrap().local, "paused");
    server.event("pause");
    assert!(
        !controller
            .publish(owner, &id, refresh, frame("scene", 2))
            .unwrap()
    );
    assert_eq!(
        controller.control(Control::Resume).unwrap().local,
        "running"
    );
    server.event("pause");
    server.event("resume");
    let fresh = controller.source(owner, &id, false).unwrap().refresh;
    assert!(fresh > refresh);
    assert!(
        controller
            .publish(owner, &id, fresh, frame("scene", 1))
            .unwrap()
    );
    server.event("frame");
    assert!(
        controller
            .publish(Uuid::new_v4(), &id, fresh, frame("scene", 2))
            .is_err()
    );
    assert!(
        !controller
            .publish(owner, &id, fresh, frame("new-scene", 1))
            .unwrap()
    );
    let retarget = controller.source(owner, &id, false).unwrap().refresh;
    assert!(retarget > fresh);
    assert!(
        controller
            .publish(owner, &id, retarget, frame("new-scene", 1))
            .unwrap()
    );
    server.event("frame");
    controller.shutdown();
    server.event("stop");
    assert_eq!(controller.status().unwrap().local, "stopped");
}

#[test]
fn lease_expiry_and_unregister_never_revive_old_authority() {
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    {
        let mut state = controller.state.lock().unwrap();
        state.local = "running";
        state.selected = Some(id.clone());
        state.sources.get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    assert!(controller.source(owner, &id, false).is_err());
    assert!(
        !controller
            .publish(owner, &id, 1, frame("scene", 1))
            .unwrap()
    );
    controller.unregister(owner, &id).unwrap();
    assert_eq!(controller.status().unwrap().local, "unavailable");
    assert!(controller.source(owner, &id, false).is_err());
    controller.shutdown();
}

#[test]
fn expired_uuid_rebinds_but_unauthorized_attempts_are_atomic() {
    let controller = Controller::new().unwrap();
    // This tests synchronous authority transitions, not frame delivery. Stop
    // the worker before staging pending data so it cannot consume the fixture.
    controller.shutdown();
    let owner = Uuid::new_v4();
    let hostile = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    {
        let mut state = controller.state.lock().unwrap();
        state.selected = Some(id.clone());
        state.local = "running";
        state.pending = Some(Pending {
            refresh: state.refresh,
            bytes: frame("scene", 1),
            relay_revision: None,
        });
    }
    let before = controller.source(owner, &id, false).unwrap().refresh;
    assert!(controller.source(hostile, &id, true).is_err());
    {
        let state = controller.state.lock().unwrap();
        assert_eq!(state.refresh, before);
        assert_eq!(state.local, "running");
        assert!(state.pending.is_some());
    }
    {
        let mut state = controller.state.lock().unwrap();
        state.sources.get_mut(&id).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    let replacement = controller.source(hostile, &id, true).unwrap();
    assert!(replacement.refresh > before);
    assert!(!replacement.selected);
    assert!(controller.source(owner, &id, false).is_err());
    assert!(controller.unregister(owner, &id).is_err());
    controller.shutdown();
}
