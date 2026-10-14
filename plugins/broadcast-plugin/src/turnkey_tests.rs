use super::*;
#[cfg(unix)]
#[test]
fn owned_retry_wrong_token_and_manual_switch_fencing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    std::fs::write(&key, "x".repeat(32)).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let controller = Controller::new().unwrap();
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let id = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    controller.source(owner, &id, true).unwrap();
    controller.source(owner, &second, true).unwrap();
    let relay = || Relay::new("http://127.0.0.1:1", "main", &key).unwrap();
    let first = controller
        .start_owned(
            owner,
            &id,
            None,
            relay(),
            "http://127.0.0.1:1/live".into(),
            dir.path(),
        )
        .unwrap();
    assert_eq!(
        controller
            .start_owned(
                owner,
                &id,
                None,
                relay(),
                "http://127.0.0.1:1/live".into(),
                dir.path()
            )
            .unwrap()
            .token,
        first.token
    );
    assert!(!controller.stop_owned(other, &first.token).unwrap());
    assert!(
        !controller
            .stop_owned(owner, &Uuid::new_v4().to_string())
            .unwrap()
    );
    let before = controller.status().unwrap();
    assert_eq!(before.source_id.as_deref(), Some(id.as_str()));
    assert!(
        controller
            .start_owned(
                owner,
                &id,
                Some("changed".into()),
                relay(),
                "url".into(),
                dir.path()
            )
            .is_err()
    );
    controller.control(Control::Source(second.clone())).unwrap();
    assert!(!controller.stop_owned(owner, &first.token).unwrap());
    assert_eq!(
        controller.status().unwrap().source_id.as_deref(),
        Some(second.as_str())
    );
    controller.control(Control::Stop).unwrap();
    assert!(
        controller
            .start_owned(owner, &id, None, relay(), "url".into(), dir.path())
            .is_err()
    );
    controller.shutdown();
}
#[test]
fn labels_reject_controls_and_unowned_registration() {
    let controller = Controller::new().unwrap();
    let id = Uuid::new_v4().to_string();
    let owner = Uuid::new_v4();
    assert!(
        controller
            .register_label(owner, &id, "path\nsecret")
            .is_err()
    );
    controller.register_label(owner, &id, "Terminal").unwrap();
    assert!(
        controller
            .register_label(Uuid::new_v4(), &id, "Terminal")
            .is_err()
    );
    controller.shutdown();
}
