use super::*;

fn connect(endpoint: &str, accept_restore: bool) -> BufReader<UnixStream> {
    let stream = UnixStream::connect(endpoint.strip_prefix("v1:").unwrap()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    send(reader.get_mut(), &serde_json::json!({"protocol":PROTOCOL,"application":"test","pid":std::process::id(),"accept_restore":accept_restore})).unwrap();
    reader
}

#[test]
fn socket_persists_restores_and_manual_launch_does_not_resume() {
    let directory = tempfile::tempdir().unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let endpoint = host.prepare(&key, false, false, false).unwrap();
    host.bind(&key, &endpoint, std::process::id()).unwrap();
    let mut reader = connect(&endpoint, true);
    let welcome: serde_json::Value = receive(&mut reader).unwrap();
    assert!(welcome["restore"].is_null());
    let descriptor = serde_json::json!({"application":"test","session":"first"});
    send(
        reader.get_mut(),
        &serde_json::json!({"operation":"selection","revision":1,"descriptor":descriptor}),
    )
    .unwrap();
    let ack: serde_json::Value = receive(&mut reader).unwrap();
    assert_eq!(ack["revision"], 1);
    let checkpoint: Checkpoint = serde_json::from_slice(
        &std::fs::read(directory.path().join(format!("{key}.json"))).unwrap(),
    )
    .unwrap();
    assert!(checkpoint.descriptor.is_some());
    host.close(&key, &endpoint).unwrap();
    let restored = host.prepare(&key, true, false, false).unwrap();
    host.bind(&key, &restored, std::process::id()).unwrap();
    let mut reader = connect(&restored, true);
    let welcome: serde_json::Value = receive(&mut reader).unwrap();
    assert_eq!(welcome["restore"], descriptor);
    host.close(&key, &restored).unwrap();
    let manual = host.prepare(&key, false, false, false).unwrap();
    host.bind(&key, &manual, std::process::id()).unwrap();
    let mut reader = connect(&manual, true);
    let welcome: serde_json::Value = receive(&mut reader).unwrap();
    assert!(welcome["restore"].is_null());
}

#[test]
fn shell_replay_retains_only_initial_prompt_and_command() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    let endpoint = host.prepare(&key, false, false, false).unwrap();
    let entry = host.entry(&key, &endpoint).unwrap();
    {
        let mut state = entry.state.lock().unwrap();
        let registration = state.execution.register(uuid::Uuid::new_v4()).unwrap();
        let checkpoint = state
            .execution
            .update(registration, 1, Some("{\"application\":\"test\"}".into()))
            .unwrap();
        save(&entry.file, &checkpoint).unwrap();
        state.execution.committed(checkpoint).unwrap();
    }
    let restored = host.prepare(&key, true, true, true).unwrap();
    host.boundary(&key, &restored, true).unwrap();
    host.boundary(&key, &restored, false).unwrap();
    let entry = host.entry(&key, &restored).unwrap();
    assert!(
        entry
            .state
            .lock()
            .unwrap()
            .execution
            .committed
            .descriptor
            .is_some()
    );
    host.boundary(&key, &restored, true).unwrap();
    assert!(
        entry
            .state
            .lock()
            .unwrap()
            .execution
            .committed
            .descriptor
            .is_none()
    );
    assert!(host.boundary(&key, &endpoint, false).is_err());
}

#[test]
fn replacing_endpoint_drains_old_writer_and_stale_close_cannot_remove_successor() {
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(Host::new(directory.path().to_owned()).unwrap());
    let key = uuid::Uuid::new_v4().to_string();
    let old_endpoint = host.prepare(&key, false, false, false).unwrap();
    let old = host.entry(&key, &old_endpoint).unwrap();
    let mut writer = old.state.lock().unwrap();
    let owner = writer.execution.register(uuid::Uuid::new_v4()).unwrap();
    let candidate = writer
        .execution
        .update(owner, 1, Some("{\"session\":\"latest\"}".into()))
        .unwrap();
    let replacement_host = Arc::clone(&host);
    let replacement_key = key.clone();
    let replacement = std::thread::spawn(move || {
        replacement_host
            .prepare(&replacement_key, true, false, false)
            .unwrap()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !old.stopped.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "replacement did not fence old writer"
        );
        std::thread::yield_now();
    }
    save(&old.file, &candidate).unwrap();
    writer.execution.committed(candidate).unwrap();
    drop(writer);
    let endpoint = replacement.join().unwrap();
    let current = host.entry(&key, &endpoint).unwrap();
    assert_eq!(
        current
            .state
            .lock()
            .unwrap()
            .execution
            .committed
            .descriptor
            .as_deref(),
        Some("{\"session\":\"latest\"}")
    );
    assert!(host.close(&key, &old_endpoint).is_err());
    assert!(host.entry(&key, &endpoint).is_ok());
}

#[test]
fn versioned_prepare_rejects_checkpoint_from_another_invocation() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    let launch = host
        .prepare_checked(&key, false, false, false, IdentityCheck::Expected(None))
        .unwrap();
    assert!(
        host.prepare_checked(
            &key,
            true,
            false,
            false,
            IdentityCheck::Expected(Some("wrong-invocation"))
        )
        .is_err()
    );
    let restored = host
        .prepare_checked(
            &key,
            true,
            false,
            false,
            IdentityCheck::Expected(Some(&launch.invocation)),
        )
        .unwrap();
    assert_eq!(restored.invocation, launch.invocation);
}

#[test]
fn versioned_boundaries_return_identity_and_missing_references_fail() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    assert!(
        host.prepare_checked(
            &key,
            true,
            false,
            false,
            IdentityCheck::Expected(Some("missing"))
        )
        .is_err()
    );
    let launch = host
        .prepare_checked(&key, false, true, false, IdentityCheck::Expected(None))
        .unwrap();
    assert_eq!(
        host.boundary_identity(&key, &launch.endpoint, true)
            .unwrap(),
        launch.invocation
    );
    let next = host
        .boundary_identity(&key, &launch.endpoint, false)
        .unwrap();
    assert_ne!(next, launch.invocation);
    let restored = host
        .prepare_checked(&key, true, true, true, IdentityCheck::Expected(Some(&next)))
        .unwrap();
    assert_eq!(
        host.boundary_identity(&key, &restored.endpoint, true)
            .unwrap(),
        next
    );
    assert_eq!(
        host.boundary_identity(&key, &restored.endpoint, false)
            .unwrap(),
        next
    );
    assert_ne!(
        host.boundary_identity(&key, &restored.endpoint, true)
            .unwrap(),
        next
    );
}

#[test]
fn checkpoint_read_distinguishes_missing_from_unreadable_and_oversized() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        load_checkpoint(&directory.path().join("missing"))
            .unwrap()
            .is_none()
    );
    assert!(load_checkpoint(directory.path()).is_err());
    let large = directory.path().join("large");
    std::fs::write(&large, vec![b' '; 65_537]).unwrap();
    assert!(load_checkpoint(&large).is_err());
}

#[test]
fn corrupt_checkpoint_does_not_become_empty_installation() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    std::fs::write(directory.path().join(format!("{key}.json")), "corrupt").unwrap();
    assert!(host.prepare(&key, true, false, false).is_err());
}

#[test]
fn shell_ownership_requires_current_foreground_child() {
    assert!(foreground_child("100 200 200\n", 100));
    for identity in [
        "100 200 300",
        "101 200 200",
        "100 200 -1",
        "100 0 0",
        "100 200",
        "100 200 200 extra",
        "corrupt",
    ] {
        assert!(!foreground_child(identity, 100), "accepted {identity}");
    }
}

#[test]
fn actual_peer_identity_rejects_forged_pid() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    let endpoint = host.prepare(&key, false, false, false).unwrap();
    host.bind(&key, &endpoint, std::process::id()).unwrap();
    let stream = UnixStream::connect(endpoint.strip_prefix("v1:").unwrap()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    send(reader.get_mut(), &serde_json::json!({"protocol":PROTOCOL,"application":"test","pid":1,"accept_restore":true})).unwrap();
    assert!(receive::<serde_json::Value>(&mut reader).is_err());
}
