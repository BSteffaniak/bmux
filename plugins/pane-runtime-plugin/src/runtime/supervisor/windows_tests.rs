//! Native Windows transport tests; these do not launch an application or helper.
use super::*;
use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS,
};

fn connected_pair() -> (Stream, std::fs::File) {
    let endpoint = endpoint(Path::new("unused"));
    let name = wide(endpoint.as_os_str());
    // SAFETY: name is NUL terminated; successful pipe handle is adopted once.
    let server = unsafe {
        let handle = CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            std::ptr::null(),
        );
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        OwnedHandle::from_raw_handle(handle)
    };
    let client = connect(&endpoint, Duration::from_secs(1)).unwrap();
    // SAFETY: the client already connected, so this synchronous accept returns
    // immediately with ERROR_PIPE_CONNECTED (or success).
    let result = unsafe { ConnectNamedPipe(server.as_raw_handle(), std::ptr::null_mut()) };
    if result == 0 {
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(i32::try_from(ERROR_PIPE_CONNECTED).unwrap())
        );
    }
    (client, std::fs::File::from(server))
}

#[test]
fn pipe_deadline_cancels_kernel_read_and_allows_later_read() {
    let (mut client, mut server) = connected_pair();
    client
        .set_read_timeout(Some(Duration::from_millis(30)))
        .unwrap();
    assert_eq!(
        client.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    server.write_all(&[42]).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0];
    client.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [42]);
}

#[test]
fn interrupter_rejects_non_pipe_handles() {
    let file = tempfile::tempfile().unwrap();
    let stream = Stream {
        pipe: Arc::new(Pipe {
            handle: file.into(),
            interrupted: AtomicBool::new(false),
        }),
        read_deadline: Mutex::new(None),
        write_deadline: Mutex::new(None),
    };
    assert!(Interrupter::new(&stream).is_err());
}

#[test]
fn interruption_is_sticky_and_wakes_a_pending_lifecycle_read() {
    let (mut client, _server) = connected_pair();
    client.set_read_timeout(None).unwrap();
    let interrupt = Interrupter::new(&client).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let result = client.read(&mut [0]);
        send.send(result.is_err()).unwrap();
        assert_eq!(
            client.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    });
    interrupt.interrupt().unwrap();
    assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
    reader.join().unwrap();
}

#[test]
fn lifetime_revocation_is_sticky_and_drops_the_dedicated_lease() {
    for installed in [false, true] {
        let mut lifetime = Lifetime::default();
        if installed {
            let (client, mut server) = connected_pair();
            lifetime.install(client).unwrap();
            lifetime.revoke();
            let result = server.read(&mut [0]);
            assert!(matches!(result, Ok(0)) || result.is_err());
        } else {
            lifetime.revoke();
        }
        lifetime.revoke();
        let (client, _server) = connected_pair();
        assert!(lifetime.install(client).is_err());
        assert!(lifetime.socket.is_none());
    }
}

#[test]
fn secure_bootstrap_is_retained_only_by_explicit_keep() {
    let directory = short_endpoint_directory().unwrap();
    let path = directory.path().to_path_buf();
    secure_bootstrap(
        &path,
        &protocol::Bootstrap {
            version: protocol::VERSION,
            execution_token: "test".into(),
            authentication: "fixture-not-a-secret".into(),
        },
    )
    .unwrap();
    assert!(path.join("bootstrap.json").is_file());
    assert!(
        secure_bootstrap(
            &path,
            &protocol::Bootstrap {
                version: protocol::VERSION,
                execution_token: "other".into(),
                authentication: "fixture-not-a-secret".into(),
            }
        )
        .is_err()
    );
    drop(directory);
    assert!(!path.exists());
}
