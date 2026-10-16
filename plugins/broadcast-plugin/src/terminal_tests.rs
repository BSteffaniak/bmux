use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn socket_rendezvous_is_private_short_and_removed_on_drop() {
    let directory = observer_directory().unwrap();
    let path = directory.path().to_owned();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let socket = path.join("out.sock");
    assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(listener);
    drop(directory);
    assert!(!path.exists());
}

#[tokio::test]
async fn observer_child_exit_is_reported_without_waiting_for_socket_timeout() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("observer-fixture");
    let bytes = b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture; exit 0; fi\nexit 7\n";
    std::fs::write(&executable, bytes).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sha256 = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let long_runtime = root.path().join("runtime".repeat(30));
    let selection = RuntimeSelection {
        executable,
        sha256,
        version: "fixture".into(),
        paths: bmux_config::ConfigPaths::new(
            root.path().join("config"),
            long_runtime,
            root.path().join("data"),
            root.path().join("state"),
        ),
    };
    let (frames, _) = watch::channel(None);
    let (_stop, stopped) = watch::channel(false);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        observe(selection, Uuid::new_v4(), 80, 24, frames, stopped),
    )
    .await
    .unwrap();
    assert_eq!(result, Err("observer_exited"));
}
