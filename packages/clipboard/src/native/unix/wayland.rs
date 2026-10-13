//! Generated data-control protocols over wayland-client's pure Rust transport.
//! No libwayland ABI descriptors, library loading or blocking roundtrips.
//! All proxies and bounded event/transfer state belong to one ownership worker.
use super::{LIMIT, failure};
use crate::ClipboardError;
use std::{
    collections::HashMap,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    ptr,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{ObjectId, WaylandError},
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ed, ext_data_control_manager_v1 as em,
    ext_data_control_offer_v1 as eo, ext_data_control_source_v1 as es,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wd, zwlr_data_control_manager_v1 as wm,
    zwlr_data_control_offer_v1 as wo, zwlr_data_control_source_v1 as ws,
};

const TIMEOUT: Duration = Duration::from_secs(4);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(8);
const MIMES: [&str; 6] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];
const MAX_EVENTS: usize = 1024;
const MAX_OFFERS: usize = 256;
const MAX_SEATS: usize = 32;
const MAX_SENDS: usize = 32;

// Consuming errors permits use directly as a map_err callback at API boundaries.
#[allow(clippy::needless_pass_by_value)]
fn error(message: impl ToString) -> ClipboardError {
    failure("native-wayland", message.to_string())
}
#[derive(Clone)]
enum Manager {
    Ext(em::ExtDataControlManagerV1),
    Wlr(wm::ZwlrDataControlManagerV1),
}
#[derive(Clone)]
enum Device {
    Ext(ed::ExtDataControlDeviceV1),
    Wlr(wd::ZwlrDataControlDeviceV1),
}
#[derive(Clone)]
enum Offer {
    Ext(eo::ExtDataControlOfferV1),
    Wlr(wo::ZwlrDataControlOfferV1),
}
#[derive(Clone)]
enum Source {
    Ext(es::ExtDataControlSourceV1),
    Wlr(ws::ZwlrDataControlSourceV1),
}
impl Manager {
    fn device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Self::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Self::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }
    fn source(&self, qh: &QueueHandle<State>) -> Source {
        match self {
            Self::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Self::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
        }
    }
    fn destroy(&self) {
        match self {
            Self::Ext(m) => m.destroy(),
            Self::Wlr(m) => m.destroy(),
        }
    }
}
impl Device {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(p) => p.id(),
            Self::Wlr(p) => p.id(),
        }
    }
    fn set(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(p), Source::Ext(s)) => p.set_selection(Some(s)),
            (Self::Wlr(p), Source::Wlr(s)) => p.set_selection(Some(s)),
            _ => unreachable!("one negotiated protocol per connection"),
        }
    }
    fn destroy(&self) {
        match self {
            Self::Ext(p) => p.destroy(),
            Self::Wlr(p) => p.destroy(),
        }
    }
}
impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(p) => p.id(),
            Self::Wlr(p) => p.id(),
        }
    }
    fn receive(&self, mime: String, fd: &OwnedFd) {
        match self {
            Self::Ext(p) => p.receive(mime, fd.as_fd()),
            Self::Wlr(p) => p.receive(mime, fd.as_fd()),
        }
    }
    fn destroy(&self) {
        match self {
            Self::Ext(p) => p.destroy(),
            Self::Wlr(p) => p.destroy(),
        }
    }
}
impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Self::Ext(p) => p.id(),
            Self::Wlr(p) => p.id(),
        }
    }
    fn offer(&self, mime: String) {
        match self {
            Self::Ext(p) => p.offer(mime),
            Self::Wlr(p) => p.offer(mime),
        }
    }
    fn destroy(&self) {
        match self {
            Self::Ext(p) => p.destroy(),
            Self::Wlr(p) => p.destroy(),
        }
    }
}
struct Seat {
    global: u32,
    proxy: wl_seat::WlSeat,
    device: Device,
    selection: Option<ObjectId>,
}
struct Offered {
    proxy: Offer,
    mimes: Vec<String>,
}
struct OwnedSource {
    proxy: Source,
    bytes: Arc<Vec<u8>>,
    image: bool,
}
struct Send {
    fd: OwnedFd,
    bytes: Arc<Vec<u8>>,
    offset: usize,
    expires: Instant,
    deadline: Instant,
}
enum Event {
    Global(u32, String, u32),
    Removed(u32),
    Offer(Offer),
    Mime(ObjectId, String),
    Selection(ObjectId, Option<ObjectId>),
    Primary(Option<Offer>),
    Finished(ObjectId),
    Cancelled(ObjectId),
    Send(ObjectId, String, OwnedFd),
    Sync(ObjectId),
}
#[derive(Default)]
struct State {
    events: Vec<Event>,
    failed: bool,
}
impl State {
    fn push(&mut self, event: Event) {
        if self.events.len() >= MAX_EVENTS {
            self.failed = true;
        } else {
            self.events.push(event);
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => state.push(Event::Global(name, interface, version)),
            wl_registry::Event::GlobalRemove { name } => state.push(Event::Removed(name)),
            _ => {}
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_callback::WlCallback,
        _: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.push(Event::Sync(proxy.id()));
    }
}
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore em::ExtDataControlManagerV1);
wayland_client::delegate_noop!(State: ignore wm::ZwlrDataControlManagerV1);
macro_rules! dispatch_protocol {
    ($device:ty, $de:path, $offer:ty, $oe:path, $source:ty, $se:path, $variant:ident) => {
        impl Dispatch<$device, ()> for State {
            fn event(state: &mut Self, proxy: &$device, event: <$device as Proxy>::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {
                use $de as E;
                match event {
                    E::DataOffer { id } => state.push(Event::Offer(Offer::$variant(id))),
                    E::Selection { id } => state.push(Event::Selection(proxy.id(), id.map(|p| p.id()))),
                    E::PrimarySelection { id } => state.push(Event::Primary(id.map(Offer::$variant))),
                    E::Finished => state.push(Event::Finished(proxy.id())), _ => {}
                }
            }
            wayland_client::event_created_child!(State, $device, [0 => ($offer, ())]);
        }
        impl Dispatch<$offer, ()> for State {
            fn event(state: &mut Self, proxy: &$offer, event: <$offer as Proxy>::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {
                use $oe as E;
                if let E::Offer { mime_type } = event { state.push(Event::Mime(proxy.id(), mime_type)); }
            }
        }
        impl Dispatch<$source, ()> for State {
            fn event(state: &mut Self, proxy: &$source, event: <$source as Proxy>::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {
                use $se as E;
                match event {
                    E::Send { mime_type, fd } => state.push(Event::Send(proxy.id(), mime_type, fd)),
                    E::Cancelled => state.push(Event::Cancelled(proxy.id())), _ => {}
                }
            }
        }
    };
}
dispatch_protocol!(
    ed::ExtDataControlDeviceV1,
    ed::Event,
    eo::ExtDataControlOfferV1,
    eo::Event,
    es::ExtDataControlSourceV1,
    es::Event,
    Ext
);
dispatch_protocol!(
    wd::ZwlrDataControlDeviceV1,
    wd::Event,
    wo::ZwlrDataControlOfferV1,
    wo::Event,
    ws::ZwlrDataControlSourceV1,
    ws::Event,
    Wlr
);

struct W {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    registry: wl_registry::WlRegistry,
    manager: Option<(u32, Manager)>,
    globals: Vec<(u32, String, u32)>,
    seats: Vec<Seat>,
    offers: HashMap<ObjectId, Offered>,
    sources: HashMap<ObjectId, OwnedSource>,
    sends: Vec<Send>,
    synced: Vec<ObjectId>,
    seats_created: usize,
}
impl Drop for W {
    fn drop(&mut self) {
        for (_, offer) in self.offers.drain() {
            offer.proxy.destroy();
        }
        for (_, source) in self.sources.drain() {
            source.proxy.destroy();
        }
        for seat in self.seats.drain(..) {
            seat.device.destroy();
        }
        if let Some((_, manager)) = &self.manager {
            manager.destroy();
        }
        // Dropping the Rust connection closes the socket and all remaining
        // local proxy/seat state. No foreign pointers or library handles remain.
    }
}
impl W {
    fn new() -> Result<Self, ClipboardError> {
        let connection = Connection::connect_to_env().map_err(error)?;
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let mut w = Self {
            connection,
            queue,
            registry,
            state: State::default(),
            manager: None,
            globals: Vec::new(),
            seats: Vec::new(),
            offers: HashMap::new(),
            sources: HashMap::new(),
            sends: Vec::new(),
            synced: Vec::new(),
            seats_created: 0,
        };
        w.sync(Instant::now() + TOTAL_TIMEOUT)?;
        let global = w
            .globals
            .iter()
            .find(|(_, name, _)| name == "ext_data_control_manager_v1")
            .or_else(|| {
                w.globals
                    .iter()
                    .find(|(_, name, _)| name == "zwlr_data_control_manager_v1")
            })
            .cloned()
            .ok_or_else(|| error("compositor provides neither ext nor wlr data-control"))?;
        let qh = w.queue.handle();
        let manager = if global.1 == "ext_data_control_manager_v1" {
            Manager::Ext(w.registry.bind(global.0, 1, &qh, ()))
        } else {
            Manager::Wlr(w.registry.bind(global.0, 1, &qh, ()))
        };
        w.manager = Some((global.0, manager));
        for (name, interface, _) in w.globals.clone() {
            if interface == "wl_seat" {
                w.add_seat(name)?;
            }
        }
        w.sync(Instant::now() + TOTAL_TIMEOUT)?;
        Ok(w)
    }
    fn add_seat(&mut self, global: u32) -> Result<(), ClipboardError> {
        if self.seats.iter().any(|s| s.global == global) {
            return Ok(());
        }
        if self.seats.len() >= MAX_SEATS || self.seats_created >= 256 {
            return Err(error("seat limit exceeded"));
        }
        let qh = self.queue.handle();
        let proxy = self.registry.bind(global, 1, &qh, ());
        let device = self
            .manager
            .as_ref()
            .ok_or_else(|| error("data-control manager unavailable"))?
            .1
            .device(&proxy, &qh);
        self.seats_created += 1;
        self.seats.push(Seat {
            global,
            proxy,
            device,
            selection: None,
        });
        Ok(())
    }
    fn discard_offer(&mut self, id: &ObjectId) {
        if let Some(offer) = self.offers.remove(id) {
            offer.proxy.destroy();
        }
    }
    fn remove_device(&mut self, id: &ObjectId) {
        if let Some(index) = self.seats.iter().position(|s| s.device.id() == *id) {
            let seat = self.seats.remove(index);
            if let Some(offer) = seat.selection {
                self.discard_offer(&offer);
            }
            seat.device.destroy();
            // Version-one wl_seat has no release request. Its local proxy is
            // reclaimed with the worker connection; hotplug count is bounded.
            drop(seat.proxy);
        }
    }
    // Keep protocol lifecycle transitions together to make ownership reviewable.
    #[allow(clippy::too_many_lines)]
    fn events(&mut self) -> Result<(), ClipboardError> {
        if self.state.failed {
            return Err(error("event queue limit exceeded"));
        }
        for event in std::mem::take(&mut self.state.events) {
            match event {
                Event::Global(id, name, version) => {
                    if self.globals.len() >= 256 || name.len() > 1024 {
                        return Err(error("registry resource limit exceeded"));
                    }
                    if name == "wl_seat" && self.manager.is_some() {
                        self.add_seat(id)?;
                    }
                    self.globals.push((id, name, version));
                }
                Event::Removed(id) => {
                    self.globals.retain(|g| g.0 != id);
                    if self.manager.as_ref().is_some_and(|m| m.0 == id) {
                        return Err(error("data-control manager removed"));
                    }
                    let devices: Vec<_> = self
                        .seats
                        .iter()
                        .filter(|s| s.global == id)
                        .map(|s| s.device.id())
                        .collect();
                    for device in devices {
                        self.remove_device(&device);
                    }
                }
                Event::Offer(proxy) => {
                    if self.offers.len() >= MAX_OFFERS {
                        proxy.destroy();
                        return Err(error("offer limit exceeded"));
                    }
                    self.offers.insert(
                        proxy.id(),
                        Offered {
                            proxy,
                            mimes: Vec::new(),
                        },
                    );
                }
                Event::Mime(id, mime) => {
                    if mime.len() > 4096 {
                        return Err(error("MIME length limit exceeded"));
                    }
                    if let Some(offer) = self.offers.get_mut(&id) {
                        if offer.mimes.len() >= 128 {
                            return Err(error("MIME count limit exceeded"));
                        }
                        offer.mimes.push(mime);
                    }
                }
                Event::Selection(device, offer) => {
                    if let Some(seat) = self.seats.iter_mut().find(|s| s.device.id() == device) {
                        let previous = std::mem::replace(&mut seat.selection, offer.clone());
                        if let Some(previous) = previous.filter(|p| Some(p) != offer.as_ref()) {
                            self.discard_offer(&previous);
                        }
                    } else if let Some(offer) = offer {
                        self.discard_offer(&offer);
                    }
                }
                Event::Primary(Some(offer)) => {
                    self.discard_offer(&offer.id());
                }
                Event::Primary(None) => {}
                Event::Finished(device) => self.remove_device(&device),
                Event::Cancelled(id) => {
                    if let Some(source) = self.sources.remove(&id) {
                        source.proxy.destroy();
                    }
                }
                Event::Send(id, mime, fd) => {
                    if mime.len() > 4096 {
                        return Err(error("MIME length limit exceeded"));
                    }
                    if let Some(source) = self.sources.get(&id)
                        && supported(&mime, source.image)
                        && self.sends.len() < MAX_SENDS
                    {
                        nonblocking(&fd)?;
                        self.sends.push(Send {
                            fd,
                            bytes: Arc::clone(&source.bytes),
                            offset: 0,
                            expires: Instant::now() + TIMEOUT,
                            deadline: Instant::now() + TOTAL_TIMEOUT,
                        });
                    }
                }
                Event::Sync(id) => {
                    if self.synced.len() >= 32 {
                        return Err(error("sync callback limit exceeded"));
                    }
                    self.synced.push(id);
                }
            }
        }
        Ok(())
    }
    fn pump(&mut self) -> Result<(), ClipboardError> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(error)?;
        self.events()?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let writable = match self.connection.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => return Err(error(e)), // guard cancellation is automatic
        };
        let mut poll = libc::pollfd {
            fd: self.connection.as_fd().as_raw_fd(),
            events: libc::POLLIN | if writable { libc::POLLOUT } else { 0 },
            revents: 0,
        };
        // SAFETY: poll references one initialized descriptor and waits at most 10ms.
        let ready = unsafe { libc::poll(&raw mut poll, 1, 10) };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(error(std::io::Error::last_os_error()));
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(error("display disconnected"));
        }
        if ready > 0 && poll.revents & libc::POLLIN != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(error(e)),
            }
        } else {
            drop(guard);
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(error)?;
        self.events()?;
        self.sends.retain_mut(|send| {
            if transfer_expired(Instant::now(), send.expires, send.deadline) {
                return false;
            }
            let remaining = &send.bytes[send.offset..];
            // SAFETY: fd is owned and nonblocking; SIGPIPE is blocked on worker.
            let written = unsafe {
                libc::write(
                    send.fd.as_raw_fd(),
                    remaining.as_ptr().cast(),
                    remaining.len().min(64 * 1024),
                )
            };
            if written > 0 {
                send.offset += usize::try_from(written).expect("positive write count");
                send.expires = Instant::now() + TIMEOUT;
            } else if written < 0
                && !matches!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                )
            {
                return false;
            }
            send.offset < send.bytes.len()
        });
        Ok(())
    }
    fn sync(&mut self, total: Instant) -> Result<(), ClipboardError> {
        let callback = self.connection.display().sync(&self.queue.handle(), ());
        let deadline = (Instant::now() + TIMEOUT).min(total);
        loop {
            if Instant::now() >= deadline {
                return Err(error("synchronization timed out"));
            }
            self.pump()?;
            if let Some(index) = self.synced.iter().position(|id| *id == callback.id()) {
                self.synced.remove(index);
                return Ok(());
            }
        }
    }
    fn set(&mut self, bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
        let total = Instant::now() + TOTAL_TIMEOUT;
        if bytes.len() > LIMIT {
            return Err(error("payload exceeds limit"));
        }
        self.sync(total)?;
        if self.seats.is_empty() {
            return Err(error("no active seats"));
        }
        let bytes = Arc::new(bytes);
        let manager = &self
            .manager
            .as_ref()
            .ok_or_else(|| error("manager unavailable"))?
            .1;
        if self.sources.len() + self.seats.len() > MAX_SEATS * 2 {
            return Err(error("source limit exceeded"));
        }
        for seat in &self.seats {
            let source = manager.source(&self.queue.handle());
            for mime in if image {
                &["image/png"][..]
            } else {
                &MIMES[..]
            } {
                source.offer((*mime).into());
            }
            seat.device.set(&source);
            self.sources.insert(
                source.id(),
                OwnedSource {
                    proxy: source,
                    bytes: Arc::clone(&bytes),
                    image,
                },
            );
        }
        self.sync(total)
    }
    fn get(&mut self, image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
        let total = Instant::now() + TOTAL_TIMEOUT;
        self.sync(total)?;
        if self.seats.is_empty() {
            return Err(error("no active seats"));
        }
        let choice = self.seats.iter().find_map(|seat| {
            let offer = self.offers.get(seat.selection.as_ref()?)?;
            choose(&offer.mimes, image).map(|mime| (offer.proxy.clone(), mime.to_string()))
        });
        let Some((offer, mime)) = choice else {
            return Ok(None);
        };
        let (read, write) = pipe()?;
        offer.receive(mime, &write);
        drop(write);
        let mut bytes = Vec::new();
        let mut buffer = vec![0; 64 * 1024].into_boxed_slice();
        let mut idle = Instant::now() + TIMEOUT;
        loop {
            if transfer_expired(Instant::now(), idle, total) {
                return Err(error("data transfer deadline exceeded"));
            }
            self.pump()?;
            // SAFETY: buffer is writable and read is an owned nonblocking fd.
            let count =
                unsafe { libc::read(read.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count == 0 {
                return Ok(Some(bytes));
            }
            if count > 0 {
                append(
                    &mut bytes,
                    &buffer[..usize::try_from(count).expect("positive read count")],
                )?;
                idle = Instant::now() + TIMEOUT;
            } else if !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Err(error(std::io::Error::last_os_error()));
            }
        }
    }
}
fn transfer_expired(now: Instant, idle: Instant, total: Instant) -> bool {
    now >= idle || now >= total
}
fn supported(mime: &str, image: bool) -> bool {
    if image {
        mime == "image/png"
    } else {
        MIMES.contains(&mime)
    }
}
fn choose(mimes: &[String], image: bool) -> Option<&str> {
    if image {
        mimes
            .iter()
            .find(|mime| mime.as_str() == "image/png")
            .map(String::as_str)
    } else {
        MIMES.iter().find_map(|wanted| {
            mimes
                .iter()
                .find(|mime| mime.as_str() == *wanted)
                .map(String::as_str)
        })
    }
}
fn transfer_fits(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_some_and(|length| length <= LIMIT)
}
fn append(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ClipboardError> {
    if !transfer_fits(bytes.len(), chunk.len()) {
        return Err(error("transfer exceeds limit"));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}
fn nonblocking(fd: &OwnedFd) -> Result<(), ClipboardError> {
    // SAFETY: fcntl only changes flags on a live owned descriptor.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        let descriptor = libc::fcntl(fd.as_raw_fd(), libc::F_GETFD);
        if flags < 0
            || descriptor < 0
            || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            || libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, descriptor | libc::FD_CLOEXEC) < 0
        {
            return Err(error(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}
fn pipe() -> Result<(OwnedFd, OwnedFd), ClipboardError> {
    let mut fds = [-1; 2];
    // SAFETY: pipe writes two descriptors on success, both immediately owned.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    nonblocking(&read)?;
    // External producers require a blocking write end: O_NONBLOCK is shared
    // across SCM_RIGHTS duplicates. Set only close-on-exec on their endpoint.
    if unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok((read, write))
}
type Reply = mpsc::SyncSender<Result<Option<Vec<u8>>, ClipboardError>>;
enum Command {
    Set(Vec<u8>, bool, Reply),
    Get(bool, Reply),
}
static WORKER: OnceLock<Mutex<Option<mpsc::SyncSender<Command>>>> = OnceLock::new();
fn request(bytes: Option<Vec<u8>>, image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
    if bytes.as_ref().is_some_and(|bytes| bytes.len() > LIMIT) {
        return Err(error("payload exceeds limit"));
    }
    let mut worker = WORKER
        .get_or_init(|| Mutex::new(None))
        .try_lock()
        .map_err(error)?;
    if worker.is_none() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clipboard-wayland".into())
            .spawn(move || {
                // SAFETY: Block SIGPIPE on this ownership thread only; closed peer
                // pipes must not terminate the process or change global handlers.
                unsafe {
                    let mut signals = std::mem::zeroed();
                    libc::sigemptyset(&raw mut signals);
                    libc::sigaddset(&raw mut signals, libc::SIGPIPE);
                    let code =
                        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const signals, ptr::null_mut());
                    if code != 0 {
                        let _ =
                            ready_tx.send(Err(error(format!("blocking SIGPIPE failed: {code}"))));
                        return;
                    }
                }
                let mut w = match W::new() {
                    Ok(w) => {
                        if ready_tx.send(Ok(())).is_err() {
                            return;
                        }
                        w
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                loop {
                    match receiver.try_recv() {
                        Ok(Command::Set(bytes, image, reply)) => {
                            let _ = reply.send(w.set(bytes, image).map(|()| None));
                        }
                        Ok(Command::Get(image, reply)) => {
                            let _ = reply.send(w.get(image));
                        }
                        Err(mpsc::TryRecvError::Disconnected) => return,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    if w.pump().is_err() {
                        return;
                    }
                }
            })
            .map_err(error)?;
        ready_rx
            .recv_timeout(Duration::from_secs(18))
            .map_err(error)??;
        *worker = Some(sender);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let command = match bytes {
        Some(bytes) => Command::Set(bytes, image, sender),
        None => Command::Get(image, sender),
    };
    match worker
        .as_ref()
        .expect("initialized worker")
        .try_send(command)
    {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => return Err(error("ownership worker busy")),
        Err(mpsc::TrySendError::Disconnected(_)) => {
            *worker = None;
            return Err(error("ownership worker disconnected"));
        }
    }
    match receiver.recv_timeout(TOTAL_TIMEOUT + Duration::from_secs(1)) {
        Ok(result) => result,
        Err(e) => {
            *worker = None;
            drop(worker);
            Err(error(e))
        }
    }
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
    #[test]
    fn aliases_are_explicit_not_arbitrary_text_formats() {
        for alias in ["UTF8_STRING", "TEXT", "STRING"] {
            assert_eq!(choose(&[alias.to_string()], false), Some(alias));
            assert!(supported(alias, false));
        }
        assert_eq!(choose(&["text/html".to_string()], false), None);
    }
    #[test]
    fn progress_cannot_extend_total_deadline() {
        let now = Instant::now();
        assert!(transfer_expired(
            now + TOTAL_TIMEOUT,
            now + TOTAL_TIMEOUT + TIMEOUT,
            now + TOTAL_TIMEOUT
        ));
        assert!(!transfer_expired(now, now + TIMEOUT, now + TOTAL_TIMEOUT));
    }
    #[test]
    fn mime_preference_and_missing_format() {
        let mimes = vec!["text/plain".into(), "text/plain;charset=utf-8".into()];
        assert_eq!(choose(&mimes, false), Some("text/plain;charset=utf-8"));
        assert_eq!(choose(&mimes, true), None);
        assert!(!supported("application/octet-stream", false));
    }
    #[test]
    fn pipe_ownership_and_nonblocking() {
        let (read, write) = pipe().unwrap();
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(read.as_raw_fd(), (&raw mut byte).cast(), 1) },
            -1
        );
        let flags = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        drop(write);
        assert_eq!(
            unsafe { libc::read(read.as_raw_fd(), (&raw mut byte).cast(), 1) },
            0
        );
    }
    #[test]
    fn event_queue_is_bounded() {
        let mut state = State::default();
        for id in 0..MAX_EVENTS {
            state.push(Event::Removed(u32::try_from(id).unwrap()));
        }
        assert!(!state.failed);
        state.push(Event::Removed(0));
        assert!(state.failed);
        assert_eq!(state.events.len(), MAX_EVENTS);
    }
    #[test]
    fn callback_selection_state() {
        let mut state = State::default();
        let device = ObjectId::null();
        state.push(Event::Selection(device.clone(), None));
        assert!(matches!(state.events.pop(), Some(Event::Selection(id, None)) if id == device));
    }
    #[test]
    fn transfer_bounds_reject_overflow_without_allocating() {
        assert!(transfer_fits(LIMIT - 1, 1));
        assert!(!transfer_fits(LIMIT, 1));
        assert!(!transfer_fits(usize::MAX, 1));
        assert!(!transfer_fits(0, LIMIT + 1));
        let mut bytes = vec![1, 2];
        append(&mut bytes, &[3]).unwrap();
        assert_eq!(bytes, [1, 2, 3]);
    }
    #[test]
    fn generated_protocol_versions_and_signatures() {
        assert_eq!(eo::ExtDataControlOfferV1::interface().version, 1);
        assert_eq!(wo::ZwlrDataControlOfferV1::interface().version, 1);
        assert_eq!(ed::ExtDataControlDeviceV1::interface().events.len(), 4);
        assert_eq!(wd::ZwlrDataControlDeviceV1::interface().events.len(), 4);
    }
}
