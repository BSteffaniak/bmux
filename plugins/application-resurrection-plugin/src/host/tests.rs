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
fn corrupt_checkpoint_does_not_become_empty_installation() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::new(directory.path().to_owned()).unwrap();
    let key = uuid::Uuid::new_v4().to_string();
    std::fs::write(directory.path().join(format!("{key}.json")), "corrupt").unwrap();
    assert!(host.prepare(&key, true, false, false).is_err());
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
