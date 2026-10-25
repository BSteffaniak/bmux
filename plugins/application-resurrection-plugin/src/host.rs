//! Durable, bounded local transport. Each socket belongs to one live pane launch.
use crate::execution::{Checkpoint, Execution};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
mod tests;

const PROTOCOL: &str = "application-resurrection/1";
const LIMIT: u64 = 16_384;

#[derive(Clone, Copy)]
pub enum IdentityCheck<'a> {
    Legacy,
    Expected(Option<&'a str>),
}

pub struct Host {
    root: PathBuf,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
}

struct Entry {
    endpoint: String,
    file: PathBuf,
    stopped: AtomicBool,
    state: Mutex<State>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommandPhase {
    AwaitingBoundary,
    Active,
}

struct State {
    execution: Execution,
    pid: Option<u32>,
    shell: bool,
    initial_prompt: bool,
    replay_start: bool,
    command_phase: CommandPhase,
    epoch: u64,
    claim: Option<u32>,
}

impl State {
    fn registration_ready(&self) -> bool {
        self.pid.is_some() && self.command_phase == CommandPhase::Active
    }
}

impl Host {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("resurrection storage root must be absolute".into());
        }
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            root,
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn prepare(
        &self,
        key: &str,
        restored: bool,
        shell: bool,
        replay: bool,
    ) -> Result<String, String> {
        self.prepare_checked(key, restored, shell, replay, IdentityCheck::Legacy)
            .map(|launch| launch.endpoint)
    }

    pub fn prepare_checked(
        &self,
        key: &str,
        restored: bool,
        shell: bool,
        replay: bool,
        identity: IdentityCheck<'_>,
    ) -> Result<
        bmux_application_resurrection_plugin_api::resurrection_launch_v2::PreparedLaunch,
        String,
    > {
        if key.is_empty()
            || key.len() > 100
            || !key.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return Err("invalid launch key".into());
        }
        let mut entries = self.entries.lock().map_err(|e| e.to_string())?;
        if entries.len() >= 1024 && !entries.contains_key(key) {
            return Err("resurrection launch limit reached".into());
        }
        if let Some(old) = entries.get(key) {
            stop_and_drain(old)?;
        }
        let file = self.root.join(format!("{key}.json"));
        let execution = if restored && (!shell || replay) {
            load_checkpoint(&file)?.map_or_else(
                || match identity {
                    IdentityCheck::Expected(Some(_)) => {
                        Err("pane references a missing resurrection checkpoint".into())
                    }
                    _ => Ok(Execution::manual()),
                },
                |checkpoint| {
                    if let IdentityCheck::Expected(expected) = identity
                        && expected != Some(checkpoint.invocation.to_string().as_str())
                    {
                        return Err(format!(
                            "resurrection checkpoint does not match pane invocation for launch {key}: \
                             pane expected {}, checkpoint contains {}; restore requires the matching \
                             pane snapshot and checkpoint; checkpoint preserved, resume refused",
                            expected.unwrap_or("<missing>"),
                            checkpoint.invocation,
                        ));
                    }
                    Execution::resurrect(checkpoint).map_err(str::to_owned)
                },
            )?
        } else {
            Execution::manual()
        };
        let invocation = execution.committed.invocation.to_string();
        save(&file, &execution.committed)?;
        // A short private directory avoids Unix socket path length limits, including on macOS.
        let directory = tempfile::Builder::new()
            .prefix("bmux-resume-")
            .tempdir_in("/tmp")
            .map_err(|e| e.to_string())?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let socket = directory.path().join("host");
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let endpoint = format!("v1:{}", socket.display());
        let entry = Arc::new(Entry {
            endpoint: endpoint.clone(),
            file,
            stopped: AtomicBool::new(false),
            state: Mutex::new(State {
                execution,
                pid: None,
                shell,
                initial_prompt: shell,
                replay_start: shell && replay,
                command_phase: if shell {
                    CommandPhase::AwaitingBoundary
                } else {
                    CommandPhase::Active
                },
                epoch: 0,
                claim: None,
            }),
        });
        entries.insert(key.to_owned(), Arc::clone(&entry));
        std::thread::spawn(move || {
            let _directory = directory;
            while !entry.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = serve(&entry, stream) {
                            tracing::debug!(%error, "resurrection connection rejected");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(
            bmux_application_resurrection_plugin_api::resurrection_launch_v2::PreparedLaunch {
                endpoint,
                invocation,
            },
        )
    }

    fn entry(&self, key: &str, endpoint: &str) -> Result<Arc<Entry>, String> {
        self.entries
            .lock()
            .map_err(|e| e.to_string())?
            .get(key)
            .filter(|entry| entry.endpoint == endpoint && !entry.stopped.load(Ordering::SeqCst))
            .cloned()
            .ok_or_else(|| "stale launch endpoint".into())
    }

    pub fn bind(&self, key: &str, endpoint: &str, pid: u32) -> Result<bool, String> {
        let entry = self.entry(key, endpoint)?;
        let mut state = entry.state.lock().map_err(|e| e.to_string())?;
        if entry.stopped.load(Ordering::SeqCst) || state.pid.is_some() || pid == 0 {
            return Err("launch already bound or invalid PID".into());
        }
        state.pid = Some(pid);
        Ok(true)
    }

    pub fn boundary(&self, key: &str, endpoint: &str, prompt: bool) -> Result<bool, String> {
        self.advance_boundary(key, endpoint, prompt)
            .map(|(changed, _)| changed)
    }

    pub fn boundary_identity(
        &self,
        key: &str,
        endpoint: &str,
        prompt: bool,
    ) -> Result<String, String> {
        self.advance_boundary(key, endpoint, prompt)
            .map(|(_, invocation)| invocation)
    }

    fn advance_boundary(
        &self,
        key: &str,
        endpoint: &str,
        prompt: bool,
    ) -> Result<(bool, String), String> {
        let entry = self.entry(key, endpoint)?;
        let mut state = entry.state.lock().map_err(|e| e.to_string())?;
        if entry.stopped.load(Ordering::SeqCst) {
            return Err("stale execution boundary".into());
        }
        if !state.shell {
            return Ok((false, state.execution.committed.invocation.to_string()));
        }
        if !prompt && state.initial_prompt {
            // Shell startup DEBUG/pre-exec hooks are not the replayed invocation.
            return Ok((false, state.execution.committed.invocation.to_string()));
        }
        if prompt && state.initial_prompt {
            state.command_phase = CommandPhase::AwaitingBoundary;
            state.initial_prompt = false;
            return Ok((true, state.execution.committed.invocation.to_string()));
        }
        state.command_phase = if prompt {
            CommandPhase::AwaitingBoundary
        } else {
            CommandPhase::Active
        };
        if !prompt && state.replay_start {
            state.replay_start = false;
            return Ok((true, state.execution.committed.invocation.to_string()));
        }
        state.initial_prompt = false;
        state.replay_start = false;
        let next = Execution::manual();
        if let Err(error) = save(&entry.file, &next.committed) {
            entry.stopped.store(true, Ordering::SeqCst);
            return Err(error);
        }
        state.execution.end();
        state.execution = next;
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or("execution epoch exhausted")?;
        state.claim = None;
        Ok((true, state.execution.committed.invocation.to_string()))
    }

    pub fn close(&self, key: &str, endpoint: &str) -> Result<bool, String> {
        let mut entries = self.entries.lock().map_err(|e| e.to_string())?;
        let entry = entries
            .get(key)
            .filter(|entry| entry.endpoint == endpoint)
            .ok_or("stale launch endpoint")?;
        stop_and_drain(entry)?;
        entries.remove(key);
        Ok(true)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Ok(entries) = self.entries.lock() {
            for entry in entries.values() {
                if let Err(error) = stop_and_drain(entry) {
                    tracing::warn!(%error, "failed draining resurrection writer during shutdown");
                }
            }
        }
    }
}

fn stop_and_drain(entry: &Entry) -> Result<(), String> {
    entry.stopped.store(true, Ordering::SeqCst);
    // Serialize with checkpoint replacement. A writer already holding this lock
    // finishes before the successor reads durable state; later writers are fenced.
    let mut state = entry.state.lock().map_err(|e| e.to_string())?;
    state.execution.end();
    Ok(())
}

fn load_checkpoint(path: &Path) -> Result<Option<Checkpoint>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read resurrection checkpoint: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 65_536 {
        return Err("oversized checkpoint".into());
    }
    let checkpoint: Checkpoint =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    checkpoint.validate().map_err(str::to_owned)?;
    Ok(Some(checkpoint))
}

fn save(path: &Path, checkpoint: &Checkpoint) -> Result<(), String> {
    let parent = path.parent().ok_or("missing checkpoint directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut temp, checkpoint).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path).map_err(|e| e.to_string())?;
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    protocol: String,
    application: String,
    pid: u32,
    accept_restore: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    operation: String,
    revision: u64,
    descriptor: Option<serde_json::Value>,
}

fn receive<T: serde::de::DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
) -> Result<T, String> {
    let mut bytes = Vec::new();
    reader
        .take(LIMIT)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 >= LIMIT || bytes.last() != Some(&b'\n') {
        return Err("invalid frame length".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
fn send(stream: &mut UnixStream, value: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 + 1 >= LIMIT {
        return Err("response too large".into());
    }
    stream
        .write_all(&bytes)
        .and_then(|()| stream.write_all(b"\n"))
        .map_err(|e| e.to_string())
}

fn configure_stream(stream: &UnixStream) -> Result<u32, String> {
    // Accepted sockets can inherit O_NONBLOCK on BSD/macOS. Framing below uses
    // bounded blocking reads; explicitly set the mode rather than race the hello.
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    peer_pid(stream)
}

fn receive_hello(reader: &mut BufReader<UnixStream>, peer: u32) -> Result<Hello, String> {
    let hello: Hello = receive(reader)?;
    if hello.protocol != PROTOCOL
        || hello.application.is_empty()
        || hello.application.len() > 128
        || peer != hello.pid
    {
        return Err("invalid protocol or peer identity".into());
    }
    Ok(hello)
}

fn wait_for_command_boundary(entry: &Entry) -> Result<(), String> {
    // Bind and shell metadata arrive independently of the application socket.
    // Acknowledge no application state until the shell's command boundary has
    // established its invocation; otherwise a delayed boundary erases that state.
    for _ in 0..100 {
        let ready = {
            let state = entry.state.lock().map_err(|e| e.to_string())?;
            state.registration_ready()
        };
        if ready {
            break;
        }
        if entry.stopped.load(Ordering::SeqCst) {
            return Err("launch stopped before command boundary".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

struct Rejection(&'static str);

struct Accepted {
    peer: u32,
    hello: Hello,
    registration: crate::execution::Registration,
    epoch: u64,
    offer: Option<String>,
}

fn accept(entry: &Entry, reader: &mut BufReader<UnixStream>) -> Result<Accepted, Rejection> {
    accept_with_inspector(entry, reader, belongs)
}

fn accept_with_inspector(
    entry: &Entry,
    reader: &mut BufReader<UnixStream>,
    inspect: impl FnOnce(u32, u32, bool) -> Result<bool, String>,
) -> Result<Accepted, Rejection> {
    let peer = configure_stream(reader.get_ref()).map_err(|_| Rejection("invalid_handshake"))?;
    let hello = receive_hello(reader, peer).map_err(|_| Rejection("invalid_handshake"))?;
    wait_for_command_boundary(entry).map_err(|_| Rejection("launch_unavailable"))?;
    let (registration, epoch, offer) = {
        let mut state = entry
            .state
            .lock()
            .map_err(|_| Rejection("launch_unavailable"))?;
        let root = state.pid.ok_or(Rejection("launch_unavailable"))?;
        if entry.stopped.load(Ordering::SeqCst)
            || state.command_phase != CommandPhase::Active
            || state.claim.is_some()
            || !inspect(peer, root, state.shell)
                .map_err(|_| Rejection("ownership_verification_unavailable"))?
        {
            return Err(Rejection("ownership_rejected"));
        }
        let registration = state
            .execution
            .register(uuid::Uuid::new_v4())
            .map_err(|_| Rejection("launch_unavailable"))?;
        let offer = if hello.accept_restore {
            state
                .execution
                .offer(registration)
                .map_err(|_| Rejection("launch_unavailable"))?
                .map(str::to_owned)
        } else {
            None
        };
        state.claim = Some(peer);
        (registration, state.epoch, offer)
    };
    Ok(Accepted {
        peer,
        hello,
        registration,
        epoch,
        offer,
    })
}

fn serve(entry: &Entry, stream: UnixStream) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let Accepted {
        peer,
        hello,
        registration,
        epoch,
        offer,
    } = match accept(entry, &mut reader) {
        Ok(accepted) => accepted,
        Err(Rejection(code)) => {
            // Send a bounded, public reason rather than private paths or raw OS errors.
            let _ = send(
                reader.get_mut(),
                &serde_json::json!({"protocol": PROTOCOL, "error": {"code": code}}),
            );
            return Err(format!("resurrection handshake rejected: {code}"));
        }
    };
    let offer = offer
        .map(|value| serde_json::from_str::<serde_json::Value>(&value))
        .transpose()
        .map_err(|e| e.to_string())?;
    send(
        reader.get_mut(),
        &serde_json::json!({"protocol": PROTOCOL, "restore": offer}),
    )?;
    // Poll readiness rather than timing out an idle application. Frame reads remain bounded.
    loop {
        if entry.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        if entry.state.lock().map_err(|e| e.to_string())?.epoch != epoch {
            return Ok(());
        }
        let mut poll = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll references one initialized pollfd for the duration of the call.
        if reader.buffer().is_empty() && unsafe { libc::poll(&raw mut poll, 1, 100) } == 0 {
            continue;
        }
        let update: Selection = receive(&mut reader)?;
        if update.operation != "selection" {
            return Err("unsupported operation".into());
        }
        if update.descriptor.as_ref().is_some_and(|value| {
            value.get("application").and_then(serde_json::Value::as_str)
                != Some(hello.application.as_str())
        }) {
            return Err("application mismatch".into());
        }
        let descriptor = update
            .descriptor
            .map(|value| serde_json::to_string(&value))
            .transpose()
            .map_err(|e| e.to_string())?;
        let mut state = entry.state.lock().map_err(|e| e.to_string())?;
        if state.epoch != epoch
            || entry.stopped.load(Ordering::SeqCst)
            || !belongs(peer, state.pid.ok_or("unbound")?, state.shell)?
        {
            return Err("stale execution".into());
        }
        let candidate = state
            .execution
            .update(registration, update.revision, descriptor)
            .map_err(str::to_owned)?;
        if let Err(error) = save(&entry.file, &candidate) {
            entry.stopped.store(true, Ordering::SeqCst);
            return Err(error);
        }
        state
            .execution
            .committed(candidate)
            .map_err(str::to_owned)?;
        send(
            reader.get_mut(),
            &serde_json::json!({"revision": update.revision}),
        )?;
    }
}

fn belongs(peer: u32, root: u32, shell: bool) -> Result<bool, String> {
    if !shell {
        return Ok(peer == root);
    }
    // NixOS provides ps through PATH rather than /bin. macOS retains its system tool.
    let program = if cfg!(target_os = "macos") {
        "/bin/ps"
    } else {
        "ps"
    };
    foreground_owner(peer, root, program)
}

fn foreground_owner(peer: u32, root: u32, program: &str) -> Result<bool, String> {
    // Parentage alone would allow an unrelated background job to claim recovery.
    let output = std::process::Command::new(program)
        .args(["-o", "ppid=,pgid=,tpgid=", "-p", &peer.to_string()])
        .output()
        .map_err(|error| format!("foreground ownership inspection unavailable: {error}"))?;
    if !output.status.success() {
        return Err("foreground ownership inspection failed".into());
    }
    let identity = String::from_utf8(output.stdout)
        .map_err(|_| "invalid foreground ownership inspection output".to_owned())?;
    Ok(foreground_child(&identity, root))
}

fn foreground_child(identity: &str, root: u32) -> bool {
    let mut fields = identity.split_whitespace();
    let parent = fields.next().and_then(|value| value.parse::<u32>().ok());
    let group = fields.next().and_then(|value| value.parse::<u32>().ok());
    let foreground = fields.next().and_then(|value| value.parse::<u32>().ok());
    parent == Some(root)
        && group.is_some_and(|group| group > 0 && Some(group) == foreground)
        && fields.next().is_none()
}

fn peer_pid(stream: &UnixStream) -> Result<u32, String> {
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: both output pointers reference valid initialized credential storage.
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
        // SAFETY: geteuid has no arguments or memory-safety preconditions.
        if result != 0 || uid != unsafe { libc::geteuid() } {
            return Err("resurrection peer user identity mismatch".into());
        }
        let mut pid: libc::pid_t = 0;
        let mut length = u32::try_from(std::mem::size_of_val(&pid)).map_err(|e| e.to_string())?;
        // SAFETY: buffer and length point to valid initialized storage of the expected size.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        u32::try_from(pid).map_err(|e| e.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: all-zero is a valid initial representation for integer-only ucred;
        // getsockopt fills it before its fields are used.
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length =
            u32::try_from(std::mem::size_of_val(&credentials)).map_err(|e| e.to_string())?;
        // SAFETY: credentials and length are valid buffers for SO_PEERCRED.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: geteuid has no arguments or memory-safety preconditions.
        if credentials.uid != unsafe { libc::geteuid() } {
            return Err("resurrection peer user identity mismatch".into());
        }
        u32::try_from(credentials.pid).map_err(|e| e.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        Err("peer PID authentication unsupported".into())
    }
}
