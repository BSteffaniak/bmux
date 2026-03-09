use crate::{NativeCommandContext, NativeDescriptor, NativeLifecycleContext, PluginEvent};
use std::ffi::{CStr, CString, c_char};
use std::sync::{Mutex, OnceLock};

pub trait RustPlugin: Default + Send + 'static {
    fn descriptor(&self) -> NativeDescriptor;

    fn run_command(&mut self, _context: NativeCommandContext) -> i32 {
        64
    }

    fn activate(&mut self, _context: NativeLifecycleContext) -> i32 {
        0
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> i32 {
        0
    }

    fn handle_event(&mut self, _event: PluginEvent) -> i32 {
        0
    }
}

#[doc(hidden)]
pub fn plugin_instance<P: RustPlugin>(instance: &'static OnceLock<Mutex<P>>) -> &'static Mutex<P> {
    instance.get_or_init(|| Mutex::new(P::default()))
}

#[doc(hidden)]
pub fn descriptor_ptr<P: RustPlugin>(
    instance: &'static Mutex<P>,
    descriptor: &'static OnceLock<Option<CString>>,
) -> *const c_char {
    let descriptor = descriptor.get_or_init(|| {
        let plugin = instance.lock().ok()?;
        let text = plugin.descriptor().to_toml_string().ok()?;
        CString::new(text).ok()
    });
    descriptor
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr())
}

#[doc(hidden)]
pub fn run_command_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    context: *const c_char,
) -> i32 {
    parse_json_input::<NativeCommandContext>(context, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance
                .lock()
                .map_or(70, |mut plugin| plugin.run_command(payload))
        },
    )
}

#[doc(hidden)]
pub fn activate_export<P: RustPlugin>(instance: &'static Mutex<P>, context: *const c_char) -> i32 {
    parse_json_input::<NativeLifecycleContext>(context, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance
                .lock()
                .map_or(70, |mut plugin| plugin.activate(payload))
        },
    )
}

#[doc(hidden)]
pub fn deactivate_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    context: *const c_char,
) -> i32 {
    parse_json_input::<NativeLifecycleContext>(context, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance
                .lock()
                .map_or(70, |mut plugin| plugin.deactivate(payload))
        },
    )
}

#[doc(hidden)]
pub fn handle_event_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    event: *const c_char,
) -> i32 {
    parse_json_input::<PluginEvent>(event, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance
                .lock()
                .map_or(70, |mut plugin| plugin.handle_event(payload))
        },
    )
}

fn parse_json_input<T>(ptr: *const c_char, null_code: i32, parse_code: i32) -> Result<T, i32>
where
    T: serde::de::DeserializeOwned,
{
    let payload = c_str_to_string(ptr).map_err(|()| null_code)?;
    serde_json::from_str(&payload).map_err(|_| parse_code)
}

fn c_str_to_string(ptr: *const c_char) -> Result<String, ()> {
    if ptr.is_null() {
        return Err(());
    }

    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ())
}
