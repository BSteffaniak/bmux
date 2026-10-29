//! Windows named-pipe client. Synchronous framing uses overlapped handles so
//! deadlines and lifecycle interruption cancel the actual outstanding kernel IO.
#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
use super::{Operation, Request, Response, SupervisorControl, protocol};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE,
    INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};

pub(super) const HELPER_ARGUMENT: &str = "--guardian-windows-session-v1";
pub(super) const NATIVE_ENV_ENCODING: &str = "windows-utf16le";

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

/// Created atomically with a protected DACL containing only the current user.
/// No credential is written into an ambient/shared-permission directory.
pub(super) struct EndpointDirectory(Option<PathBuf>);
impl EndpointDirectory {
    pub(super) fn path(&self) -> &Path {
        self.0.as_deref().expect("owned bootstrap directory")
    }
    pub(super) fn keep(mut self) -> PathBuf {
        self.0.take().expect("owned bootstrap directory")
    }
}
impl Drop for EndpointDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: allocated by the Windows LocalAlloc-backed conversion APIs.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn private_security_descriptor(directory: bool) -> anyhow::Result<LocalAllocation> {
    // SAFETY: all output pointers refer to initialized, live storage; handles and
    // LocalAlloc results are adopted immediately and retained through the call.
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut needed = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
        );
        anyhow::ensure!(needed > 0, "current user token unavailable");
        // usize storage supplies TOKEN_USER pointer alignment.
        let mut storage = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut sid = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &raw mut sid) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let _sid = LocalAllocation(sid.cast());
        let mut length = 0;
        while *sid.add(length) != 0 {
            length += 1;
        }
        let sid_string = String::from_utf16(std::slice::from_raw_parts(sid, length))?;
        let inheritance = if directory { "OICI" } else { "" };
        let sddl = wide(std::ffi::OsStr::new(&format!(
            "O:{sid_string}D:P(A;{inheritance};FA;;;{sid_string})"
        )));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(LocalAllocation(descriptor))
    }
}

fn security_attributes(descriptor: &LocalAllocation) -> anyhow::Result<SECURITY_ATTRIBUTES> {
    Ok(SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    })
}

pub(super) fn short_endpoint_directory() -> anyhow::Result<EndpointDirectory> {
    let descriptor = private_security_descriptor(true)?;
    let attributes = security_attributes(&descriptor)?;
    let path = std::env::temp_dir().join(format!("bmux-exec-{}", uuid::Uuid::new_v4()));
    let native_path = wide(path.as_os_str());
    // SAFETY: terminated name and owned security descriptor live through creation.
    if unsafe { CreateDirectoryW(native_path.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(EndpointDirectory(Some(path)))
}
pub(super) fn endpoint(_directory: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\bmux-exec-{}", uuid::Uuid::new_v4()))
}
pub(super) fn secure_bootstrap(
    directory: &Path,
    bootstrap: &protocol::Bootstrap,
) -> anyhow::Result<()> {
    // Inheritance alone neither pins TokenUser ownership for elevated callers
    // nor protects the leaf DACL. Supply both at CREATE_NEW: no broad window,
    // no post-creation repair, and no replacement of an existing credential.
    let descriptor = private_security_descriptor(false)?;
    let attributes = security_attributes(&descriptor)?;
    let path = wide(directory.join("bootstrap.json").as_os_str());
    // SAFETY: all pointers remain valid; a successful noninheritable handle is
    // adopted once and closes after the complete credential has been flushed.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            &raw const attributes,
            CREATE_NEW,
            0,
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(
        handle != INVALID_HANDLE_VALUE,
        "create private supervisor bootstrap: {}",
        io::Error::last_os_error()
    );
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    file.write_all(&serde_json::to_vec(bootstrap)?)?;
    file.sync_all()?;
    Ok(())
}

struct Pipe {
    handle: OwnedHandle,
    interrupted: AtomicBool,
}
pub(super) struct Stream {
    pipe: Arc<Pipe>,
    read_deadline: Mutex<Option<Instant>>,
    write_deadline: Mutex<Option<Instant>>,
}
pub(super) fn connect(endpoint: &Path, timeout: Duration) -> io::Result<Stream> {
    connect_while(endpoint, timeout, || Ok(()))
}

fn connect_while(
    endpoint: &Path,
    timeout: Duration,
    mut ensure_alive: impl FnMut() -> io::Result<()>,
) -> io::Result<Stream> {
    let deadline = Instant::now() + timeout;
    let endpoint = wide(endpoint.as_os_str());
    loop {
        ensure_alive()?;
        // SAFETY: endpoint is NUL terminated. The noninheritable handle is adopted
        // once; overlapped IO owns its event and buffer until completion/cancellation.
        let handle = unsafe {
            CreateFileW(
                endpoint.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(Stream {
                pipe: Arc::new(Pipe {
                    // SAFETY: successful CreateFileW returns an owned handle.
                    handle: unsafe { OwnedHandle::from_raw_handle(handle) },
                    interrupted: AtomicBool::new(false),
                }),
                read_deadline: Mutex::new(Some(deadline)),
                write_deadline: Mutex::new(Some(deadline)),
            });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok()),
            Some(ERROR_PIPE_BUSY | ERROR_FILE_NOT_FOUND)
        ) {
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}
impl Stream {
    pub(super) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        *self
            .read_deadline
            .lock()
            .map_err(|_| io::Error::other("pipe deadline lock poisoned"))? =
            timeout.map(|timeout| Instant::now() + timeout);
        Ok(())
    }
    fn transfer(&self, buffer: *mut u8, length: usize, write: bool) -> io::Result<usize> {
        if self.pipe.interrupted.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let deadline = *if write {
            &self.write_deadline
        } else {
            &self.read_deadline
        }
        .lock()
        .map_err(|_| io::Error::other("pipe deadline lock poisoned"))?;
        let wait = match deadline {
            None => INFINITE,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                u32::try_from(remaining.as_millis())
                    .unwrap_or(INFINITE - 1)
                    .max(1)
            }
        };
        let length = u32::try_from(length).map_err(io::Error::other)?;
        // SAFETY: transfer does not return until this operation completes, including
        // cancellation. The caller's buffer and stack OVERLAPPED therefore remain live.
        unsafe {
            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = OwnedHandle::from_raw_handle(event);
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            overlapped.hEvent = event.as_raw_handle();
            let handle = self.pipe.handle.as_raw_handle();
            let mut transferred = 0;
            let result = if write {
                WriteFile(
                    handle,
                    buffer.cast_const(),
                    length,
                    &raw mut transferred,
                    &raw mut overlapped,
                )
            } else {
                ReadFile(
                    handle,
                    buffer,
                    length,
                    &raw mut transferred,
                    &raw mut overlapped,
                )
            };
            if result != 0 {
                return Ok(transferred as usize);
            }
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                != Some(ERROR_IO_PENDING)
            {
                return Err(error);
            }
            // Close the cancellation-before-issuance race: interruption is sticky.
            let interrupted = self.pipe.interrupted.load(Ordering::SeqCst);
            let outcome = if interrupted {
                WAIT_TIMEOUT
            } else {
                WaitForSingleObject(event.as_raw_handle(), wait)
            };
            if outcome != WAIT_OBJECT_0 {
                CancelIoEx(handle, &raw const overlapped);
                GetOverlappedResult(handle, &raw const overlapped, &raw mut transferred, 1);
                return Err(
                    if interrupted || self.pipe.interrupted.load(Ordering::SeqCst) {
                        io::ErrorKind::Interrupted.into()
                    } else if outcome == WAIT_TIMEOUT {
                        io::ErrorKind::TimedOut.into()
                    } else {
                        io::Error::last_os_error()
                    },
                );
            }
            if GetOverlappedResult(handle, &raw const overlapped, &raw mut transferred, 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(transferred as usize)
        }
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.transfer(buffer.as_mut_ptr(), buffer.len(), false)
    }
}
impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.transfer(buffer.as_ptr().cast_mut(), buffer.len(), true)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) struct Interrupter(Arc<Pipe>);
impl Interrupter {
    pub(super) fn new(stream: &Stream) -> io::Result<Self> {
        // Refuse a non-pipe handle rather than installing a cancellation owner
        // that cannot revoke the transport it promises to control.
        let mut flags = 0;
        if unsafe {
            windows_sys::Win32::System::Pipes::GetNamedPipeInfo(
                stream.pipe.handle.as_raw_handle(),
                &raw mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Arc::clone(&stream.pipe)))
    }
    pub(super) fn interrupt(&self) -> io::Result<()> {
        self.0.interrupted.store(true, Ordering::SeqCst);
        // SAFETY: handle remains owned by this Arc; a null OVERLAPPED cancels all
        // pending IO. ERROR_NOT_FOUND is benign (sticky flag covers the next read).
        let result = unsafe { CancelIoEx(self.0.handle.as_raw_handle(), std::ptr::null()) };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                != Some(windows_sys::Win32::Foundation::ERROR_NOT_FOUND)
            {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Lifetime {
    revoked: bool,
    socket: Option<Stream>,
}
impl Lifetime {
    pub(super) fn revoke(&mut self) {
        self.revoked = true;
        self.socket.take();
    }
    fn install(&mut self, socket: Stream) -> anyhow::Result<()> {
        anyhow::ensure!(!self.revoked, "execution lifetime already revoked");
        anyhow::ensure!(
            self.socket.is_none(),
            "execution lifetime already installed"
        );
        self.socket = Some(socket);
        Ok(())
    }
}
/// Own the helper until authentication and lifetime binding both succeed.
struct StartupChild(Option<Box<dyn portable_pty::Child + Send + Sync>>);
impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let deadline = Instant::now() + Duration::from_millis(250);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => break,
                }
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}
impl SupervisorControl {
    pub(in crate::runtime) fn spawn_terminal(
        &self,
        _master: &dyn portable_pty::MasterPty,
        slave: &dyn portable_pty::SlavePty,
        command: portable_pty::CommandBuilder,
    ) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        anyhow::ensure!(
            self.version == protocol::VERSION,
            "guardian protocol mismatch"
        );
        let expected = vec![
            self.artifact.as_os_str().to_os_string(),
            std::ffi::OsString::from(HELPER_ARGUMENT),
            self.directory.join("bootstrap.json").into_os_string(),
            self.endpoint.as_os_str().to_os_string(),
        ];
        anyhow::ensure!(
            command.get_argv() == &expected,
            "unexpected guardian command"
        );
        // The helper itself is attached to ConPTY. It creates the application only
        // after Start, in its owned kill-on-close Job Object.
        let mut child = StartupChild(Some(slave.spawn_command(command)?));
        progress("native-child-spawned");
        let mut socket = connect_while(&self.endpoint, Duration::from_secs(10), || {
            let status = child.0.as_mut().expect("startup owns helper").try_wait()?;
            if let Some(status) = status {
                return Err(io::Error::other(format!(
                    "supervisor exited before pipe readiness: {status}"
                )));
            }
            Ok(())
        })?;
        progress("pipe-connected");
        protocol::write_frame(
            &mut socket,
            &Request {
                version: self.version,
                execution_token: self.token.clone(),
                authentication: self.authentication.clone(),
                operation: Operation::BindLifetime,
            },
        )?;
        progress("lifetime-request-written");
        let response: Response = protocol::read_frame(&mut socket)?;
        progress("lifetime-response-read");
        self.validate_response(&response)?;
        // Only a successful authenticated Response establishes the lease; closing
        // this dedicated connection thereafter instructs the helper to clean its job.
        self.lifetime
            .lock()
            .map_err(|_| anyhow::anyhow!("supervisor lifetime lock poisoned"))?
            .install(socket)?;
        progress("lifetime-installed");
        Ok(child.0.take().expect("negotiated guardian child"))
    }
}

// Closed vocabulary only: no endpoint, command, bootstrap, or credentials.
fn progress(stage: &'static str) {
    tracing::debug!(stage, "supervisor bootstrap progress");
    #[cfg(test)]
    eprintln!("supervisor bootstrap stage={stage}");
}

#[cfg(test)]
mod bootstrap_security_tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, EqualSid,
        GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };

    #[test]
    fn windows_bootstrap_is_created_with_explicit_owner_and_protected_private_dacl() {
        let directory = short_endpoint_directory().unwrap();
        let bootstrap = protocol::Bootstrap {
            version: protocol::VERSION,
            execution_token: uuid::Uuid::new_v4().to_string(),
            authentication: "fixture-not-a-production-secret".repeat(2),
        };
        secure_bootstrap(directory.path(), &bootstrap).unwrap();
        assert!(secure_bootstrap(directory.path(), &bootstrap).is_err());
        let file = std::fs::File::open(directory.path().join("bootstrap.json")).unwrap();
        let expected = private_security_descriptor(false).unwrap();
        // SAFETY: owned file, live descriptors and initialized output pointers;
        // GetSecurityInfo allocation is adopted before assertions can unwind.
        unsafe {
            let mut owner = std::ptr::null_mut();
            let mut dacl = std::ptr::null_mut();
            let mut raw = std::ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &raw mut owner,
                    std::ptr::null_mut(),
                    &raw mut dacl,
                    std::ptr::null_mut(),
                    &raw mut raw
                ),
                0
            );
            let actual = LocalAllocation(raw);
            let mut expected_owner = std::ptr::null_mut();
            let mut defaulted = 0;
            assert_ne!(
                GetSecurityDescriptorOwner(expected.0, &raw mut expected_owner, &raw mut defaulted),
                0
            );
            assert_ne!(EqualSid(owner, expected_owner), 0);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(actual.0, &raw mut control, &raw mut revision),
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
            assert_ne!(
                GetAclInformation(
                    dacl,
                    (&raw mut info).cast(),
                    u32::try_from(std::mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap(),
                    AclSizeInformation
                ),
                0
            );
            assert_eq!(info.AceCount, 1);
            let mut ace = std::ptr::null_mut();
            assert_ne!(
                windows_sys::Win32::Security::GetAce(dacl, 0, &raw mut ace),
                0
            );
            let ace = &*ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0);
            assert_ne!(
                EqualSid((&raw const ace.SidStart).cast_mut().cast(), expected_owner),
                0
            );
        }
    }

    #[test]
    fn windows_pipe_readiness_reports_helper_exit_without_waiting_for_deadline() {
        let endpoint = endpoint(Path::new("unused"));
        let mut checked = 0;
        let result = connect_while(&endpoint, Duration::from_secs(10), || {
            checked += 1;
            Err(io::Error::other("helper exited"))
        });
        assert!(result.is_err());
        assert_eq!(checked, 1);
    }
}
