//! Pure-Rust X11 selection transport (x11rb 0.13.2, no optional features).
//! Stream contracts reviewed against cached `rust_connection/stream.rs`: reads
//! and writes are nonblocking; poll is deadline-bound by `DeadlineStream`.
use super::{LIMIT, failure};
use crate::ClipboardError;
use std::{
    collections::HashMap,
    ffi::c_int,
    io::{self, IoSlice},
    os::fd::AsRawFd,
    sync::{Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};
use x11rb::reexports::x11rb_protocol::{parse_display::parse_display, xauth::get_auth};
use x11rb::{
    connection::Connection,
    cookie::VoidCookie,
    protocol::xproto::{
        ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
        WindowClass,
    },
    rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
    utils::RawFdContainer,
};

const TIMEOUT: Duration = Duration::from_secs(4);
const TEXT: [&str; 6] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "TEXT",
    "STRING",
];
struct DeadlineStream {
    inner: DefaultStream,
    deadline: Mutex<Instant>,
}
impl DeadlineStream {
    fn arm(&self) {
        *self.deadline.lock().expect("thread-confined deadline") = Instant::now() + TIMEOUT;
    }
    fn check_deadline(&self) -> io::Result<()> {
        if Instant::now() >= *self.deadline.lock().expect("thread-confined deadline") {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 I/O deadline exceeded",
            ));
        }
        Ok(())
    }
}
impl Stream for DeadlineStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        loop {
            self.check_deadline()?;
            let mut poll = libc::pollfd {
                fd: self.inner.as_raw_fd(),
                events: (if mode.readable() { libc::POLLIN } else { 0 })
                    | (if mode.writable() { libc::POLLOUT } else { 0 }),
                revents: 0,
            };
            let ready = unsafe { libc::poll(&raw mut poll, 1, 10) };
            if ready > 0 {
                return Ok(());
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.read(buf, fds)
    }
    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.write(buf, fds)
    }
    fn write_vectored(
        &self,
        buf: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.check_deadline()?;
        self.inner.write_vectored(buf, fds)
    }
}
fn native_error(error: impl std::fmt::Display) -> ClipboardError {
    failure("native-x11", error.to_string())
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed protocol field"),
    )
}
fn put(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}
struct X {
    connection: RustConnection<DeadlineStream>,
    window: u32,
    atoms: HashMap<&'static str, u32>,
    chunk: usize,
    content: Option<(Vec<u8>, bool)>,
    sends: HashMap<(u32, u32), Send>,
}
struct Send {
    bytes: Vec<u8>,
    offset: usize,
    target: u32,
    expires: Instant,
    deadline: Instant,
}
impl Send {
    fn acknowledged_chunk(&self, chunk: usize) -> std::ops::Range<usize> {
        self.offset..self.offset.saturating_add(chunk).min(self.bytes.len())
    }
    fn acknowledge_written(&mut self, end: usize) -> bool {
        // After the final nonempty chunk, retain state for one more delete:
        // ICCCM requires a typed zero-length property as the terminator.
        if self.offset == self.bytes.len() {
            return true;
        }
        self.offset = end;
        self.expires = Instant::now() + TIMEOUT;
        false
    }
}
type Authorization = (Vec<u8>, Vec<u8>);
fn auth_or_empty(result: io::Result<Option<Authorization>>) -> Authorization {
    result.unwrap_or(None).unwrap_or_default()
}
impl X {
    fn new() -> Result<Self, ClipboardError> {
        let parsed = parse_display(None).map_err(native_error)?;
        let screen = usize::from(parsed.screen);
        let mut connected = None;
        let mut last_error = None;
        for address in parsed.connect_instruction() {
            match DefaultStream::connect(&address) {
                Ok((stream, (family, address))) => {
                    // Match RustConnection::connect: authority discovery is
                    // advisory. Missing/unreadable authority must still allow
                    // an unauthenticated setup attempt; the server remains the
                    // authority and rejects it if credentials are required.
                    let (name, data) = auth_or_empty(get_auth(family, &address, parsed.display));
                    let stream = DeadlineStream {
                        inner: stream,
                        deadline: Mutex::new(Instant::now() + TIMEOUT),
                    };
                    connected = Some(
                        RustConnection::connect_to_stream_with_auth_info(
                            stream, screen, name, data,
                        )
                        .map_err(native_error)?,
                    );
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let connection = connected.ok_or_else(|| {
            native_error(last_error.map_or_else(
                || "no DISPLAY address".to_string(),
                |error| error.to_string(),
            ))
        })?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| native_error("missing screen"))?
            .root;
        let window = connection.generate_id().map_err(native_error)?;
        // Stay below core request length: no BIG-REQUESTS feature/negotiation.
        let chunk = usize::from(connection.setup().maximum_request_length)
            .saturating_mul(4)
            .saturating_sub(64)
            .min(64 * 1024);
        let mut x = Self {
            connection,
            window,
            chunk,
            atoms: HashMap::new(),
            content: None,
            sends: HashMap::new(),
        };
        x.arm();
        x.check(
            x.connection
                .create_window(
                    0,
                    window,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )
                .map_err(native_error)?,
        )?;
        for name in TEXT.into_iter().chain([
            "CLIPBOARD",
            "CLIPBOARD_MANAGER",
            "SAVE_TARGETS",
            "TARGETS",
            "INCR",
            "image/png",
            "BMUX_DATA",
            "BMUX_SAVE",
        ]) {
            x.arm();
            let atom = x
                .connection
                .intern_atom(false, name.as_bytes())
                .map_err(native_error)?
                .reply()
                .map_err(native_error)?
                .atom;
            x.atoms.insert(name, atom);
        }
        if x.chunk == 0 {
            return Err(native_error("invalid request limit"));
        }
        Ok(x)
    }
    fn arm(&self) {
        self.connection.stream().arm();
    }
    fn atom(&self, name: &str) -> u32 {
        self.atoms[name]
    }
    fn check(
        &self,
        cookie: VoidCookie<'_, RustConnection<DeadlineStream>>,
    ) -> Result<(), ClipboardError> {
        // Every checked request must share its caller's armed stream deadline;
        // check_deadline also rejects work queued after that deadline expired.
        self.connection
            .stream()
            .check_deadline()
            .map_err(native_error)?;
        cookie.check().map_err(native_error)
    }
    fn owner(&self, selection: u32) -> Result<u32, ClipboardError> {
        self.arm();
        Ok(self
            .connection
            .get_selection_owner(selection)
            .map_err(native_error)?
            .reply()
            .map_err(native_error)?
            .owner)
    }
    fn property(
        &self,
        window: u32,
        property: u32,
        target: u32,
        format: u8,
        bytes: &[u8],
    ) -> Result<(), ClipboardError> {
        let unit = usize::from(format) / 8;
        if unit == 0 || !bytes.len().is_multiple_of(unit) {
            return Err(native_error("invalid property format"));
        }
        self.arm();
        self.check(
            self.connection
                .change_property(
                    PropMode::REPLACE,
                    window,
                    property,
                    target,
                    format,
                    u32::try_from(bytes.len() / unit).map_err(native_error)?,
                    bytes,
                )
                .map_err(native_error)?,
        )
    }
    fn event(&self) -> Result<Option<[u8; 32]>, ClipboardError> {
        self.arm();
        self.connection
            .poll_for_raw_event()
            .map_err(native_error)?
            .map(|event| {
                if event.len() < 32 {
                    return Err(native_error("short X event"));
                }
                let mut bytes = [0; 32];
                bytes.copy_from_slice(&event[..32]);
                if bytes[0] == 0 {
                    return Err(native_error(format!("X protocol error {}", bytes[1])));
                }
                Ok(bytes)
            })
            .transpose()
    }
    fn pause(&self) {
        self.arm();
        let _ = self.connection.flush();
        std::thread::sleep(Duration::from_millis(2));
    }
    fn notify(&self, window: u32, bytes: &[u8; 32]) -> Result<(), ClipboardError> {
        self.arm();
        self.check(
            self.connection
                .send_event(false, window, EventMask::NO_EVENT, *bytes)
                .map_err(native_error)?,
        )
    }
    fn selection_request(&mut self, event: &[u8; 32]) {
        let requestor = word(event, 12);
        let target = word(event, 20);
        let property = match word(event, 24) {
            0 => target,
            n => n,
        };
        let accepted = match self.accept_selection(requestor, target, property, word(event, 16)) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(requestor, target, property, %error, "X11 selection request rejected");
                false
            }
        };
        let mut notify = [0; 32];
        notify[0] = 31;
        put(&mut notify, 4, word(event, 4));
        put(&mut notify, 8, requestor);
        put(&mut notify, 12, word(event, 16));
        put(&mut notify, 16, target);
        put(&mut notify, 20, if accepted { property } else { 0 });
        let _ = self.notify(requestor, &notify);
    }
    fn accept_selection(
        &mut self,
        requestor: u32,
        target: u32,
        property: u32,
        selection: u32,
    ) -> Result<(), ClipboardError> {
        if selection != self.atom("CLIPBOARD") {
            return Err(native_error("unsupported selection"));
        }
        let Some((bytes, image)) = &self.content else {
            return Err(native_error("selection ownership lost"));
        };
        let targets: Vec<u32> = if *image {
            vec![self.atom("image/png")]
        } else {
            TEXT.iter().map(|name| self.atom(name)).collect()
        };
        if target == self.atom("TARGETS") {
            let atoms: Vec<u8> = targets
                .into_iter()
                .chain([self.atom("TARGETS"), self.atom("SAVE_TARGETS")])
                .flat_map(u32::to_ne_bytes)
                .collect();
            return self.property(requestor, property, 4, 32, &atoms);
        }
        if target == self.atom("SAVE_TARGETS") {
            return self.property(requestor, property, 4, 32, &[]);
        }
        if !targets.contains(&target) {
            return Err(native_error("missing selection target"));
        }
        let bytes = if target == self.atom("STRING") {
            String::from_utf8_lossy(bytes)
                .chars()
                .map(|character| u8::try_from(u32::from(character)).unwrap_or(b'?'))
                .collect()
        } else {
            bytes.clone()
        };
        if bytes.len() <= self.chunk {
            return self.property(requestor, property, target, 8, &bytes);
        }
        if self.sends.len() >= 32 || self.sends.contains_key(&(requestor, property)) {
            return Err(native_error("selection transfer capacity exceeded"));
        }
        self.arm();
        self.check(
            self.connection
                .change_window_attributes(
                    requestor,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )
                .map_err(native_error)?,
        )?;
        self.property(
            requestor,
            property,
            self.atom("INCR"),
            32,
            &u32::try_from(bytes.len())
                .map_err(native_error)?
                .to_ne_bytes(),
        )?;

        self.sends.insert(
            (requestor, property),
            Send {
                bytes,
                offset: 0,
                target,
                expires: Instant::now() + TIMEOUT,
                deadline: Instant::now() + Duration::from_secs(8),
            },
        );
        Ok(())
    }
    fn serve(&mut self, event: [u8; 32]) {
        self.sends.retain(|_, send| {
            let active = Instant::now() < send.expires && Instant::now() < send.deadline;
            if !active {
                tracing::warn!(
                    sent = send.offset,
                    total = send.bytes.len(),
                    "X11 INCR deadline exceeded; no success terminator sent"
                );
            }
            active
        });
        match event[0] & 127 {
            29 => self.content = None,
            30 => self.selection_request(&event),
            28 if event[16] == 1 => {
                let key = (word(&event, 4), word(&event, 8));
                if let Some(mut send) = self.sends.remove(&key) {
                    let range = send.acknowledged_chunk(self.chunk);
                    let end = range.end;
                    match self.property(key.0, key.1, send.target, 8, &send.bytes[range]) {
                        Ok(()) => {
                            if send.acknowledge_written(end) {
                            } else {
                                self.sends.insert(key, send);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(requestor=key.0, property=key.1, %error, "X11 INCR write failed");
                        }
                    }
                }
            }
            _ => {}
        }
        self.arm();
        let _ = self.connection.flush();
    }
    fn set(&mut self, bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
        self.content = Some((bytes, image));
        self.arm();
        self.check(
            self.connection
                .set_selection_owner(self.window, self.atom("CLIPBOARD"), 0u32)
                .map_err(native_error)?,
        )?;
        if self.owner(self.atom("CLIPBOARD"))? != self.window {
            self.content = None;
            return Err(native_error("selection ownership refused"));
        }
        self.arm();
        self.connection.flush().map_err(native_error)
    }
    fn handoff(&mut self) -> Result<(), ClipboardError> {
        if self.content.is_none() || self.owner(self.atom("CLIPBOARD"))? != self.window {
            return Ok(());
        }
        let manager = self.atom("CLIPBOARD_MANAGER");
        if self.owner(manager)? == 0 {
            return Err(native_error(
                "no clipboard manager: clipboard cannot survive process exit",
            ));
        }
        let property = self.atom("BMUX_SAVE");
        self.arm();
        self.connection
            .delete_property(self.window, property)
            .map_err(native_error)?;
        self.connection
            .convert_selection(
                self.window,
                manager,
                self.atom("SAVE_TARGETS"),
                property,
                0u32,
            )
            .map_err(native_error)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                return Err(native_error("clipboard manager handoff timed out"));
            }
            let Some(event) = self.event()? else {
                self.pause();
                continue;
            };
            if let Some(result) = handoff_notification(
                &event,
                self.window,
                manager,
                self.atom("SAVE_TARGETS"),
                property,
            ) {
                return result;
            }
            self.serve(event);
        }
    }
    fn fetch(&mut self, target: u32) -> Result<Option<(u8, Vec<u8>)>, ClipboardError> {
        let property = self.atom("BMUX_DATA");
        self.arm();
        self.connection
            .delete_property(self.window, property)
            .map_err(native_error)?;
        self.connection
            .convert_selection(self.window, self.atom("CLIPBOARD"), target, property, 0u32)
            .map_err(native_error)?;

        let mut transfer = Receive::default();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(native_error("selection transfer timed out"));
            }
            let Some(event) = self.event()? else {
                self.pause();
                continue;
            };
            let initial = event[0] & 127 == 31
                && word(&event, 8) == self.window
                && word(&event, 12) == self.atom("CLIPBOARD")
                && word(&event, 16) == target;
            let chunk = event[0] & 127 == 28
                && event[16] == 0
                && word(&event, 4) == self.window
                && word(&event, 8) == property
                && transfer.incremental;
            if !initial && !chunk {
                self.serve(event);
                continue;
            }
            if initial && word(&event, 20) == 0 {
                return Ok(None);
            }
            self.arm();
            let reply = self
                .connection
                .get_property(
                    false,
                    self.window,
                    property,
                    0u32,
                    0,
                    u32::try_from(LIMIT / 4).expect("bounded limit"),
                )
                .map_err(native_error)?
                .reply()
                .map_err(native_error)?;
            if reply.bytes_after != 0 || reply.value.len() > LIMIT {
                return Err(native_error("oversized or incomplete property"));
            }
            if initial && reply.type_ == self.atom("INCR") {
                transfer.start(reply.format, &reply.value)?;
                self.connection
                    .delete_property(self.window, property)
                    .map_err(native_error)?;
            } else {
                if reply.type_ != expected_property_type(target, self.atom("TARGETS"))
                    && !(target == self.atom("TEXT")
                        && (reply.type_ == self.atom("STRING")
                            || reply.type_ == self.atom("UTF8_STRING")))
                {
                    return Err(native_error("selection property type mismatch"));
                }
                let finished = transfer.push(reply.format, &reply.value)?;
                self.connection
                    .delete_property(self.window, property)
                    .map_err(native_error)?;
                if finished {
                    return Ok(Some((reply.format, transfer.bytes)));
                }
            }
        }
    }
    fn get(&mut self, image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
        let owner = self.owner(self.atom("CLIPBOARD"))?;
        if owner == 0 {
            return Ok(None);
        }
        if owner == self.window {
            return Ok(local_content(self.content.as_ref(), image));
        }
        let offered = match self.fetch(self.atom("TARGETS"))? {
            Some((32, bytes)) if bytes.len().is_multiple_of(4) => Some(
                bytes
                    .chunks_exact(4)
                    .map(|bytes| word(bytes, 0))
                    .collect::<Vec<_>>(),
            ),
            Some(_) => return Err(native_error("invalid TARGETS property")),
            None => None,
        };
        let candidates: Vec<&str> = if image {
            vec!["image/png"]
        } else {
            TEXT.to_vec()
        };
        for name in candidates {
            let atom = self.atom(name);
            if offered.as_ref().is_some_and(|list| !list.contains(&atom)) {
                continue;
            }
            if let Some((format, bytes)) = self.fetch(atom)? {
                if format != 8 {
                    return Err(native_error("invalid data format"));
                }
                return Ok(Some(if name == "STRING" {
                    bytes
                        .into_iter()
                        .map(char::from)
                        .collect::<String>()
                        .into_bytes()
                } else {
                    bytes
                }));
            }
        }
        Ok(None)
    }
}
#[derive(Default)]
struct Receive {
    incremental: bool,
    format: Option<u8>,
    bytes: Vec<u8>,
}
impl Receive {
    fn start(&mut self, format: u8, bytes: &[u8]) -> Result<(), ClipboardError> {
        // ICCCM specifies one size hint, but xclip 0.13 emits INCR/32 with
        // zero items. The hint is advisory: accept that peer without reserving
        // memory, while bounding every actual chunk independently.
        if self.incremental
            || format != 32
            || ![0, 4].contains(&bytes.len())
            || (bytes.len() == 4 && word(bytes, 0) as usize > LIMIT)
        {
            return Err(failure("native-x11", "invalid INCR header".to_string()));
        }
        self.incremental = true;
        Ok(())
    }
    fn push(&mut self, format: u8, bytes: &[u8]) -> Result<bool, ClipboardError> {
        if ![8, 16, 32].contains(&format)
            || self.format.is_some_and(|old| old != format)
            || !bytes.len().is_multiple_of(usize::from(format / 8))
        {
            return Err(failure(
                "native-x11",
                "inconsistent transfer format".to_string(),
            ));
        }
        self.format = Some(format);
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > LIMIT)
        {
            return Err(failure("native-x11", "transfer exceeds limit".to_string()));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(!self.incremental || bytes.is_empty())
    }
}
const fn expected_property_type(target: u32, targets: u32) -> u32 {
    // ICCCM TARGETS returns an ATOM list, not a property of type TARGETS.
    if target == targets { 4 } else { target }
}
fn local_content(content: Option<&(Vec<u8>, bool)>, image: bool) -> Option<Vec<u8>> {
    content
        .filter(|(_, kind)| *kind == image)
        .map(|(bytes, _)| bytes.clone())
}
fn handoff_notification(
    event: &[u8; 32],
    window: u32,
    manager: u32,
    target: u32,
    property: u32,
) -> Option<Result<(), ClipboardError>> {
    if event[0] & 127 != 31
        || word(event, 8) != window
        || word(event, 12) != manager
        || word(event, 16) != target
    {
        return None;
    }
    Some(if word(event, 20) == property {
        Ok(())
    } else {
        Err(native_error("clipboard manager refused saving selection"))
    })
}
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
fn completion<T>(receiver: &mpsc::Receiver<T>, timeout: Duration) -> Result<T, ClipboardError> {
    receiver
        .recv_timeout(timeout)
        .map_err(|error| failure("native-x11", format!("worker completion failed: {error}")))
}
type Reply = mpsc::SyncSender<Result<Option<Vec<u8>>, ClipboardError>>;
enum Command {
    Set(Vec<u8>, bool, Reply),
    Get(bool, Reply),
    Shutdown(Reply),
}
static WORKER: OnceLock<Mutex<Option<mpsc::SyncSender<Command>>>> = OnceLock::new();
fn request(bytes: Option<Vec<u8>>, image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
    let mut worker = WORKER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| failure("native-x11", e.to_string()))?;
    if worker.is_none() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clipboard-x11".into())
            .spawn(move || {
                let mut x = match X::new() {
                    Ok(x) => {
                        let _ = ready_tx.send(Ok(()));
                        x
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                loop {
                    match receiver.try_recv() {
                        Ok(Command::Set(bytes, image, reply)) => {
                            let _ = reply.send(x.set(bytes, image).map(|()| None));
                        }
                        Ok(Command::Get(image, reply)) => {
                            let _ = reply.send(x.get(image));
                        }
                        Ok(Command::Shutdown(reply)) => {
                            let result = x.handoff().map(|()| None);
                            drop(x);
                            let _ = reply.send(result);
                            return;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    match x.event() {
                        Ok(Some(event)) => {
                            x.serve(event);
                        }
                        Ok(None) => x.pause(),
                        Err(_) => return,
                    }
                }
            })
            .map_err(|e| failure("native-x11", e.to_string()))?;
        completion(&ready_rx, REQUEST_TIMEOUT)??;
        EXIT_HOOK.get_or_init(|| unsafe { libc::atexit(exit_handoff) });
        if *EXIT_HOOK.get().expect("registered exit hook") != 0 {
            return Err(native_error("registering exit hook failed"));
        }
        *worker = Some(sender);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let command = match bytes {
        Some(bytes) => Command::Set(bytes, image, sender),
        None => Command::Get(image, sender),
    };
    if worker
        .as_ref()
        .expect("initialized worker")
        .try_send(command)
        .is_err()
    {
        *worker = None;
        drop(worker);
        return Err(failure("native-x11", "worker stopped".to_string()));
    }
    match completion(&receiver, REQUEST_TIMEOUT) {
        Ok(result) => {
            drop(worker);
            result
        }
        Err(error) => {
            drop(worker);
            Err(error)
        }
    }
}
static EXIT_HOOK: OnceLock<c_int> = OnceLock::new();
extern "C" fn exit_handoff() {
    let _ = std::panic::catch_unwind(|| {
        if let Err(error) = super::shutdown() {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr().lock(), "{error}");
        }
    });
}
/// Disconnect after an acknowledged `SAVE_TARGETS` handoff, not detached `Drop`.
pub fn shutdown() -> Result<(), ClipboardError> {
    let Some(worker) = WORKER.get() else {
        return Ok(());
    };
    let mut worker = worker
        .try_lock()
        .map_err(|error| native_error(format!("shutdown worker unavailable: {error}")))?;
    let Some(sender) = worker.take() else {
        return Ok(());
    };
    let (reply, receiver) = mpsc::sync_channel(1);
    if let Err(error) = sender.try_send(Command::Shutdown(reply)) {
        *worker = Some(sender);
        drop(worker);
        return Err(native_error(error));
    }
    let result = completion(&receiver, REQUEST_TIMEOUT).and_then(|result| result.map(|_| ()));
    if result.is_err() {
        *worker = Some(sender);
    }
    drop(worker);
    result
}
pub fn write(bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
    request(Some(bytes), image).map(|_| ())
}
pub fn read(image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
    request(None, image)
}
#[cfg(test)]
mod tests {
    use super::*;
    const EXIT_CONTENT: &[u8] = b"persisted by real process exit";
    const EXIT_HELPER: &str = "native::unix::x11::tests::isolated_exit_hook_child";

    // Child cleanup must run on every parent assertion/error path. stdout/stderr
    // are inherited (not piped), so no subprocess output can fill a pipe.
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    #[test]
    #[ignore = "helper invoked only by isolated_process_exit_hook_persists_clipboard"]
    fn isolated_exit_hook_child() {
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
            Ok("1")
        );
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_EXIT_HELPER").as_deref(),
            Ok("1")
        );
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
        write(EXIT_CONTENT.to_vec(), false).unwrap();
        assert_eq!(*EXIT_HOOK.get().expect("copy registered exit callback"), 0);
        // Return normally to the test harness; its process exit, not an explicit
        // shutdown/handoff call, must run our libc atexit callback.
    }
    fn save_exit_selection(manager: &mut X, request: &[u8; 32]) {
        assert_eq!(word(request, 20), manager.atom("SAVE_TARGETS"));
        let bytes = manager.get(false).unwrap().expect("child owns text");
        assert_eq!(bytes, EXIT_CONTENT);
        manager.set(bytes, false).unwrap();
        let mut notify = [0; 32];
        notify[0] = 31;
        put(&mut notify, 4, word(request, 4));
        put(&mut notify, 8, word(request, 12));
        put(&mut notify, 12, word(request, 16));
        put(&mut notify, 16, word(request, 20));
        put(&mut notify, 20, word(request, 24));
        manager.notify(word(request, 12), &notify).unwrap();
    }
    #[test]
    #[ignore = "requires a coordinator-reserved disposable X11 display"]
    fn isolated_process_exit_hook_persists_clipboard() {
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
            Ok("1")
        );
        assert!(std::env::var_os("WAYLAND_DISPLAY").is_none());
        let mut manager = X::new().unwrap();
        manager.arm();
        manager
            .check(
                manager
                    .connection
                    .set_selection_owner(manager.window, manager.atom("CLIPBOARD_MANAGER"), 0u32)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            manager.owner(manager.atom("CLIPBOARD_MANAGER")).unwrap(),
            manager.window
        );
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    EXIT_HELPER,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("BMUX_CLIPBOARD_EXIT_HELPER", "1")
                .env_remove("WAYLAND_DISPLAY")
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let mut saved = false;
        loop {
            assert!(
                Instant::now() < deadline,
                "child exit/handoff exceeded deadline"
            );
            if let Some(event) = manager.event().unwrap() {
                if event[0] & 127 == 30 && word(&event, 16) == manager.atom("CLIPBOARD_MANAGER") {
                    assert!(!saved, "unexpected repeated exit handoff");
                    save_exit_selection(&mut manager, &event);
                    saved = true;
                } else {
                    manager.serve(event);
                }
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "exit helper failed: {status}");
                assert!(saved, "helper exited without acknowledged manager handoff");
                break;
            }
            manager.pause();
        }
        assert_eq!(
            manager.owner(manager.atom("CLIPBOARD")).unwrap(),
            manager.window
        );
        // Fresh connection after confirmed child exit: no local-content shortcut.
        let (reply, result) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let value = X::new().and_then(|mut reader| reader.get(false));
            let _ = reply.send(value);
        });
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let bytes = loop {
            if let Ok(result) = result.try_recv() {
                break result.unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "post-exit independent read exceeded deadline"
            );
            if let Some(event) = manager.event().unwrap() {
                manager.serve(event);
            } else {
                manager.pause();
            }
        };
        reader.join().unwrap();
        assert_eq!(bytes, Some(EXIT_CONTENT.to_vec()));
    }
    #[test]
    fn missing_authority_allows_server_setup_attempt() {
        assert_eq!(
            auth_or_empty(Err(io::Error::from(io::ErrorKind::NotFound))),
            (Vec::new(), Vec::new())
        );
        assert_eq!(auth_or_empty(Ok(None)), (Vec::new(), Vec::new()));
        let credentials = (b"MIT-MAGIC-COOKIE-1".to_vec(), vec![7; 16]);
        assert_eq!(auth_or_empty(Ok(Some(credentials.clone()))), credentials);
    }
    #[test]
    fn xclip_zero_item_incr_hint_is_advisory_not_empty_content() {
        let mut receive = Receive::default();
        receive.start(32, &[]).unwrap();
        assert!(!receive.push(8, b"external producer").unwrap());
        assert!(receive.push(8, &[]).unwrap());
        assert_eq!(receive.bytes, b"external producer");
        let mut invalid = Receive::default();
        assert!(invalid.start(32, &[0; 8]).is_err());
    }
    #[test]
    fn outgoing_incr_requires_ack_for_final_empty_property() {
        let mut send = Send {
            bytes: b"abcdef".to_vec(),
            offset: 0,
            target: 1,
            expires: Instant::now() + TIMEOUT,
            deadline: Instant::now() + TIMEOUT,
        };
        assert_eq!(send.acknowledged_chunk(3), 0..3);
        assert!(!send.acknowledge_written(3));
        assert_eq!(send.acknowledged_chunk(3), 3..6);
        assert!(!send.acknowledge_written(6));
        assert_eq!(send.acknowledged_chunk(3), 6..6);
        assert!(send.acknowledge_written(6));
    }
    #[test]
    fn handoff_requires_exact_manager_acknowledgement() {
        let mut event = [0; 32];
        event[0] = 31;
        put(&mut event, 8, 1);
        put(&mut event, 12, 2);
        put(&mut event, 16, 3);
        put(&mut event, 20, 4);
        assert!(handoff_notification(&event, 1, 2, 3, 4).unwrap().is_ok());
        assert!(handoff_notification(&event, 9, 2, 3, 4).is_none());
        put(&mut event, 20, 0);
        assert!(handoff_notification(&event, 1, 2, 3, 4).unwrap().is_err());
        event[0] = 29;
        assert!(handoff_notification(&event, 1, 2, 3, 4).is_none());
    }
    #[test]
    #[ignore = "requires a coordinator-reserved disposable X11 display"]
    fn isolated_manager_saves_before_owner_disconnects() {
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
            Ok("1")
        );
        let mut manager = X::new().unwrap();
        manager.arm();
        manager
            .check(
                manager
                    .connection
                    .set_selection_owner(manager.window, manager.atom("CLIPBOARD_MANAGER"), 0u32)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            manager.owner(manager.atom("CLIPBOARD_MANAGER")).unwrap(),
            manager.window
        );
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            let mut owner = X::new().unwrap();
            owner.set(b"saved through manager".to_vec(), false).unwrap();
            ready_tx.send(()).unwrap();
            let result = owner.handoff();
            drop(owner);
            result
        });
        completion(&ready_rx, REQUEST_TIMEOUT).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "manager request deadline");
            let Some(request) = manager.event().unwrap() else {
                manager.pause();
                continue;
            };
            if request[0] & 127 != 30 || word(&request, 16) != manager.atom("CLIPBOARD_MANAGER") {
                manager.serve(request);
                continue;
            }
            let bytes = manager.get(false).unwrap().unwrap();
            manager.set(bytes, false).unwrap();
            let mut notify = [0; 32];
            notify[0] = 31;
            for offset in [4, 12, 16, 20] {
                put(
                    &mut notify,
                    offset,
                    word(&request, offset + if offset == 4 { 0 } else { 4 }),
                );
            }
            put(&mut notify, 8, word(&request, 12));
            manager.notify(word(&request, 12), &notify).unwrap();
            break;
        }
        owner.join().unwrap().unwrap();
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let result = X::new().unwrap().get(false);
            stop_tx.send(()).unwrap();
            result
        });
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while stop_rx.try_recv().is_err() && Instant::now() < deadline {
            if let Some(event) = manager.event().unwrap() {
                manager.serve(event);
            } else {
                manager.pause();
            }
        }
        assert_eq!(
            reader.join().unwrap().unwrap(),
            Some(b"saved through manager".to_vec())
        );
    }
    #[test]
    fn targets_property_is_atom_list() {
        assert_eq!(expected_property_type(99, 99), 4);
        assert_eq!(expected_property_type(100, 99), 100);
    }
    #[test]
    fn targets_atom32_incremental_fixture() {
        let targets = 99;
        assert_eq!(expected_property_type(targets, targets), 4);
        let mut receive = Receive::default();
        receive.start(32, &8u32.to_ne_bytes()).unwrap();
        assert!(!receive.push(32, &100u32.to_ne_bytes()).unwrap());
        assert!(!receive.push(32, &101u32.to_ne_bytes()).unwrap());
        assert!(receive.push(32, &[]).unwrap());
        assert_eq!(
            receive
                .bytes
                .chunks_exact(4)
                .map(|bytes| word(bytes, 0))
                .collect::<Vec<_>>(),
            vec![100, 101]
        );
    }
    #[test]
    fn local_read_preserves_empty_unicode_and_missing_formats() {
        for text in ["", "界 🦀"] {
            let content = (text.as_bytes().to_vec(), false);
            assert_eq!(
                local_content(Some(&content), false),
                Some(text.as_bytes().to_vec())
            );
            assert_eq!(local_content(Some(&content), true), None);
        }
        assert_eq!(local_content(None, false), None);
    }
    #[test]
    fn completion_is_bounded_even_with_live_unresponsive_worker() {
        let (_sender, receiver) = mpsc::sync_channel::<()>(1);
        assert!(completion(&receiver, Duration::ZERO).is_err());
    }
    #[test]
    #[ignore = "requires a coordinator-reserved disposable X11 display"]
    fn isolated_two_connection_incr_roundtrip() {
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
            Ok("1")
        );
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let text = "large 界\n".repeat(200_000).into_bytes();
        let expected = text.clone();
        let owner = std::thread::spawn(move || {
            let mut x = X::new().unwrap();
            x.set(text, false).unwrap();
            ready_tx.send(()).unwrap();
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            while Instant::now() < deadline && stop_rx.try_recv().is_err() {
                if let Some(event) = x.event().unwrap() {
                    x.serve(event);
                } else {
                    x.pause();
                }
            }
        });
        completion(&ready_rx, REQUEST_TIMEOUT).unwrap();
        let mut reader = X::new().unwrap();
        let result = reader.get(false);
        stop_tx.send(()).unwrap();
        owner.join().unwrap();
        assert_eq!(result.unwrap(), Some(expected));
    }
    #[test]
    fn abi_layouts() {
        // Generated x11rb wire layouts replace the handwritten XCB ABI.
        let mut event = [0; 32];
        put(&mut event, 24, 0x1234);
        assert_eq!(word(&event, 24), 0x1234);
    }
    #[test]
    fn incr_needs_final_empty_chunk() {
        let mut r = Receive::default();
        r.start(32, &6u32.to_ne_bytes()).unwrap();
        assert!(!r.push(8, b"abc").unwrap());
        assert!(!r.push(8, b"def").unwrap());
        assert!(r.push(8, b"").unwrap());
        assert_eq!(r.bytes, b"abcdef");
    }
    #[test]
    fn reject_invalid_incr() {
        let mut r = Receive::default();
        assert!(r.start(8, &[0; 4]).is_err());
        assert!(r.start(32, &u32::MAX.to_ne_bytes()).is_err());
        r.start(32, &0u32.to_ne_bytes()).unwrap();
        assert!(r.start(32, &0u32.to_ne_bytes()).is_err());
    }
    #[test]
    fn reject_format_switch() {
        let mut r = Receive::default();
        r.start(32, &0u32.to_ne_bytes()).unwrap();
        r.push(8, b"a").unwrap();
        assert!(r.push(32, &[0; 4]).is_err());
    }
}
