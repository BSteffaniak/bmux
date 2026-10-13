//! libwayland-client ABI reviewed against wayland-client-core.h/wayland-util.h
//! 1.24.0. Descriptors follow ext-data-control-v1.xml and
//! wlr-data-control-unstable-v1.xml from cached wayland-protocols 0.32.13 /
//! wayland-protocols-wlr 0.3.12. We bind wlr v1 (regular clipboard only).
//! All proxies, callbacks and descriptor allocations are confined to one thread.
use super::{LIMIT, failure};
use crate::ClipboardError;
use libloading::Library;
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

type Proxy = *mut c_void;
#[repr(C)]
struct Message {
    name: *const c_char,
    signature: *const c_char,
    types: *const *const Interface,
}
#[repr(C)]
struct Interface {
    name: *const c_char,
    version: c_int,
    method_count: c_int,
    methods: *const Message,
    event_count: c_int,
    events: *const Message,
}
#[repr(C)]
#[derive(Clone, Copy)]
union Argument {
    u: u32,
    s: *const c_char,
    o: Proxy,
    h: c_int,
}
type Dispatcher =
    unsafe extern "C" fn(*const c_void, Proxy, u32, *const Message, *mut Argument) -> c_int;
type AddDispatcher = unsafe extern "C" fn(Proxy, Dispatcher, *const c_void, *mut c_void) -> c_int;
macro_rules! api {
    ($($name:ident:$ty:ty),+ $(,)?)=>{
        struct Api {_library:Library,$($name:$ty),+,registry:*const Interface,callback:*const Interface,seat:*const Interface}
        impl Api {unsafe fn load()->Result<Self,ClipboardError>{
            let library=unsafe{Library::new("libwayland-client.so.0")}.map_err(|e|failure("native-wayland", e.to_string()))?;
            $(let $name=*unsafe{library.get::<$ty>(concat!(stringify!($name),"\0").as_bytes())}.map_err(|e|failure("native-wayland", e.to_string()))?;)+
            let registry=*unsafe{library.get::<*const Interface>(b"wl_registry_interface\0")}.map_err(|e|failure("native-wayland", e.to_string()))?;
            let callback=*unsafe{library.get::<*const Interface>(b"wl_callback_interface\0")}.map_err(|e|failure("native-wayland", e.to_string()))?;
            let seat=*unsafe{library.get::<*const Interface>(b"wl_seat_interface\0")}.map_err(|e|failure("native-wayland", e.to_string()))?;
            Ok(Self{_library:library,$($name),+,registry,callback,seat})
        }}
    }
}
api! {
    wl_display_connect:unsafe extern "C" fn(*const c_char)->Proxy,
    wl_display_disconnect:unsafe extern "C" fn(Proxy),
    wl_display_get_fd:unsafe extern "C" fn(Proxy)->c_int,
    wl_display_dispatch_pending:unsafe extern "C" fn(Proxy)->c_int,
    wl_display_prepare_read:unsafe extern "C" fn(Proxy)->c_int,
    wl_display_read_events:unsafe extern "C" fn(Proxy)->c_int,
    wl_display_cancel_read:unsafe extern "C" fn(Proxy),
    wl_display_flush:unsafe extern "C" fn(Proxy)->c_int,
    wl_proxy_marshal_array_flags:unsafe extern "C" fn(Proxy,u32,*const Interface,u32,u32,*mut Argument)->Proxy,
    wl_proxy_destroy:unsafe extern "C" fn(Proxy),
    wl_proxy_add_dispatcher:AddDispatcher,
}
// The boxed descriptor graph and its names/messages/type arrays outlive every
// proxy. Heap addresses remain stable when the containing struct moves.
struct Descriptors {
    // Fixed heap storage keeps the descriptor graph stable across owner moves.
    interfaces: Box<[Interface; 4]>,
    names: Vec<CString>,
    messages: Vec<Box<[Message]>>,
    types: Vec<Box<[*const Interface]>>,
}
impl Descriptors {
    fn new(ext: bool, seat: *const Interface) -> Self {
        let prefix = if ext { "ext" } else { "zwlr" };
        let names = ["manager", "device", "source", "offer"].map(|kind| {
            CString::new(format!("{prefix}_data_control_{kind}_v1")).expect("protocol name")
        });
        let mut d = Self {
            interfaces: Box::new(names.each_ref().map(|name| Interface {
                name: name.as_ptr(),
                version: 1,
                method_count: 0,
                methods: ptr::null(),
                event_count: 0,
                events: ptr::null(),
            })),
            names: names.into(),
            messages: Vec::new(),
            types: Vec::new(),
        };
        let device = d.interface(1);
        let source = d.interface(2);
        let offer = d.interface(3);
        d.install(
            0,
            false,
            &[
                ("create_data_source", "n", vec![source]),
                ("get_data_device", "no", vec![device, seat]),
                ("destroy", "", vec![]),
            ],
        );
        d.install(
            1,
            false,
            &[
                ("set_selection", "?o", vec![source]),
                ("destroy", "", vec![]),
            ],
        );
        // ext primary selection is part of v1. wlr primary selection is v2 and
        // intentionally omitted because only v1 is negotiated here.
        if ext {
            d.install(
                1,
                true,
                &[
                    ("data_offer", "n", vec![offer]),
                    ("selection", "?o", vec![offer]),
                    ("finished", "", vec![]),
                    ("primary_selection", "?o", vec![offer]),
                ],
            );
            d.install(
                1,
                false,
                &[
                    ("set_selection", "?o", vec![source]),
                    ("destroy", "", vec![]),
                    ("set_primary_selection", "?o", vec![source]),
                ],
            );
        } else {
            d.install(
                1,
                true,
                &[
                    ("data_offer", "n", vec![offer]),
                    ("selection", "?o", vec![offer]),
                    ("finished", "", vec![]),
                ],
            );
        }
        d.install(
            2,
            false,
            &[("offer", "s", vec![ptr::null()]), ("destroy", "", vec![])],
        );
        d.install(
            2,
            true,
            &[
                ("send", "sh", vec![ptr::null(), ptr::null()]),
                ("cancelled", "", vec![]),
            ],
        );
        d.install(
            3,
            false,
            &[
                ("receive", "sh", vec![ptr::null(), ptr::null()]),
                ("destroy", "", vec![]),
            ],
        );
        d.install(3, true, &[("offer", "s", vec![ptr::null()])]);
        d
    }
    fn interface(&self, index: usize) -> *const Interface {
        &raw const self.interfaces[index]
    }
    fn install(
        &mut self,
        index: usize,
        events: bool,
        specs: &[(&str, &str, Vec<*const Interface>)],
    ) {
        let messages: Box<[Message]> = specs
            .iter()
            .map(|(name, signature, types)| {
                self.names
                    .push(CString::new(*name).expect("protocol message"));
                let name = self.names.last().expect("inserted name").as_ptr();
                self.names
                    .push(CString::new(*signature).expect("protocol signature"));
                let signature = self.names.last().expect("inserted signature").as_ptr();
                self.types.push(types.clone().into_boxed_slice());
                let types = self.types.last().expect("inserted types").as_ptr();
                Message {
                    name,
                    signature,
                    types,
                }
            })
            .collect();
        let interface = &mut self.interfaces[index];
        if events {
            interface.event_count = c_int::try_from(messages.len()).expect("small descriptor");
            interface.events = messages.as_ptr();
        } else {
            interface.method_count = c_int::try_from(messages.len()).expect("small descriptor");
            interface.methods = messages.as_ptr();
        }
        self.messages.push(messages);
    }
}
#[derive(Clone, Copy)]
enum Role {
    Registry,
    Sync,
    Seat,
    Device,
    Source,
    Offer,
}
enum Event {
    Global(u32, String, u32),
    Remove(u32),
    Synced(usize),
    NewOffer(usize),
    Mime(usize, String),
    Selection(usize, usize),
    Primary(usize),
    Finished(usize),
    Send(usize, String, OwnedFd),
    Cancelled(usize),
}
struct Inbox {
    roles: HashMap<usize, Role>,
    events: Vec<Event>,
    add: AddDispatcher,
    failed: bool,
}
unsafe fn string(argument: Argument) -> String {
    unsafe { CStr::from_ptr(argument.s) }
        .to_string_lossy()
        .into_owned()
}
unsafe extern "C" fn dispatch(
    data: *const c_void,
    proxy: Proxy,
    opcode: u32,
    _message: *const Message,
    args: *mut Argument,
) -> c_int {
    // No Rust panic or unwind crosses the C ABI. Limits are checked before
    // collecting variable peer data. All protocol fd arguments become OwnedFd.
    unsafe {
        let inbox = &mut *data.cast_mut().cast::<Inbox>();
        let Some(role) = inbox.roles.get(&(proxy as usize)).copied() else {
            return -1;
        };
        let event = match (role, opcode) {
            (Role::Registry, 0) => Some(Event::Global(
                (*args).u,
                string(*args.add(1)),
                (*args.add(2)).u,
            )),
            (Role::Registry, 1) => Some(Event::Remove((*args).u)),
            (Role::Sync, 0) => Some(Event::Synced(proxy as usize)),
            (Role::Device, 0) => {
                let offer = (*args).o;
                if offer.is_null() || inbox.roles.len() >= 512 {
                    inbox.failed = true;
                    return -1;
                }
                inbox.roles.insert(offer as usize, Role::Offer);
                if (inbox.add)(offer, dispatch, data, ptr::null_mut()) != 0 {
                    inbox.failed = true;
                    return -1;
                }
                Some(Event::NewOffer(offer as usize))
            }
            (Role::Device, 1) => Some(Event::Selection(proxy as usize, (*args).o as usize)),
            (Role::Device, 2) => Some(Event::Finished(proxy as usize)),
            (Role::Device, 3) => Some(Event::Primary((*args).o as usize)),
            (Role::Offer, 0) => Some(Event::Mime(proxy as usize, string(*args))),
            (Role::Source, 0) => Some(Event::Send(
                proxy as usize,
                string(*args),
                OwnedFd::from_raw_fd((*args.add(1)).h),
            )),
            (Role::Source, 1) => Some(Event::Cancelled(proxy as usize)),
            (Role::Seat, _) => None,
            _ => {
                inbox.failed = true;
                return -1;
            }
        };
        if let Some(event) = event {
            if inbox.events.len() >= 1024 {
                inbox.failed = true;
                return -1;
            }
            inbox.events.push(event);
        }
        0
    }
}
struct Offer {
    mimes: Vec<String>,
}
struct Seat {
    global: u32,
    proxy: Proxy,
    device: Proxy,
    selection: usize,
}
struct Source {
    bytes: Arc<Vec<u8>>,
    image: bool,
}
struct Send {
    fd: OwnedFd,
    bytes: Arc<Vec<u8>>,
    offset: usize,
    expires: Instant,
}
struct W {
    api: Api,
    display: Proxy,
    registry: Proxy,
    manager: Proxy,
    descriptors: Option<Descriptors>,
    inbox: Box<Inbox>,
    globals: Vec<(u32, String, u32)>,
    seats: Vec<Seat>,
    offers: HashMap<usize, Offer>,
    sources: HashMap<usize, Source>,
    sends: Vec<Send>,
    synced: Vec<usize>,
}
const TIMEOUT: Duration = Duration::from_secs(4);
const MIMES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "UTF8_STRING",
];
impl Drop for W {
    fn drop(&mut self) {
        // Local proxy teardown precedes display/library/descriptor destruction.
        for (proxy, _) in self.inbox.roles.drain() {
            unsafe {
                (self.api.wl_proxy_destroy)(proxy as Proxy);
            }
        }
        if !self.manager.is_null() {
            unsafe {
                (self.api.wl_proxy_destroy)(self.manager);
            }
        }
        unsafe {
            (self.api.wl_display_disconnect)(self.display);
        }
    }
}
impl W {
    fn new() -> Result<Self, ClipboardError> {
        unsafe {
            let api = Api::load()?;
            let display = (api.wl_display_connect)(ptr::null());
            if display.is_null() {
                return Err(failure(
                    "native-wayland",
                    "cannot connect to WAYLAND_DISPLAY".to_string(),
                ));
            }
            let inbox = Box::new(Inbox {
                roles: HashMap::new(),
                events: Vec::new(),
                add: api.wl_proxy_add_dispatcher,
                failed: false,
            });
            let mut w = Self {
                api,
                display,
                registry: ptr::null_mut(),
                manager: ptr::null_mut(),
                descriptors: None,
                inbox,
                globals: Vec::new(),
                seats: Vec::new(),
                offers: HashMap::new(),
                sources: HashMap::new(),
                sends: Vec::new(),
                synced: Vec::new(),
            };
            w.registry = w.marshal(
                display,
                1,
                w.api.registry,
                &mut [Argument { o: ptr::null_mut() }],
                false,
            )?;
            w.listen(w.registry, Role::Registry)?;
            w.sync()?;
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
                .ok_or_else(|| {
                    failure(
                        "native-wayland",
                        "compositor has no data-control protocol".to_string(),
                    )
                })?;
            w.descriptors = Some(Descriptors::new(global.1.starts_with("ext_"), w.api.seat));
            w.manager = w.bind(
                global.0,
                w.descriptors.as_ref().expect("descriptors").interface(0),
            )?;
            let seats = w
                .globals
                .iter()
                .filter(|(_, name, _)| name == "wl_seat")
                .map(|(id, _, _)| *id)
                .collect::<Vec<_>>();
            if seats.is_empty() || seats.len() > 32 {
                return Err(failure(
                    "native-wayland",
                    "missing or excessive seats".to_string(),
                ));
            }
            for global in seats {
                w.add_seat(global)?;
            }
            w.sync()?;
            Ok(w)
        }
    }
    fn marshal(
        &self,
        proxy: Proxy,
        opcode: u32,
        interface: *const Interface,
        args: &mut [Argument],
        destroy: bool,
    ) -> Result<Proxy, ClipboardError> {
        let result = unsafe {
            (self.api.wl_proxy_marshal_array_flags)(
                proxy,
                opcode,
                interface,
                1,
                u32::from(destroy),
                args.as_mut_ptr(),
            )
        };
        if !interface.is_null() && result.is_null() {
            return Err(failure(
                "native-wayland",
                "proxy allocation failed".to_string(),
            ));
        }
        Ok(result)
    }
    fn listen(&mut self, proxy: Proxy, role: Role) -> Result<(), ClipboardError> {
        self.inbox.roles.insert(proxy as usize, role);
        if unsafe {
            (self.api.wl_proxy_add_dispatcher)(
                proxy,
                dispatch,
                (&raw const *self.inbox).cast(),
                ptr::null_mut(),
            )
        } != 0
        {
            return Err(failure(
                "native-wayland",
                "dispatcher installation failed".to_string(),
            ));
        }
        Ok(())
    }
    fn bind(&self, name: u32, interface: *const Interface) -> Result<Proxy, ClipboardError> {
        self.marshal(
            self.registry,
            0,
            interface,
            &mut [
                Argument { u: name },
                Argument {
                    s: unsafe { (*interface).name },
                },
                Argument { u: 1 },
                Argument { o: ptr::null_mut() },
            ],
            false,
        )
    }
    fn add_seat(&mut self, global: u32) -> Result<(), ClipboardError> {
        if self.seats.len() >= 32 {
            return Err(failure("native-wayland", "seat limit exceeded".to_string()));
        }
        let proxy = self.bind(global, self.api.seat)?;
        self.listen(proxy, Role::Seat)?;
        let device = self.marshal(
            self.manager,
            1,
            self.descriptors.as_ref().expect("descriptors").interface(1),
            &mut [Argument { o: ptr::null_mut() }, Argument { o: proxy }],
            false,
        )?;
        self.listen(device, Role::Device)?;
        self.seats.push(Seat {
            global,
            proxy,
            device,
            selection: 0,
        });
        Ok(())
    }
    fn destroy(&mut self, proxy: usize, opcode: u32) {
        if proxy == 0 || self.inbox.roles.remove(&proxy).is_none() {
            return;
        }
        let _ = self.marshal(proxy as Proxy, opcode, ptr::null(), &mut [], true);
        self.offers.remove(&proxy);
        self.sources.remove(&proxy);
    }
    // The match is the protocol lifecycle state machine: keeping creation,
    // replacement and destruction side by side makes proxy ownership auditable.
    #[allow(clippy::too_many_lines)]
    fn events(&mut self) -> Result<(), ClipboardError> {
        if self.inbox.failed {
            return Err(failure(
                "native-wayland",
                "peer event resource limit or invalid event".to_string(),
            ));
        }
        for event in std::mem::take(&mut self.inbox.events) {
            match event {
                Event::Global(id, name, version) => {
                    if self.globals.len() >= 256 || name.len() > 1024 {
                        return Err(failure(
                            "native-wayland",
                            "global resource limit exceeded".to_string(),
                        ));
                    }
                    if self.descriptors.is_some() && name == "wl_seat" {
                        self.add_seat(id)?;
                    }
                    self.globals.push((id, name, version));
                }
                Event::Remove(id) => {
                    self.globals.retain(|(global, _, _)| *global != id);
                    if let Some(index) = self.seats.iter().position(|seat| seat.global == id) {
                        let seat = self.seats.remove(index);
                        self.destroy(seat.selection, 1);
                        self.destroy(seat.device as usize, 1);
                        self.inbox.roles.remove(&(seat.proxy as usize));
                        unsafe {
                            (self.api.wl_proxy_destroy)(seat.proxy);
                        }
                    }
                }
                Event::Synced(proxy) => {
                    self.inbox.roles.remove(&proxy);
                    unsafe {
                        (self.api.wl_proxy_destroy)(proxy as Proxy);
                    }
                    self.synced.push(proxy);
                }
                Event::NewOffer(proxy) => {
                    if self.offers.len() >= 128 {
                        return Err(failure(
                            "native-wayland",
                            "offer limit exceeded".to_string(),
                        ));
                    }
                    self.offers.insert(proxy, Offer { mimes: Vec::new() });
                }
                Event::Mime(proxy, mime) => {
                    if let Some(offer) = self.offers.get_mut(&proxy) {
                        if mime.len() > 1024 || offer.mimes.len() >= 128 {
                            return Err(failure(
                                "native-wayland",
                                "MIME limit exceeded".to_string(),
                            ));
                        }
                        offer.mimes.push(mime);
                    }
                }
                Event::Selection(device, offer) => {
                    if let Some(seat) = self
                        .seats
                        .iter_mut()
                        .find(|seat| seat.device as usize == device)
                    {
                        let old = seat.selection;
                        seat.selection = offer;
                        if old != offer {
                            self.destroy(old, 1);
                        }
                    }
                }
                Event::Primary(offer) => self.destroy(offer, 1),
                Event::Finished(device) => {
                    if let Some(index) = self
                        .seats
                        .iter()
                        .position(|seat| seat.device as usize == device)
                    {
                        let seat = self.seats.remove(index);
                        self.destroy(seat.selection, 1);
                        self.destroy(device, 1);
                        self.inbox.roles.remove(&(seat.proxy as usize));
                        unsafe {
                            (self.api.wl_proxy_destroy)(seat.proxy);
                        }
                    }
                }
                Event::Cancelled(source) => self.destroy(source, 1),
                Event::Send(source, mime, fd) => {
                    if let Some(source) = self.sources.get(&source)
                        && supported(&mime, source.image)
                        && self.sends.len() < 32
                    {
                        nonblocking(&fd)?;
                        self.sends.push(Send {
                            fd,
                            bytes: Arc::clone(&source.bytes),
                            offset: 0,
                            expires: Instant::now() + TIMEOUT,
                        });
                    }
                }
            }
        }
        Ok(())
    }
    fn pump(&mut self) -> Result<(), ClipboardError> {
        unsafe {
            while (self.api.wl_display_prepare_read)(self.display) != 0 {
                if (self.api.wl_display_dispatch_pending)(self.display) < 0 {
                    return Err(failure("native-wayland", "dispatch failed".to_string()));
                }
                self.events()?;
            }
            // No fallible operations occur between prepare_read and its paired
            // read_events/cancel_read, so the display cannot remain read-locked.
            let flushed = (self.api.wl_display_flush)(self.display);
            let mut poll = libc::pollfd {
                fd: (self.api.wl_display_get_fd)(self.display),
                events: libc::POLLIN | if flushed < 0 { libc::POLLOUT } else { 0 },
                revents: 0,
            };
            let ready = libc::poll(&raw mut poll, 1, 10);
            if ready > 0 && poll.revents & libc::POLLIN != 0 {
                if (self.api.wl_display_read_events)(self.display) < 0 {
                    return Err(failure("native-wayland", "display read failed".to_string()));
                }
            } else {
                (self.api.wl_display_cancel_read)(self.display);
            }
            if ready > 0 && poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(failure(
                    "native-wayland",
                    "display disconnected".to_string(),
                ));
            }
            if (self.api.wl_display_dispatch_pending)(self.display) < 0 {
                return Err(failure("native-wayland", "dispatch failed".to_string()));
            }
        }
        self.events()?;
        self.sends.retain_mut(|send| {
            if Instant::now() >= send.expires {
                return false;
            }
            // Bound one iteration per slow consumer. SIGPIPE is blocked on this
            // worker thread only, so disconnected readers cannot kill bmux.
            let remaining = &send.bytes[send.offset..];
            let written = unsafe {
                libc::write(
                    send.fd.as_raw_fd(),
                    remaining.as_ptr().cast(),
                    remaining.len().min(64 * 1024),
                )
            };
            match written.cmp(&0) {
                std::cmp::Ordering::Greater => {
                    send.offset += written.cast_unsigned();
                    send.expires = Instant::now() + TIMEOUT;
                    send.offset < send.bytes.len()
                }
                std::cmp::Ordering::Equal => false,
                std::cmp::Ordering::Less => {
                    let error = std::io::Error::last_os_error();
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    )
                }
            }
        });
        Ok(())
    }
    fn sync(&mut self) -> Result<(), ClipboardError> {
        let callback = self.marshal(
            self.display,
            0,
            self.api.callback,
            &mut [Argument { o: ptr::null_mut() }],
            false,
        )?;
        self.listen(callback, Role::Sync)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump()?;
            if let Some(index) = self
                .synced
                .iter()
                .position(|proxy| *proxy == callback as usize)
            {
                self.synced.remove(index);
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "native-wayland",
                    "synchronization timed out".to_string(),
                ));
            }
        }
    }
    fn set(&mut self, bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
        self.sync()?;
        if self.seats.is_empty() {
            return Err(failure("native-wayland", "no active seats".to_string()));
        }
        let bytes = Arc::new(bytes);
        let devices = self
            .seats
            .iter()
            .map(|seat| seat.device)
            .collect::<Vec<_>>();
        for device in devices {
            let source = self.marshal(
                self.manager,
                0,
                self.descriptors.as_ref().expect("descriptors").interface(2),
                &mut [Argument { o: ptr::null_mut() }],
                false,
            )?;
            self.listen(source, Role::Source)?;
            self.sources.insert(
                source as usize,
                Source {
                    bytes: Arc::clone(&bytes),
                    image,
                },
            );
            for mime in if image {
                vec!["image/png"]
            } else {
                MIMES.to_vec()
            } {
                let mime = CString::new(mime).expect("static MIME");
                self.marshal(
                    source,
                    0,
                    ptr::null(),
                    &mut [Argument { s: mime.as_ptr() }],
                    false,
                )?;
            }
            self.marshal(device, 0, ptr::null(), &mut [Argument { o: source }], false)?;
        }
        self.sync()
    }
    fn get(&mut self, image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
        self.sync()?;
        if self.seats.is_empty() {
            return Err(failure("native-wayland", "no active seats".to_string()));
        }
        let choice = self.seats.iter().find_map(|seat| {
            let offer = self.offers.get(&seat.selection)?;
            choose(&offer.mimes, image).map(|mime| (seat.selection, mime.to_owned()))
        });
        let Some((offer, mime)) = choice else {
            return Ok(None);
        };
        let (read, write) = pipe()?;
        let mime = CString::new(mime).map_err(|e| failure("native-wayland", e.to_string()))?;
        self.marshal(
            offer as Proxy,
            0,
            ptr::null(),
            &mut [
                Argument { s: mime.as_ptr() },
                Argument {
                    h: write.as_raw_fd(),
                },
            ],
            false,
        )?;
        drop(write);
        let mut bytes = Vec::new();
        let mut buffer = vec![0; 64 * 1024].into_boxed_slice();
        let mut deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump()?;
            let length =
                unsafe { libc::read(read.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if length == 0 {
                return Ok(Some(bytes));
            }
            if length > 0 {
                append(&mut bytes, &buffer[..length.cast_unsigned()])?;
                deadline = Instant::now() + TIMEOUT;
            } else {
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) {
                    return Err(failure("native-wayland", error.to_string()));
                }
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "native-wayland",
                    "data transfer timed out".to_string(),
                ));
            }
        }
    }
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
fn append(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ClipboardError> {
    if bytes
        .len()
        .checked_add(chunk.len())
        .is_none_or(|n| n > LIMIT)
    {
        return Err(failure(
            "native-wayland",
            "transfer exceeds limit".to_string(),
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}
fn nonblocking(fd: &OwnedFd) -> Result<(), ClipboardError> {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        let descriptor = libc::fcntl(fd.as_raw_fd(), libc::F_GETFD);
        if flags < 0
            || descriptor < 0
            || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            || libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, descriptor | libc::FD_CLOEXEC) < 0
        {
            return Err(failure(
                "native-wayland",
                std::io::Error::last_os_error().to_string(),
            ));
        }
    }
    Ok(())
}
fn pipe() -> Result<(OwnedFd, OwnedFd), ClipboardError> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(failure(
            "native-wayland",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    nonblocking(&read)?;
    // O_NONBLOCK is shared by SCM_RIGHTS duplicates. External producers need
    // a blocking write end; only our receive end may be nonblocking.
    if unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(failure(
            "native-wayland",
            std::io::Error::last_os_error().to_string(),
        ));
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
    let mut worker = WORKER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|e| failure("native-wayland", e.to_string()))?;
    if worker.is_none() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clipboard-wayland".into())
            .spawn(move || {
                unsafe {
                    let mut signals = std::mem::zeroed();
                    libc::sigemptyset(&raw mut signals);
                    libc::sigaddset(&raw mut signals, libc::SIGPIPE);
                    let error =
                        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const signals, ptr::null_mut());
                    if error != 0 {
                        let _ = ready_tx.send(Err(failure(
                            "native-wayland",
                            format!("blocking SIGPIPE failed: {error}"),
                        )));
                        return;
                    }
                }
                let mut w = match W::new() {
                    Ok(w) => {
                        let _ = ready_tx.send(Ok(()));
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
            .map_err(|e| failure("native-wayland", e.to_string()))?;
        ready_rx
            .recv_timeout(Duration::from_secs(12))
            .map_err(|e| failure("native-wayland", e.to_string()))??;
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
        return Err(failure("native-wayland", "worker stopped".to_string()));
    }
    match receiver.recv_timeout(Duration::from_secs(12)) {
        Ok(result) => {
            drop(worker);
            result
        }
        Err(e) => {
            *worker = None;
            drop(worker);
            Err(failure("native-wayland", e.to_string()))
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
    fn abi_layouts() {
        assert_eq!(
            std::mem::size_of::<Argument>(),
            std::mem::size_of::<*mut c_void>()
        );
        assert_eq!(
            std::mem::size_of::<Message>(),
            3 * std::mem::size_of::<*mut c_void>()
        );
        assert_eq!(
            std::mem::offset_of!(Interface, version),
            std::mem::size_of::<*mut c_void>()
        );
    }
    #[test]
    fn descriptors_match_negotiated_versions() {
        for ext in [false, true] {
            let d = Descriptors::new(ext, ptr::null());
            unsafe {
                assert_eq!((*d.interface(0)).method_count, 3);
                assert_eq!((*d.interface(1)).event_count, if ext { 4 } else { 3 });
                assert_eq!(
                    CStr::from_ptr((*(*d.interface(2)).events).signature).to_bytes(),
                    b"sh"
                );
                assert_eq!((*d.interface(3)).version, 1);
                let first = &*(*d.interface(1)).events;
                assert_eq!(*first.types, d.interface(3));
            }
        }
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
        drop(write);
        assert_eq!(
            unsafe { libc::read(read.as_raw_fd(), (&raw mut byte).cast(), 1) },
            0
        );
    }
    #[test]
    fn callback_selection_state() {
        let mut inbox = Box::new(Inbox {
            roles: HashMap::new(),
            events: Vec::new(),
            add: dummy_add,
            failed: false,
        });
        inbox.roles.insert(12, Role::Device);
        let mut args = [Argument {
            o: 24usize as Proxy,
        }];
        unsafe {
            assert_eq!(
                dispatch(
                    (&raw const *inbox).cast(),
                    12usize as Proxy,
                    1,
                    ptr::null(),
                    args.as_mut_ptr()
                ),
                0
            );
        }
        assert!(matches!(inbox.events.pop(), Some(Event::Selection(12, 24))));
    }
    unsafe extern "C" fn dummy_add(
        _: Proxy,
        _: Dispatcher,
        _: *const c_void,
        _: *mut c_void,
    ) -> c_int {
        0
    }
}
