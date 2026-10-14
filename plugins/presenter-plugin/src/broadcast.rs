//! Caller-owned output source. No relay credentials or network work in render hooks.
use std::{sync::Arc, time::Duration};

use bmux_broadcast_plugin_api::{broadcast_control_v1 as control, broadcast_output_v1 as output};
use tokio::sync::watch;

const CALL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

type Frame = Arc<bmux_web_renderer::BrowserFrame>;

#[cfg(unix)]
const PROOF_BASENAME: &std::ffi::CStr = c"broadcast-local-proof";

// Never return paths, filesystem errors, or proof bytes as diagnostics.
#[cfg(unix)]
fn read_local_proof(runtime: &std::path::Path) -> Option<Vec<u8>> {
    use std::{
        fs::{File, OpenOptions},
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(runtime)
        .ok()?;
    // SAFETY: geteuid has no preconditions and does not modify process state.
    let owner = unsafe { libc::geteuid() };
    let metadata = directory.metadata().ok()?;
    if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
        return None;
    }
    // SAFETY: directory owns a live descriptor; the literal is NUL-terminated.
    // openat pins the directory, and NOFOLLOW/NONBLOCK reject links and blocking IO.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            PROOF_BASENAME.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return None;
    }
    // SAFETY: openat returned a fresh descriptor whose ownership transfers to File.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() != 32
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(33);
    file.take(33).read_to_end(&mut bytes).ok()?;
    (bytes.len() == 32).then_some(bytes)
}

#[cfg(not(unix))]
fn read_local_proof(_runtime: &std::path::Path) -> Option<Vec<u8>> {
    None
}

async fn authorize_source<C: bmux_plugin_sdk::TypedDispatchClient>(
    client: &mut C,
    runtime: &std::path::Path,
) -> bool {
    if !matches!(
        tokio::time::timeout(CALL_TIMEOUT, control::client::prepare_local(client)).await,
        Ok(Ok(true))
    ) {
        return false;
    }
    let Some(proof) = read_local_proof(runtime) else {
        return false;
    };
    matches!(
        tokio::time::timeout(CALL_TIMEOUT, control::client::authorize(client, proof)).await,
        Ok(Ok(true))
    )
}

pub(crate) struct Source {
    frames: watch::Sender<Option<Frame>>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Source {
    pub fn start(source: uuid::Uuid, paths: bmux_config::ConfigPaths) -> Self {
        let (frames, receiver) = watch::channel(None);
        let (stop, stopping) = watch::channel(false);
        let task = tokio::spawn(run(source.to_string(), paths, receiver, stopping));
        Self { frames, stop, task }
    }

    pub fn publish(&self, frame: bmux_web_renderer::BrowserFrame) {
        self.frames.send_replace(Some(Arc::new(frame)));
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn needs_publication(
    selected: bool,
    refresh: u64,
    revision: u64,
    sent: Option<(u64, u64)>,
) -> bool {
    selected && sent != Some((refresh, revision))
}

async fn run(
    source: String,
    paths: bmux_config::ConfigPaths,
    frames: watch::Receiver<Option<Frame>>,
    mut stop: watch::Receiver<bool>,
) {
    eprintln!("Broadcast source {source}: registering (audience output only)");
    let mut retry = tokio::time::interval(Duration::from_secs(1));
    let mut unavailable = false;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = retry.tick() => {}
        }
        if *stop.borrow() {
            break;
        }
        let connection = tokio::time::timeout(
            CALL_TIMEOUT,
            bmux_client::BmuxClient::connect_with_paths(&paths, "presenter broadcast source"),
        )
        .await;
        let Ok(Ok(mut client)) = connection else {
            if !unavailable {
                eprintln!("Broadcast source {source}: unavailable; local playback continues");
            }
            unavailable = true;
            continue;
        };
        if !authorize_source(&mut client, &paths.runtime_dir).await {
            if !unavailable {
                eprintln!("Broadcast source {source}: unavailable; local playback continues");
            }
            unavailable = true;
            continue;
        }
        let registered = tokio::time::timeout(
            CALL_TIMEOUT,
            output::client::register(&mut client, source.clone()),
        )
        .await;
        if !matches!(registered, Ok(Ok(_))) {
            if !unavailable {
                eprintln!("Broadcast source {source}: unavailable; local playback continues");
            }
            unavailable = true;
            continue;
        }
        unavailable = false;
        eprintln!("Broadcast source {source}: registered; select with --source {source}");
        let mut sent = None;
        let mut selected = None;
        let mut rejected = false;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = tick.tick() => {}
            }
            if *stop.borrow() {
                break;
            }
            let state = tokio::time::timeout(
                CALL_TIMEOUT,
                output::client::poll(&mut client, source.clone()),
            )
            .await;
            let Ok(Ok(state)) = state else {
                break;
            };
            if selected != Some(state.selected) {
                eprintln!(
                    "Broadcast source {source}: {}",
                    if state.selected {
                        "selected"
                    } else {
                        "standby"
                    }
                );
                selected = Some(state.selected);
            }
            let Some(frame) = frames.borrow().clone() else {
                continue;
            };
            let revision = frame.revision();
            if !needs_publication(state.selected, state.refresh, revision, sent) {
                continue;
            }
            let mut encoded =
                tokio::task::spawn_blocking(move || bmux_web_renderer::encode_binary(&frame));
            let mut lease = tokio::time::interval(Duration::from_secs(1));
            let mut invalidated = false;
            let bytes = loop {
                tokio::select! {
                    _ = stop.changed() => { invalidated = true; break None; }
                    _ = lease.tick() => {
                        let renewed = tokio::time::timeout(CALL_TIMEOUT,
                            output::client::poll(&mut client, source.clone())).await;
                        match renewed {
                            Ok(Ok(current)) => {
                                invalidated |= !current.selected || current.refresh != state.refresh;
                            }
                            _ => { invalidated = true; }
                        }
                    }
                    result = &mut encoded => {
                        break match result {
                            Ok(Ok(bytes)) if bytes.len() <= MAX_FRAME_BYTES => Some(bytes),
                            _ => {
                                if !rejected { eprintln!("Broadcast source {source}: frame rejected; local playback continues"); }
                                rejected = true;
                                None
                            }
                        };
                    }
                }
            };
            if *stop.borrow() {
                break;
            }
            if invalidated {
                continue;
            }
            let Some(bytes) = bytes else {
                continue;
            };
            // Async work may have crossed pause/resume/switch or a newer local commit.
            let current = tokio::time::timeout(
                CALL_TIMEOUT,
                output::client::poll(&mut client, source.clone()),
            )
            .await;
            let Ok(Ok(current)) = current else {
                break;
            };
            if !current.selected
                || current.refresh != state.refresh
                || frames
                    .borrow()
                    .as_ref()
                    .is_none_or(|frame| frame.revision() != revision)
                || *stop.borrow()
            {
                continue;
            }
            let published = tokio::time::timeout(
                CALL_TIMEOUT,
                output::client::publish(&mut client, source.clone(), current.refresh, bytes),
            )
            .await;
            match published {
                Ok(Ok(true)) => {
                    sent = Some((current.refresh, revision));
                    rejected = false;
                }
                Ok(Ok(false)) => {}
                _ => break,
            }
        }
        // Explicit release is best effort; dropping this same authenticated connection
        // and the controller's six-second source lease cover transport failure/crash.
        let _ = tokio::time::timeout(
            CALL_TIMEOUT,
            output::client::unregister(&mut client, source.clone()),
        )
        .await;
        drop(client);
        if *stop.borrow() {
            break;
        }
        eprintln!("Broadcast source {source}: disconnected; local playback continues");
    }
    eprintln!("Broadcast source {source}: closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingClient {
        calls: Vec<String>,
        refresh: u64,
        selected: bool,
        authorized: bool,
        prepare_available: bool,
        prepared: bool,
    }

    impl bmux_plugin_sdk::TypedDispatchClient for RecordingClient {
        async fn invoke_service_raw(
            &mut self,
            capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            self.calls.push(operation.into());
            if operation == "prepare-local" {
                assert_eq!(capability, "bmux.broadcast.control.v1");
                assert_eq!(interface, "broadcast-control-v1");
                self.prepared = self.prepare_available;
                return Ok(bmux_plugin_sdk::encode_service_message(&self.prepared).unwrap());
            }
            if operation == "authorize" {
                assert!(self.prepared, "proof authorization must follow preparation");
                assert_eq!(capability, "bmux.broadcast.control.v1");
                assert_eq!(interface, "broadcast-control-v1");
                let request: control::client::AuthorizeRequest =
                    bmux_plugin_sdk::decode_service_message(&payload).unwrap();
                self.authorized = request.local_proof == vec![7; 32];
                return Ok(bmux_plugin_sdk::encode_service_message(&self.authorized).unwrap());
            }
            assert!(
                self.authorized,
                "output must follow authorization on this client"
            );
            assert_eq!(capability, "bmux.broadcast.output.v1");
            assert_eq!(interface, "broadcast-output-v1");
            match operation {
                "register" | "poll" => Ok(bmux_plugin_sdk::encode_service_message(
                    &output::SourceState {
                        refresh: self.refresh,
                        selected: self.selected,
                    },
                )
                .unwrap()),
                "publish" => {
                    let request: output::client::PublishRequest =
                        bmux_plugin_sdk::decode_service_message(&payload).unwrap();
                    assert_eq!(request.refresh, self.refresh);
                    assert_eq!(request.frame, b"BMWP\x02");
                    Ok(bmux_plugin_sdk::encode_service_message(&true).unwrap())
                }
                "unregister" => Ok(bmux_plugin_sdk::encode_service_message(&true).unwrap()),
                _ => panic!("unexpected operation"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_source_lifecycle_uses_one_client_and_refresh_contract() {
        let source = uuid::Uuid::new_v4().to_string();
        let mut client = RecordingClient {
            calls: Vec::new(),
            refresh: 1,
            selected: false,
            authorized: false,
            prepare_available: true,
            prepared: false,
        };
        let proof = ProofDirectory::new();
        proof.write(&[7; 32], 0o600);
        assert!(authorize_source(&mut client, &proof.0).await);
        let registered = output::client::register(&mut client, source.clone())
            .await
            .unwrap();
        assert!(!registered.selected);
        client.refresh = 2;
        client.selected = true;
        let state = output::client::poll(&mut client, source.clone())
            .await
            .unwrap();
        assert!(needs_publication(state.selected, state.refresh, 1, None));
        assert!(
            output::client::publish(
                &mut client,
                source.clone(),
                state.refresh,
                b"BMWP\x02".to_vec()
            )
            .await
            .unwrap()
        );
        client.selected = false;
        client.refresh = 3;
        let paused = output::client::poll(&mut client, source.clone())
            .await
            .unwrap();
        assert!(!needs_publication(
            paused.selected,
            paused.refresh,
            1,
            Some((2, 1))
        ));
        assert!(
            output::client::unregister(&mut client, source)
                .await
                .unwrap()
        );
        assert_eq!(
            client.calls,
            [
                "prepare-local",
                "authorize",
                "register",
                "poll",
                "publish",
                "poll",
                "unregister"
            ]
        );
    }

    #[cfg(unix)]
    struct ProofDirectory(std::path::PathBuf);

    #[cfg(unix)]
    impl ProofDirectory {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("bmux-presenter-proof-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join(PROOF_BASENAME.to_str().unwrap())
        }
        fn write(&self, bytes: &[u8], mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(self.path(), bytes).unwrap();
            std::fs::set_permissions(self.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    #[cfg(unix)]
    impl Drop for ProofDirectory {
        fn drop(&mut self) {
            for entry in std::fs::read_dir(&self.0).unwrap() {
                let _ = std::fs::remove_file(entry.unwrap().path());
            }
            std::fs::remove_dir(&self.0).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn proof_read_is_private_exact_bounded_and_rejects_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let proof = ProofDirectory::new();
        assert!(read_local_proof(&proof.0).is_none());
        for size in [0, 31, 33, 1024] {
            proof.write(&vec![7; size], 0o600);
            assert!(read_local_proof(&proof.0).is_none());
        }
        proof.write(&[7; 32], 0o644);
        assert!(read_local_proof(&proof.0).is_none());
        proof.write(&[7; 32], 0o600);
        assert_eq!(read_local_proof(&proof.0), Some(vec![7; 32]));
        std::fs::hard_link(proof.path(), proof.0.join("hard-link")).unwrap();
        assert!(read_local_proof(&proof.0).is_none());
        std::fs::remove_file(proof.0.join("hard-link")).unwrap();
        std::fs::rename(proof.path(), proof.0.join("target")).unwrap();
        symlink(proof.0.join("target"), proof.path()).unwrap();
        assert!(read_local_proof(&proof.0).is_none());
        std::fs::remove_file(proof.path()).unwrap();
        std::fs::rename(proof.0.join("target"), proof.path()).unwrap();
        std::fs::set_permissions(&proof.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(read_local_proof(&proof.0).is_none());
        std::fs::set_permissions(&proof.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = ProofDirectory::new();
        symlink(&proof.0, link.0.join("runtime-link")).unwrap();
        assert!(read_local_proof(&link.0.join("runtime-link")).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unavailable_and_wrong_proof_never_register_and_reconnect_authorizes_again() {
        let proof = ProofDirectory::new();
        let mut client = RecordingClient {
            calls: Vec::new(),
            refresh: 1,
            selected: false,
            authorized: false,
            prepare_available: true,
            prepared: false,
        };
        assert!(!authorize_source(&mut client, &proof.0).await);
        assert_eq!(client.calls, ["prepare-local"]);
        proof.write(&[8; 32], 0o600);
        assert!(!authorize_source(&mut client, &proof.0).await);
        assert_eq!(
            client.calls,
            ["prepare-local", "prepare-local", "authorize"]
        );
        proof.write(&[7; 32], 0o600);
        assert!(authorize_source(&mut client, &proof.0).await);
        let mut reconnected = RecordingClient {
            calls: Vec::new(),
            refresh: 1,
            selected: false,
            authorized: false,
            prepare_available: true,
            prepared: false,
        };
        assert!(authorize_source(&mut reconnected, &proof.0).await);
        output::client::register(&mut reconnected, uuid::Uuid::new_v4().to_string())
            .await
            .unwrap();
        assert_eq!(
            reconnected.calls,
            ["prepare-local", "authorize", "register"]
        );
        let mut unavailable = RecordingClient {
            calls: Vec::new(),
            refresh: 1,
            selected: false,
            authorized: false,
            prepare_available: false,
            prepared: false,
        };
        assert!(!authorize_source(&mut unavailable, &proof.0).await);
        assert_eq!(unavailable.calls, ["prepare-local"]);
        assert!(!unavailable.authorized);
    }

    #[test]
    fn refresh_republishes_unchanged_scene_and_selection_gates_output() {
        assert!(needs_publication(true, 1, 9, None));
        assert!(!needs_publication(true, 1, 9, Some((1, 9))));
        assert!(needs_publication(true, 2, 9, Some((1, 9))));
        assert!(needs_publication(true, 2, 10, Some((2, 9))));
        assert!(!needs_publication(false, 2, 10, Some((1, 9))));
    }
}
