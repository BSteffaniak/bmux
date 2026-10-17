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
async fn negotiation_reconstructs_delta_before_latest_coalescing() {
    let (mut source, reader) = tokio::net::UnixStream::pair().unwrap();
    let target = Uuid::new_v4();
    let epoch = Uuid::new_v4();
    let generation = format!("{epoch}:{target}");
    let scene =
        bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(0, 0, 4, 1));
    let prepared = scene
        .prepare(bmux_tui::damage::Damage::Full, |_| {})
        .unwrap();
    let first =
        bmux_web_renderer::BrowserFrame::from_prepared_in_generation(&generation, 1, &prepared)
            .unwrap();
    let mut value = serde_json::to_value(&first).unwrap();
    value["revision"] = 2.into();
    value["buffer"]["cells"][1]["symbol"] = "2".into();
    let second = serde_json::from_value(value).unwrap();
    let full = bmux_web_renderer::encode_binary(&first).unwrap();
    let delta = bmux_web_renderer::encode_delta(&second, &first).unwrap();
    let (sender, receiver) = watch::channel(None);
    let read = read_output(reader, target, &sender);
    let write = async {
        let mut request = [0; 8];
        source.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[..6], b"BMAQ\x02\x02");
        assert!((1..=60).contains(&request[6]));
        assert_eq!(request[7], 0);
        source.write_all(b"BMAO\x02").await.unwrap();
        source.write_all(epoch.as_bytes()).await.unwrap();
        source.write_all(target.as_bytes()).await.unwrap();
        source.write_all(&request[6..7]).await.unwrap();
        for bytes in [full, delta] {
            source
                .write_u32(u32::try_from(bytes.len()).unwrap())
                .await
                .unwrap();
            source.write_all(&bytes).await.unwrap();
        }
        source.shutdown().await.unwrap();
    };
    let (result, ()) = tokio::join!(read, write);
    assert_eq!(result, Err("observer_closed"));
    let latest = receiver.borrow().clone().unwrap();
    assert_eq!(
        bmux_web_renderer::decode_binary(&latest, None).unwrap(),
        second
    );
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
