//! Opt-in real ConPTY product-path test. Build the helper first, set
//! BMUX_SUPERVISOR_TEST_HELPER to its absolute path, then run this bin's tests
//! with `--ignored`. No existing runtime or user terminal is touched.
use super::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Write;
use windows_sys::Win32::Foundation::{GENERIC_WRITE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

fn server() -> Server {
    let bootstrap = Bootstrap {
        version: protocol::VERSION,
        execution_token: uuid::Uuid::new_v4().to_string(),
        authentication: "a".repeat(64),
    };
    Server {
        scope: Scope::new(bootstrap.execution_token.clone()).unwrap(),
        bootstrap,
        receipts: BTreeMap::new(),
        receipt_bytes: 0,
        lease: false,
        retired: false,
        revision: 0,
        last_status: Vec::new(),
    }
}
#[test]
fn authenticated_contract_and_mutation_replay() {
    let mut server = server();
    let mut request = Request {
        version: protocol::VERSION,
        execution_token: server.bootstrap.execution_token.clone(),
        authentication: server.bootstrap.authentication.clone(),
        operation: Operation::Hello,
    };
    assert!(server.authenticated(&request));
    request.authentication.push('a');
    assert!(!server.authenticated(&request));
    request.authentication = server.bootstrap.authentication.clone();
    request.version -= 1;
    assert!(!server.authenticated(&request));
    let mutation = uuid::Uuid::new_v4().to_string();
    let response = server
        .operate(&Operation::Term {
            mutation_id: mutation.clone(),
        })
        .unwrap();
    assert!(response.error.unwrap().contains("unsupported"));
    assert!(
        server
            .operate(&Operation::Sweep {
                mutation_id: mutation.clone()
            })
            .is_err()
    );
    let response = server
        .operate(&Operation::Term {
            mutation_id: mutation,
        })
        .unwrap();
    assert!(response.error.unwrap().contains("unsupported"));
    assert_eq!(response.terminal_session_ended, Some(false));
    let snapshot = server.snapshot().unwrap();
    assert_eq!(server.snapshot().unwrap().revision, snapshot.revision);
}
#[test]
fn first_instance_pipe_and_nonblocking_frame_limits() {
    let security = Security::current_user().unwrap();
    let name = wide(OsStr::new(&format!(
        r"\\.\pipe\bmux-exec-{}",
        uuid::Uuid::new_v4()
    )))
    .unwrap();
    let accepting = listener(&name, &security, true).unwrap();
    assert!(
        listener(&name, &security, true).is_err(),
        "first-instance squatting must fail"
    );
    unsafe { ConnectNamedPipe(accepting.0, null_mut()) };
    let path = String::from_utf16(&name[..name.len() - 1]).unwrap();
    let mut client = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut peer = Peer::new(accepting);
    assert!(peer.read().unwrap().is_none(), "idle connection is not EOF");
    client
        .write_all(&((protocol::MAX_FRAME + 1) as u32).to_be_bytes())
        .unwrap();
    assert!(
        peer.read().is_err(),
        "oversized declaration must fail before allocation"
    );
}
#[test]
fn starts_require_lifetime_and_retirement_requires_empty_scope() {
    let mut server = server();
    let response = server
        .operate(&Operation::Start {
            mutation_id: uuid::Uuid::new_v4().to_string(),
            program: "cmd.exe".into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            native_env: None,
            native_env_encoding: None,
        })
        .unwrap();
    assert!(response.error.unwrap().contains("lifetime"));
    assert!(!response.started);
    assert!(
        server
            .operate(&Operation::RetireOwner {
                mutation_id: uuid::Uuid::new_v4().to_string()
            })
            .unwrap()
            .error
            .is_some()
    );
    let response = server
        .operate(&Operation::Sweep {
            mutation_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
    assert_eq!(response.terminal_session_ended, Some(true));
    assert!(
        server
            .operate(&Operation::RetireOwner {
                mutation_id: uuid::Uuid::new_v4().to_string()
            })
            .unwrap()
            .error
            .is_none()
    );
}

struct Fixture {
    _directory: tempfile::TempDir,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pipe: String,
    bootstrap: Bootstrap,
    output: std::sync::mpsc::Receiver<Vec<u8>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let deadline = Instant::now() + TIMEOUT;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // ClosePseudoConsole can block in the OS even after target termination.
        // Keep that destructor off the test/unwind thread; output keeps draining.
        if let Some(master) = self.master.take() {
            let (done, completed) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                drop(master);
                let _ = done.send(());
            });
            if completed.recv_timeout(TIMEOUT).is_err() {
                eprintln!("test ConPTY close did not complete within deadline");
            }
        }
    }
}
impl Fixture {
    fn new() -> Self {
        let executable = std::env::var_os("BMUX_SUPERVISOR_TEST_HELPER")
            .expect("set BMUX_SUPERVISOR_TEST_HELPER to the built helper's absolute path");
        assert!(std::path::Path::new(&executable).is_absolute());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bootstrap.json");
        let security = Security::current_user().unwrap();
        let attributes = security.attributes();
        let path_wide = wide(path.as_os_str()).unwrap();
        let handle = Handle::new(unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                0,
                null_mut(),
            )
        })
        .unwrap();
        let raw = handle.0;
        std::mem::forget(handle);
        let mut file = unsafe { std::fs::File::from_raw_handle(raw) };
        let bootstrap = Bootstrap {
            version: protocol::VERSION,
            execution_token: uuid::Uuid::new_v4().to_string(),
            authentication: format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        };
        file.write_all(&serde_json::to_vec(&bootstrap).unwrap())
            .unwrap();
        drop(file);
        let pipe = format!(r"\\.\pipe\bmux-exec-{}", uuid::Uuid::new_v4());
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(executable);
        command.arg("--guardian-windows-session-v1");
        command.arg(&path);
        command.arg(&pipe);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let (send, output) = std::sync::mpsc::sync_channel(16);
        std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut handshake = Vec::new();
            let mut truncated = false;
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                // portable-pty enables PSEUDOCONSOLE_INHERIT_CURSOR. Conhost
                // requests the cursor before launching the attached process;
                // a headless test must answer, just like the real VT renderer.
                for byte in &bytes[..count] {
                    handshake.push(*byte);
                    if handshake.ends_with(b"\x1b[6n") {
                        let _ = writer.write_all(b"\x1b[1;1R");
                        let _ = writer.flush();
                    }
                    if handshake.len() > 4 {
                        handshake.remove(0);
                    }
                }
                // Bounded diagnostic retention; always drain ConPTY even when
                // the assertion thread is waiting for pipe readiness.
                match send.try_send(bytes[..count].to_vec()) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        if !truncated {
                            eprintln!("test ConPTY diagnostic output truncated at bounded queue");
                            truncated = true;
                        }
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                }
            }
        });
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        Self {
            _directory: directory,
            child,
            master: Some(pair.master),
            pipe,
            bootstrap,
            output,
        }
    }
    fn connect(&self, operation: Operation) -> std::fs::File {
        let deadline = Instant::now() + TIMEOUT;
        let mut connection = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.pipe)
            {
                Ok(connection) => break connection,
                Err(error) => {
                    let exited = self.child.as_raw_handle().is_some_and(|handle| unsafe {
                        WaitForSingleObject(handle.cast(), 0) == WAIT_OBJECT_0
                    });
                    if exited || Instant::now() >= deadline {
                        let mut diagnostic = Vec::new();
                        while let Ok(bytes) = self.output.try_recv() {
                            if diagnostic.len() + bytes.len() > 65_536 {
                                break;
                            }
                            diagnostic.extend(bytes);
                        }
                        panic!(
                            "pipe readiness: {error}; helper_exited={exited}; bounded console output: {:?}",
                            String::from_utf8_lossy(&diagnostic)
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        protocol::write_frame(
            &mut connection,
            &Request {
                version: protocol::VERSION,
                execution_token: self.bootstrap.execution_token.clone(),
                authentication: self.bootstrap.authentication.clone(),
                operation,
            },
        )
        .unwrap();
        connection
    }
    fn receive<T: serde::de::DeserializeOwned>(&self, connection: &mut std::fs::File) -> T {
        let deadline = Instant::now() + TIMEOUT;
        let mut frame = Vec::new();
        loop {
            let mut available = 0;
            let handle = std::os::windows::io::AsRawHandle::as_raw_handle(connection);
            assert_ne!(
                unsafe {
                    PeekNamedPipe(
                        handle.cast(),
                        null_mut(),
                        0,
                        null_mut(),
                        &mut available,
                        null_mut(),
                    )
                },
                0,
                "response pipe closed: {}",
                io::Error::last_os_error()
            );
            if available > 0 {
                let mut bytes = [0; 8192];
                let amount = (available as usize).min(bytes.len());
                let count = connection.read(&mut bytes[..amount]).unwrap();
                assert!(count > 0, "response pipe EOF");
                frame.extend_from_slice(&bytes[..count]);
                assert!(frame.len() <= protocol::MAX_FRAME + 4, "oversized response");
            }
            if frame.len() >= 4 {
                let length = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
                assert!(
                    length <= protocol::MAX_FRAME,
                    "oversized response declaration"
                );
                if frame.len() >= length + 4 {
                    return protocol::read_frame(&mut frame.as_slice()).unwrap();
                }
            }
            assert!(
                Instant::now() < deadline,
                "response deadline elapsed after {} bytes",
                frame.len()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn call(&self, operation: Operation) -> Response {
        self.receive(&mut self.connect(operation))
    }
    fn bind(&self) -> std::fs::File {
        let mut lease = self.connect(Operation::BindLifetime);
        let response: Response = self.receive(&mut lease);
        assert!(response.error.is_none());
        lease
    }
    fn start(&self, script: &str) -> Response {
        self.start_with_environment(script, std::env::vars().collect(), None)
    }
    fn start_with_environment(
        &self,
        script: &str,
        env: BTreeMap<String, String>,
        native_env: Option<Vec<(Vec<u8>, Vec<u8>)>>,
    ) -> Response {
        let native_env_encoding = native_env.as_ref().map(|_| "windows-utf16le".into());
        self.call(Operation::Start {
            mutation_id: uuid::Uuid::new_v4().to_string(),
            program: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            args: vec!["/D".into(), "/C".into(), script.into()],
            cwd: None,
            env,
            native_env,
            native_env_encoding,
        })
    }
}

#[test]
#[ignore = "requires built helper path and real Windows ConPTY"]
fn conpty_cmd_pseudo_environment_launches_in_both_transports() {
    for native in [false, true] {
        let fixture = Fixture::new();
        let lease = fixture.bind();
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        env.insert("=C:".into(), "C:\\".into());
        env.insert("=ExitCode".into(), "00000000".into());
        env.insert("=ExitCodeAscii".into(), "A".into());
        let entries = native.then(|| {
            env.iter()
                .map(|(key, value)| {
                    (
                        key.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                        value.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                    )
                })
                .collect()
        });
        let response = fixture.start_with_environment(
            "echo BMUX_CMD_PSEUDO_ENV_OK & exit /b 37",
            env,
            entries,
        );
        assert!(
            response.error.is_none(),
            "native={native}: {:?}",
            response.error
        );
        let deadline = Instant::now() + TIMEOUT;
        let mut output = Vec::new();
        loop {
            if let Ok(bytes) = fixture.output.recv_timeout(Duration::from_millis(20)) {
                assert!(output.len() + bytes.len() <= 65_536);
                output.extend(bytes);
            }
            let status = fixture.call(Operation::Status);
            if status.terminal_session_ended == Some(true)
                && String::from_utf8_lossy(&output).contains("BMUX_CMD_PSEUDO_ENV_OK")
            {
                assert_eq!(status.exit_code, Some(37));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native={native}: shell did not complete: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
        drop(lease);
    }
}

#[test]
#[ignore = "requires built helper path and real Windows ConPTY"]
fn conpty_target_output_exit_and_lifetime_cleanup() {
    let mut fixture = Fixture::new();
    assert!(fixture.call(Operation::Hello).error.is_none());
    let mut subscription = fixture.connect(Operation::Subscribe {
        capability: protocol::LIFECYCLE_CAPABILITY,
    });
    let snapshot: LifecycleSnapshot = fixture.receive(&mut subscription);
    assert_eq!(snapshot.capability, protocol::LIFECYCLE_CAPABILITY);
    drop(subscription);
    let lease = fixture.bind();
    let response = fixture.start("echo BMUX_CONPTY_CHILD_OK & exit /b 37");
    assert!(response.error.is_none(), "{:?}", response.error);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status = fixture.call(Operation::Status);
        if status.terminal_session_ended == Some(true) {
            let mut diagnostic = Vec::new();
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                match fixture.output.recv_timeout(Duration::from_millis(20)) {
                    Ok(bytes) if diagnostic.len() + bytes.len() <= 65_536 => {
                        diagnostic.extend(bytes)
                    }
                    Ok(_) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
            assert_eq!(
                status.exit_code,
                Some(37),
                "bounded child output: {:?}",
                String::from_utf8_lossy(&diagnostic)
            );
            assert!(
                String::from_utf8_lossy(&diagnostic).contains("BMUX_CONPTY_CHILD_OK"),
                "bounded child output: {:?}",
                String::from_utf8_lossy(&diagnostic)
            );
            break;
        }
        assert!(Instant::now() < deadline, "target did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(lease);
    let deadline = Instant::now() + TIMEOUT;
    while fixture.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "helper survived lease EOF");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut fixture = Fixture::new();
    let lease = fixture.bind();
    let response = fixture.start("ping -n 120 127.0.0.1 >nul");
    assert!(response.error.is_none(), "{:?}", response.error);
    let target =
        Handle::new(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, response.target_pid.unwrap()) })
            .unwrap();
    assert!(
        fixture
            .call(Operation::Term {
                mutation_id: uuid::Uuid::new_v4().to_string()
            })
            .error
            .unwrap()
            .contains("unsupported")
    );
    drop(lease);
    assert_eq!(
        unsafe { WaitForSingleObject(target.0, 5000) },
        WAIT_OBJECT_0,
        "job target survived lease EOF"
    );
    let deadline = Instant::now() + TIMEOUT;
    while fixture.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "helper survived lease EOF");
        std::thread::sleep(Duration::from_millis(20));
    }
}
