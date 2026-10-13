//! XCB transport. All connection calls are confined to one worker thread.
//! Fixed-width reply/event fields follow xproto.h; allocations returned by XCB
//! are released with `libc::free`, never with Rust's allocator.
use super::{LIMIT, failure};
use crate::ClipboardError;
use libloading::Library;
use std::{
    collections::HashMap,
    ffi::{CString, c_char, c_int, c_void},
    ptr,
    sync::{Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

type Connection = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy)]
struct Cookie {
    sequence: u32,
}
#[repr(C)]
struct Iterator {
    data: *mut c_void,
    rem: c_int,
    index: c_int,
}
macro_rules! api {
    ($($name:ident: $ty:ty),+ $(,)?) => {
        struct Api { _library: Library, $($name: $ty),+ }
        impl Api { unsafe fn load() -> Result<Self, ClipboardError> {
            let library = unsafe { Library::new("libxcb.so.1") }.map_err(|e| failure("native-x11", e.to_string()))?;
            $(let $name = *unsafe { library.get::<$ty>(concat!(stringify!($name), "\0").as_bytes()) }.map_err(|e| failure("native-x11", e.to_string()))?;)+
            Ok(Self { _library: library, $($name),+ })
        }}
    }
}
api! {
    xcb_connect: unsafe extern "C" fn(*const c_char, *mut c_int)->Connection,
    xcb_disconnect: unsafe extern "C" fn(Connection),
    xcb_connection_has_error: unsafe extern "C" fn(Connection)->c_int,
    xcb_get_setup: unsafe extern "C" fn(Connection)->*const c_void,
    xcb_setup_roots_iterator: unsafe extern "C" fn(*const c_void)->Iterator,
    xcb_screen_next: unsafe extern "C" fn(*mut Iterator),
    xcb_generate_id: unsafe extern "C" fn(Connection)->u32,
    xcb_create_window_checked: unsafe extern "C" fn(Connection,u8,u32,u32,i16,i16,u16,u16,u16,u16,u32,u32,*const c_void)->Cookie,
    xcb_change_window_attributes_checked: unsafe extern "C" fn(Connection,u32,u32,*const c_void)->Cookie,
    xcb_poll_for_reply: unsafe extern "C" fn(Connection,u32,*mut *mut c_void,*mut *mut c_void)->c_int,
    xcb_discard_reply: unsafe extern "C" fn(Connection,u32),
    xcb_get_input_focus: unsafe extern "C" fn(Connection)->Cookie,
    xcb_request_check: unsafe extern "C" fn(Connection,Cookie)->*mut c_void,
    xcb_intern_atom: unsafe extern "C" fn(Connection,u8,u16,*const c_char)->Cookie,
    xcb_get_selection_owner: unsafe extern "C" fn(Connection,u32)->Cookie,
    xcb_set_selection_owner: unsafe extern "C" fn(Connection,u32,u32,u32)->Cookie,
    xcb_convert_selection: unsafe extern "C" fn(Connection,u32,u32,u32,u32,u32)->Cookie,
    xcb_change_property_checked: unsafe extern "C" fn(Connection,u8,u32,u32,u32,u8,u32,*const c_void)->Cookie,
    xcb_delete_property: unsafe extern "C" fn(Connection,u32,u32)->Cookie,
    xcb_get_property: unsafe extern "C" fn(Connection,u8,u32,u32,u32,u32,u32)->Cookie,
    xcb_get_property_value: unsafe extern "C" fn(*const c_void)->*mut c_void,
    xcb_get_property_value_length: unsafe extern "C" fn(*const c_void)->c_int,
    xcb_send_event_checked: unsafe extern "C" fn(Connection,u8,u32,u32,*const c_char)->Cookie,
    xcb_poll_for_event: unsafe extern "C" fn(Connection)->*mut c_void,
    xcb_flush: unsafe extern "C" fn(Connection)->c_int,
    xcb_get_file_descriptor: unsafe extern "C" fn(Connection)->c_int,
    xcb_get_maximum_request_length: unsafe extern "C" fn(Connection)->u32,
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed XCB field"),
    )
}
fn put(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}
unsafe fn free(pointer: *mut c_void) {
    unsafe {
        libc::free(pointer);
    }
}
struct X {
    api: Api,
    connection: Connection,
    window: u32,
    atoms: HashMap<&'static str, u32>,
    chunk: usize,
    content: Option<(Vec<u8>, bool)>,
    sends: HashMap<(u32, u32), Send>,
}
impl Drop for X {
    fn drop(&mut self) {
        unsafe {
            (self.api.xcb_disconnect)(self.connection);
        }
    }
}
struct Send {
    bytes: Vec<u8>,
    offset: usize,
    target: u32,
    expires: Instant,
}
const TEXT: [&str; 6] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "TEXT",
    "STRING",
];
const TIMEOUT: Duration = Duration::from_secs(4);
impl X {
    fn new() -> Result<Self, ClipboardError> {
        unsafe {
            let api = Api::load()?;
            let mut screen = 0;
            let connection = (api.xcb_connect)(ptr::null(), &raw mut screen);
            if connection.is_null() || (api.xcb_connection_has_error)(connection) != 0 {
                if !connection.is_null() {
                    (api.xcb_disconnect)(connection);
                }
                return Err(failure(
                    "native-x11",
                    "cannot connect to DISPLAY".to_string(),
                ));
            }
            let mut roots = (api.xcb_setup_roots_iterator)((api.xcb_get_setup)(connection));
            for _ in 0..screen {
                (api.xcb_screen_next)(&raw mut roots);
            }
            if roots.rem <= 0 {
                (api.xcb_disconnect)(connection);
                return Err(failure("native-x11", "missing screen".to_string()));
            }
            let root = ptr::read_unaligned(roots.data.cast::<u32>());
            let window = (api.xcb_generate_id)(connection);
            let chunk = ((api.xcb_get_maximum_request_length)(connection) as usize)
                .saturating_mul(4)
                .saturating_sub(64)
                .min(64 * 1024);
            let mut x = Self {
                api,
                connection,
                window,
                atoms: HashMap::new(),
                chunk,
                content: None,
                sends: HashMap::new(),
            };
            // InputOnly window, CopyFromParent depth/visual, PropertyChangeMask.
            let mask = 1u32 << 22;
            x.check((x.api.xcb_create_window_checked)(
                connection,
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                2,
                0,
                1 << 11,
                (&raw const mask).cast(),
            ))?;
            for name in TEXT.into_iter().chain([
                "CLIPBOARD",
                "CLIPBOARD_MANAGER",
                "SAVE_TARGETS",
                "TARGETS",
                "INCR",
                "image/png",
                "BMUX_DATA",
            ]) {
                let name_c = CString::new(name).expect("static atom");
                let cookie = (x.api.xcb_intern_atom)(
                    connection,
                    0,
                    u16::try_from(name.len()).expect("short atom"),
                    name_c.as_ptr(),
                );
                let reply = x.reply(cookie)?;
                if reply.is_null() {
                    return Err(failure("native-x11", "atom lookup failed".to_string()));
                }
                let atom = ptr::read_unaligned(reply.cast::<u8>().add(8).cast::<u32>());
                free(reply);
                x.atoms.insert(name, atom);
            }
            if x.chunk == 0 {
                return Err(failure(
                    "native-x11",
                    "invalid server request limit".to_string(),
                ));
            }
            Ok(x)
        }
    }
    fn atom(&self, name: &str) -> u32 {
        self.atoms[name]
    }
    fn reply(&self, cookie: Cookie) -> Result<*mut c_void, ClipboardError> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let mut reply = ptr::null_mut();
            let mut error = ptr::null_mut();
            let completed = unsafe {
                (self.api.xcb_poll_for_reply)(
                    self.connection,
                    cookie.sequence,
                    &raw mut reply,
                    &raw mut error,
                )
            };
            if completed != 0 {
                if !error.is_null() {
                    let code = unsafe { *error.cast::<u8>().add(1) };
                    unsafe {
                        free(error);
                        free(reply);
                    }
                    return Err(failure("native-x11", format!("X protocol error {code}")));
                }
                if reply.is_null() {
                    return Err(failure("native-x11", "missing X reply".to_string()));
                }
                return Ok(reply);
            }
            if Instant::now() >= deadline {
                unsafe {
                    (self.api.xcb_discard_reply)(self.connection, cookie.sequence);
                }
                return Err(failure(
                    "native-x11",
                    "X reply deadline exceeded".to_string(),
                ));
            }
            if unsafe { (self.api.xcb_connection_has_error)(self.connection) } != 0 {
                return Err(failure("native-x11", "X connection lost".to_string()));
            }
            self.pause();
        }
    }
    fn check(&self, cookie: Cookie) -> Result<(), ClipboardError> {
        unsafe {
            // A completed later roundtrip proves the checked void request has
            // been processed. request_check can then collect its error without
            // waiting for an additional server roundtrip (xcb.h contract).
            let barrier = self.reply((self.api.xcb_get_input_focus)(self.connection))?;
            free(barrier);
            let error = (self.api.xcb_request_check)(self.connection, cookie);
            if error.is_null() {
                Ok(())
            } else {
                let code = *error.cast::<u8>().add(1);
                free(error);
                Err(failure("native-x11", format!("X protocol error {code}")))
            }
        }
    }
    fn owner(&self, selection: u32) -> Result<u32, ClipboardError> {
        unsafe {
            let reply = self.reply((self.api.xcb_get_selection_owner)(
                self.connection,
                selection,
            ))?;
            if reply.is_null() {
                return Err(failure("native-x11", "owner lookup failed".to_string()));
            }
            let owner = ptr::read_unaligned(reply.cast::<u8>().add(8).cast::<u32>());
            free(reply);
            Ok(owner)
        }
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
            return Err(failure("native-x11", "invalid property format".to_string()));
        }
        let count =
            u32::try_from(bytes.len() / unit).map_err(|e| failure("native-x11", e.to_string()))?;
        unsafe {
            self.check((self.api.xcb_change_property_checked)(
                self.connection,
                0,
                window,
                property,
                target,
                format,
                count,
                bytes.as_ptr().cast(),
            ))
        }
    }
    fn event(&self) -> Result<Option<[u8; 32]>, ClipboardError> {
        unsafe {
            let pointer = (self.api.xcb_poll_for_event)(self.connection);
            if pointer.is_null() {
                if (self.api.xcb_connection_has_error)(self.connection) != 0 {
                    return Err(failure("native-x11", "connection lost".to_string()));
                }
                return Ok(None);
            }
            let mut bytes = [0; 32];
            ptr::copy_nonoverlapping(pointer.cast::<u8>(), bytes.as_mut_ptr(), 32);
            free(pointer);
            Ok(Some(bytes))
        }
    }
    fn pause(&self) {
        unsafe {
            (self.api.xcb_flush)(self.connection);
            let mut poll = libc::pollfd {
                fd: (self.api.xcb_get_file_descriptor)(self.connection),
                events: libc::POLLIN,
                revents: 0,
            };
            libc::poll(&raw mut poll, 1, 10);
        }
    }
    // One dispatch state machine keeps selection acceptance, notification and
    // INCR acknowledgements together; splitting obscures their ordering.
    #[allow(clippy::too_many_lines)]
    fn serve(&mut self, event: [u8; 32]) {
        self.sends.retain(|_, send| send.expires > Instant::now());
        match event[0] & 127 {
            29 => {
                self.content = None; /* accepted outgoing transfers retain their snapshot */
            }
            30 => {
                let requestor = word(&event, 12);
                let target = word(&event, 20);
                let property = match word(&event, 24) {
                    0 => target,
                    n => n,
                };
                let mut accepted = false;
                if word(&event, 16) == self.atom("CLIPBOARD")
                    && let Some((bytes, image)) = &self.content
                {
                    let target_list: Vec<u32> = if *image {
                        vec![self.atom("image/png")]
                    } else {
                        TEXT.iter().map(|n| self.atom(n)).collect()
                    };
                    if target == self.atom("TARGETS") {
                        let atoms: Vec<u8> = target_list
                            .iter()
                            .copied()
                            .chain([self.atom("TARGETS"), self.atom("SAVE_TARGETS")])
                            .flat_map(u32::to_ne_bytes)
                            .collect();
                        accepted = self.property(requestor, property, 4, 32, &atoms).is_ok();
                    } else if target_list.contains(&target) {
                        let bytes = if target == self.atom("STRING") {
                            String::from_utf8_lossy(bytes)
                                .chars()
                                .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                                .collect()
                        } else {
                            bytes.clone()
                        };
                        if bytes.len() <= self.chunk {
                            accepted = self
                                .property(requestor, property, target, 8, &bytes)
                                .is_ok();
                        } else if self.sends.len() < 32
                            && !self.sends.contains_key(&(requestor, property))
                        {
                            let mask = 1u32 << 22;
                            unsafe {
                                let cookie = (self.api.xcb_change_window_attributes_checked)(
                                    self.connection,
                                    requestor,
                                    1 << 11,
                                    (&raw const mask).cast(),
                                );
                                if self.check(cookie).is_ok()
                                    && self
                                        .property(
                                            requestor,
                                            property,
                                            self.atom("INCR"),
                                            32,
                                            &u32::try_from(bytes.len())
                                                .expect("bounded bytes")
                                                .to_ne_bytes(),
                                        )
                                        .is_ok()
                                {
                                    self.sends.insert(
                                        (requestor, property),
                                        Send {
                                            bytes,
                                            offset: 0,
                                            target,
                                            expires: Instant::now() + TIMEOUT,
                                        },
                                    );
                                    accepted = true;
                                }
                            }
                        }
                    } else if target == self.atom("SAVE_TARGETS") {
                        accepted = self.property(requestor, property, 4, 32, &[]).is_ok();
                    }
                }
                let mut notify = [0; 32];
                notify[0] = 31;
                put(&mut notify, 4, word(&event, 4));
                put(&mut notify, 8, requestor);
                put(&mut notify, 12, word(&event, 16));
                put(&mut notify, 16, target);
                put(&mut notify, 20, if accepted { property } else { 0 });
                unsafe {
                    let cookie = (self.api.xcb_send_event_checked)(
                        self.connection,
                        0,
                        requestor,
                        0,
                        notify.as_ptr().cast(),
                    );
                    let _ = self.check(cookie);
                }
            }
            28 if event[16] == 1 => {
                let key = (word(&event, 4), word(&event, 8));
                if let Some(mut send) = self.sends.remove(&key) {
                    let end = (send.offset + self.chunk).min(send.bytes.len());
                    if self
                        .property(key.0, key.1, send.target, 8, &send.bytes[send.offset..end])
                        .is_ok()
                        && send.offset < send.bytes.len()
                    {
                        send.offset = end;
                        send.expires = Instant::now() + TIMEOUT;
                        self.sends.insert(key, send);
                    }
                }
            }
            _ => {}
        }
        unsafe {
            (self.api.xcb_flush)(self.connection);
        }
    }
    fn set(&mut self, bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
        self.content = Some((bytes, image));
        unsafe {
            (self.api.xcb_set_selection_owner)(
                self.connection,
                self.window,
                self.atom("CLIPBOARD"),
                0,
            );
        }
        if self.owner(self.atom("CLIPBOARD"))? != self.window {
            self.content = None;
            return Err(failure(
                "native-x11",
                "selection ownership refused".to_string(),
            ));
        }
        // Ask a clipboard manager to persist the advertised targets. The worker
        // remains alive and serves all requests while the manager saves them.
        if self.owner(self.atom("CLIPBOARD_MANAGER"))? != 0 {
            unsafe {
                (self.api.xcb_convert_selection)(
                    self.connection,
                    self.window,
                    self.atom("CLIPBOARD_MANAGER"),
                    self.atom("SAVE_TARGETS"),
                    0,
                    0,
                );
            }
        }
        unsafe {
            (self.api.xcb_flush)(self.connection);
        }
        Ok(())
    }
    fn fetch(&mut self, target: u32) -> Result<Option<(u8, Vec<u8>)>, ClipboardError> {
        let property = self.atom("BMUX_DATA");
        unsafe {
            (self.api.xcb_delete_property)(self.connection, self.window, property);
            (self.api.xcb_convert_selection)(
                self.connection,
                self.window,
                self.atom("CLIPBOARD"),
                target,
                property,
                0,
            );
        }
        let mut transfer = Receive::default();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(failure(
                    "native-x11",
                    "selection transfer timed out".to_string(),
                ));
            }
            let Some(event) = self.event()? else {
                self.pause();
                continue;
            };
            let initial = (event[0] & 127) == 31
                && word(&event, 8) == self.window
                && word(&event, 12) == self.atom("CLIPBOARD")
                && word(&event, 16) == target;
            let chunk = (event[0] & 127) == 28
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
            unsafe {
                let cookie = (self.api.xcb_get_property)(
                    self.connection,
                    1,
                    self.window,
                    property,
                    0,
                    0,
                    u32::try_from(LIMIT / 4).expect("bounded limit"),
                );
                let reply = self.reply(cookie)?;
                if reply.is_null() {
                    return Err(failure("native-x11", "property read failed".to_string()));
                }
                let header = std::slice::from_raw_parts(reply.cast::<u8>(), 32);
                let format = header[1];
                let kind = word(header, 8);
                let remaining = word(header, 12);
                let length = (self.api.xcb_get_property_value_length)(reply);
                if length < 0 || remaining != 0 || length.cast_unsigned() as usize > LIMIT {
                    free(reply);
                    return Err(failure(
                        "native-x11",
                        "oversized or incomplete property".to_string(),
                    ));
                }
                let data = std::slice::from_raw_parts(
                    (self.api.xcb_get_property_value)(reply).cast::<u8>(),
                    length.cast_unsigned() as usize,
                )
                .to_vec();
                free(reply);
                if initial && kind == self.atom("INCR") {
                    transfer.start(format, &data)?;
                    (self.api.xcb_delete_property)(self.connection, self.window, property);
                } else {
                    if kind != expected_property_type(target, self.atom("TARGETS"))
                        && !(target == self.atom("TEXT")
                            && (kind == self.atom("STRING") || kind == self.atom("UTF8_STRING")))
                    {
                        return Err(failure(
                            "native-x11",
                            "selection property type mismatch".to_string(),
                        ));
                    }
                    if transfer.push(format, &data)? {
                        return Ok(Some((format, transfer.bytes)));
                    }
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
            // A local read must not depend on serving our own protocol request.
            // Verify server ownership first: an unprocessed SelectionClear may
            // otherwise expose stale local content after another client copies.
            return Ok(local_content(self.content.as_ref(), image));
        }
        let targets = self.fetch(self.atom("TARGETS"))?;
        let offered = match targets {
            Some((32, bytes)) if bytes.len().is_multiple_of(4) => Some(
                bytes
                    .chunks_exact(4)
                    .map(|b| word(b, 0))
                    .collect::<Vec<_>>(),
            ),
            Some(_) => {
                return Err(failure(
                    "native-x11",
                    "invalid TARGETS property".to_string(),
                ));
            }
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
                    return Err(failure("native-x11", "invalid data format".to_string()));
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
        if self.incremental || format != 32 || bytes.len() != 4 || word(bytes, 0) as usize > LIMIT {
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
        assert_eq!(std::mem::size_of::<Cookie>(), 4);
        assert_eq!(
            std::mem::offset_of!(Iterator, rem),
            std::mem::size_of::<*mut c_void>()
        );
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
