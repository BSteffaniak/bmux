//! Single-threaded bounded, owner-only named-pipe control transport.
//! The dedicated authenticated lease is the helper's lifetime authority; losing
//! ordinary status/subscription peers never terminates an execution.
use super::windows_job::Scope;
use super::windows_security::{Handle, Security, wide};
use crate::protocol::{self, Bootstrap, LifecycleSnapshot, Operation, Request, Response};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GENERIC_READ,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, GetFileInformationByHandle, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PeekNamedPipe,
};

const MAX_PEERS: usize = 16;
const TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
struct Peer {
    handle: Handle,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    deadline: Instant,
    mode: Mode,
}
#[derive(PartialEq)]
enum Mode {
    Request,
    Reply,
    Subscription,
    Lifetime,
}
impl Peer {
    fn new(handle: Handle) -> Self {
        Self {
            handle,
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
            deadline: Instant::now() + TIMEOUT,
            mode: Mode::Request,
        }
    }
    fn send(&mut self, value: &impl serde::Serialize) -> io::Result<()> {
        if self.written != self.output.len() {
            return Err(io::Error::other("slow supervisor peer"));
        }
        self.output.clear();
        self.written = 0;
        protocol::write_frame(&mut self.output, value)?;
        self.deadline = Instant::now() + TIMEOUT;
        Ok(())
    }
    fn write(&mut self) -> io::Result<()> {
        if self.written == self.output.len() {
            return Ok(());
        }
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "slow pipe reader"));
        }
        let remaining = &self.output[self.written..];
        let mut count = 0;
        // SAFETY: pipe is nonblocking, slice remains valid for this synchronous call.
        if unsafe {
            WriteFile(
                self.handle.0,
                remaining.as_ptr(),
                remaining.len() as u32,
                &mut count,
                null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_DATA as i32) {
                return Ok(());
            }
            return Err(error);
        }
        self.written += count as usize;
        Ok(())
    }
    fn read(&mut self) -> io::Result<Option<Request>> {
        let mut available = 0;
        // Peek distinguishes a connected idle nonblocking pipe from EOF. A
        // zero-byte nonblocking ReadFile alone does not establish lease loss.
        if unsafe {
            PeekNamedPipe(
                self.handle.0,
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if available == 0 {
            return Ok(None);
        }
        let mut bytes = [0u8; 8192];
        let mut count = 0;
        if unsafe {
            ReadFile(
                self.handle.0,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut count,
                null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_DATA as i32) {
                return Ok(None);
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "pipe peer closed",
            ));
        }
        if self.mode != Mode::Request {
            return Err(io::Error::other(
                "dedicated connection must carry no further frames",
            ));
        }
        if self.input.len() + count as usize > protocol::MAX_FRAME + 4 {
            return Err(io::Error::other("supervisor frame exceeds limit"));
        }
        self.input.extend_from_slice(&bytes[..count as usize]);
        if self.input.len() < 4 {
            return Ok(None);
        }
        let length =
            u32::from_be_bytes(self.input[..4].try_into().map_err(io::Error::other)?) as usize;
        if length > protocol::MAX_FRAME {
            return Err(io::Error::other("supervisor frame exceeds limit"));
        }
        if self.input.len() < length + 4 {
            return Ok(None);
        }
        if self.input.len() != length + 4 {
            return Err(io::Error::other("one request per control connection"));
        }
        protocol::read_frame(&mut self.input.as_slice()).map(Some)
    }
}

fn listener(name: &[u16], security: &Security, first: bool) -> io::Result<Handle> {
    let attributes = security.attributes();
    // PIPE_REJECT_REMOTE_CLIENTS plus a protected current-user-only DACL enforce
    // local same-user access, independently of the protocol's secret check.
    Handle::new(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            (MAX_PEERS + 1) as u32,
            (protocol::MAX_FRAME + 4) as u32,
            (protocol::MAX_FRAME + 4) as u32,
            0,
            &attributes,
        )
    })
}

fn bootstrap(path: &str, security: &Security) -> io::Result<Bootstrap> {
    let path = wide(OsStr::new(path))?;
    let handle = Handle::new(unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    })?;
    security.check_file(handle.0)?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || information.nFileSizeHigh != 0
        || information.nFileSizeLow as usize > protocol::MAX_FRAME
    {
        return Err(io::Error::other(
            "bootstrap is not a bounded regular non-reparse file",
        ));
    }
    let Handle(raw) = handle;
    // SAFETY: transfer sole ownership from our wrapper into File.
    std::mem::forget(handle);
    let mut file = unsafe { std::fs::File::from_raw_handle(raw) };
    let mut bytes = Vec::new();
    (&mut file)
        .take((protocol::MAX_FRAME + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > protocol::MAX_FRAME {
        return Err(io::Error::other("bootstrap exceeds limit"));
    }
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if bootstrap.version != protocol::VERSION
        || bootstrap.authentication.len() < 32
        || uuid::Uuid::parse_str(&bootstrap.execution_token).is_err()
    {
        return Err(io::Error::other(
            "unsupported or invalid supervisor bootstrap",
        ));
    }
    Ok(bootstrap)
}

struct Receipt {
    fingerprint: Vec<u8>,
    response: Response,
}
struct Server {
    bootstrap: Bootstrap,
    scope: Scope,
    receipts: BTreeMap<uuid::Uuid, Receipt>,
    receipt_bytes: usize,
    lease: bool,
    retired: bool,
    revision: u64,
    last_status: Vec<u8>,
}
impl Server {
    fn authenticated(&self, request: &Request) -> bool {
        // Fixed-size random secret; do not short-circuit its byte comparison.
        let expected = self.bootstrap.authentication.as_bytes();
        let actual = request.authentication.as_bytes();
        let mut difference = expected.len() ^ actual.len();
        for (index, byte) in expected.iter().enumerate() {
            difference |= usize::from(*byte ^ actual.get(index).copied().unwrap_or(0));
        }
        request.version == protocol::VERSION
            && request.execution_token == self.bootstrap.execution_token
            && difference == 0
    }
    fn snapshot(&mut self) -> io::Result<LifecycleSnapshot> {
        let bytes = serde_json::to_vec(&self.scope.status).map_err(io::Error::other)?;
        if bytes != self.last_status {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("lifecycle revision exhausted"))?;
            self.last_status = bytes;
        }
        Ok(LifecycleSnapshot {
            capability: protocol::LIFECYCLE_CAPABILITY,
            revision: self.revision,
            status: self.scope.status.clone(),
        })
    }
    fn operate(&mut self, operation: &Operation) -> io::Result<Response> {
        let mutation = match operation {
            Operation::Start { mutation_id, .. }
            | Operation::Sweep { mutation_id }
            | Operation::Term { mutation_id }
            | Operation::RetireOwner { mutation_id } => mutation_id,
            _ => return Ok(self.scope.status.clone()),
        };
        let mutation = uuid::Uuid::parse_str(mutation).map_err(io::Error::other)?;
        let fingerprint = serde_json::to_vec(operation).map_err(io::Error::other)?;
        if let Some(receipt) = self.receipts.get(&mutation) {
            if receipt.fingerprint != fingerprint {
                return Err(io::Error::other("conflicting mutation receipt"));
            }
            return Ok(receipt.response.clone());
        }
        // The helper cannot be restarted: its job handle is the execution scope.
        // In-memory receipts survive connection loss for exactly that lifetime.
        if self.receipts.len() >= 128 || self.receipt_bytes + fingerprint.len() + 8192 > 1_048_576 {
            return Err(io::Error::other("mutation receipt capacity exhausted"));
        }
        let result = match operation {
            Operation::Start { .. } if !self.lease || self.retired => {
                Err(io::Error::other("Start requires an active lifetime lease"))
            }
            Operation::Start { .. } => self.scope.start(operation),
            Operation::Term { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows graceful Term unsupported; use owned-job Sweep",
            )),
            Operation::Sweep { .. } => self.scope.sweep(),
            Operation::RetireOwner { .. }
                if self.scope.status.terminal_session_ended != Some(true) =>
            {
                Err(io::Error::other("cannot retire a nonempty execution scope"))
            }
            Operation::RetireOwner { .. } => {
                self.retired = true;
                Ok(())
            }
            _ => unreachable!(),
        };
        let mut response = self.scope.status.clone();
        response.error = result.err().map(|error| error.to_string());
        self.receipt_bytes += fingerprint.len()
            + serde_json::to_vec(&response)
                .map_err(io::Error::other)?
                .len();
        self.receipts.insert(
            mutation,
            Receipt {
                fingerprint,
                response: response.clone(),
            },
        );
        Ok(response)
    }
}

pub(super) fn run(path: &str, pipe: &str) -> io::Result<()> {
    let suffix = pipe
        .strip_prefix(r"\\.\pipe\bmux-exec-")
        .ok_or_else(|| io::Error::other("invalid private pipe name"))?;
    uuid::Uuid::parse_str(suffix).map_err(io::Error::other)?;
    let security = Security::current_user()?;
    let bootstrap = bootstrap(path, &security)?;
    let scope = Scope::new(bootstrap.execution_token.clone())?;
    let mut server = Server {
        bootstrap,
        scope,
        receipts: BTreeMap::new(),
        receipt_bytes: 0,
        lease: false,
        retired: false,
        revision: 0,
        last_status: Vec::new(),
    };
    let name = wide(OsStr::new(pipe))?;
    let mut accepting = listener(&name, &security, true)?;
    let mut peers: Vec<Peer> = Vec::new();
    let bind_deadline = Instant::now() + Duration::from_secs(15);
    let mut published_revision = 0;
    loop {
        // Every return drops the sole job handle, including framing/IO errors,
        // lease EOF, failed readiness and unexpected internal failures.
        if !server.lease && Instant::now() >= bind_deadline {
            return Err(io::Error::other("lifetime lease not bound"));
        }
        if peers.len() < MAX_PEERS {
            let connected = unsafe { ConnectNamedPipe(accepting.0, null_mut()) };
            // With PIPE_NOWAIT the first successful call only makes the instance
            // available. A later ERROR_PIPE_CONNECTED proves an actual connection.
            let error = if connected != 0 {
                ERROR_PIPE_LISTENING
            } else {
                io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
            };
            match error {
                ERROR_PIPE_CONNECTED => {
                    // Keep an instance open continuously: a foreign server must not
                    // be able to squat the name between authenticated clients.
                    let next = listener(&name, &security, false)?;
                    let connected = std::mem::replace(&mut accepting, next);
                    if peers.len() < MAX_PEERS {
                        peers.push(Peer::new(connected));
                    }
                }
                ERROR_NO_DATA => {
                    let next = listener(&name, &security, false)?;
                    accepting = next;
                }
                ERROR_PIPE_LISTENING => {}
                _ => return Err(io::Error::from_raw_os_error(error as i32)),
            }
        }
        // Check the lifetime authority before admitting any mutation, even if
        // an earlier connected rogue/status peer is stalled or a Start is ready.
        if let Some(lease) = peers.iter_mut().find(|peer| peer.mode == Mode::Lifetime)
            && let Err(error) = lease.read()
        {
            let _ = server.scope.sweep();
            return Err(error);
        }
        if let Err(error) = server.scope.refresh() {
            server.scope.status.cleanup_error = Some(error.to_string());
        }
        let snapshot = server.snapshot()?;
        let changed = snapshot.revision != published_revision;
        published_revision = snapshot.revision;
        let mut index = 0;
        while index < peers.len() {
            let peer = &mut peers[index];
            let result = (|| -> io::Result<bool> {
                peer.write()?;
                // Keep a reply instance alive until the client closes it: on
                // Windows closing a server handle can discard unread buffered
                // bytes. Do not block the lifetime loop in FlushFileBuffers.
                if matches!(peer.mode, Mode::Request | Mode::Reply)
                    && Instant::now() >= peer.deadline
                {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "request deadline elapsed",
                    ));
                }
                if let Some(request) = peer.read()? {
                    if !server.authenticated(&request) {
                        return Err(io::Error::other("supervisor authentication failed"));
                    }
                    match request.operation {
                        Operation::BindLifetime if !server.lease => {
                            server.lease = true;
                            peer.mode = Mode::Lifetime;
                            peer.send(&server.scope.status)?;
                        }
                        Operation::BindLifetime => {
                            return Err(io::Error::other("lifetime already bound"));
                        }
                        Operation::Subscribe { capability } => {
                            if capability != protocol::LIFECYCLE_CAPABILITY {
                                return Err(io::Error::other("unsupported lifecycle capability"));
                            }
                            peer.mode = Mode::Subscription;
                            peer.send(&snapshot)?;
                        }
                        operation => {
                            let response = server.operate(&operation).unwrap_or_else(|error| {
                                let mut response = server.scope.status.clone();
                                response.error = Some(error.to_string());
                                response
                            });
                            peer.mode = Mode::Reply;
                            peer.send(&response)?;
                        }
                    }
                } else if peer.mode == Mode::Subscription && changed {
                    peer.send(&snapshot)?;
                }
                Ok(true)
            })();
            if !matches!(result, Ok(true)) {
                if peer.mode == Mode::Lifetime {
                    // TerminateJobObject is best effort; dropping the final job
                    // handle is the unconditional kill-on-close safety net.
                    let _ = server.scope.sweep();
                    return result.map(|_| ());
                }
                peers.swap_remove(index);
            } else {
                index += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
