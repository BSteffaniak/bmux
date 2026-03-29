use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext, PluginEvent,
    ServiceEnvelopeKind, ServiceResponse, decode_service_envelope, encode_service_envelope,
};
use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::{Mutex, OnceLock};

// ── Plugin exit codes ────────────────────────────────────────────────────────

/// Command completed successfully.
pub const EXIT_OK: i32 = 0;

/// Command failed with a generic error.
pub const EXIT_ERROR: i32 = 1;

/// Command received invalid arguments or was unknown.
pub const EXIT_USAGE: i32 = 64;

/// Plugin is unavailable (e.g. mutex poisoned, feature disabled).
pub const EXIT_UNAVAILABLE: i32 = 70;

// ── Plugin command error ─────────────────────────────────────────────────────

/// Error type for plugin command and lifecycle methods.
///
/// Carries an exit code and a human-readable message.  When a plugin method
/// returns `Err(PluginCommandError)`, the SDK prints the message to stderr
/// and passes the exit code back to the host.
///
/// Implements `From<String>` and `From<&str>` for easy use with the `?`
/// operator — string errors map to [`EXIT_ERROR`].
#[derive(Debug, Clone)]
pub struct PluginCommandError {
    pub code: i32,
    pub message: String,
}

impl PluginCommandError {
    /// Create an error with a specific exit code and message.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Generic failure ([`EXIT_ERROR`]).
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(EXIT_ERROR, message)
    }

    /// Unknown or unsupported command ([`EXIT_USAGE`]).
    #[must_use]
    pub fn unknown_command(name: &str) -> Self {
        Self::new(EXIT_USAGE, format!("unknown command '{name}'"))
    }

    /// Invalid arguments ([`EXIT_USAGE`]).
    #[must_use]
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }

    /// Plugin unavailable ([`EXIT_UNAVAILABLE`]).
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(EXIT_UNAVAILABLE, message)
    }
}

impl std::fmt::Display for PluginCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PluginCommandError {}

impl From<String> for PluginCommandError {
    fn from(message: String) -> Self {
        Self::failed(message)
    }
}

impl From<&str> for PluginCommandError {
    fn from(message: &str) -> Self {
        Self::failed(message)
    }
}

/// Convert a plugin Result into an FFI exit code.
///
/// - `Ok(code)` → returns `code`
/// - `Err(e)` → prints `e.message` to stderr, returns `e.code`
fn result_to_exit_code(result: Result<i32, PluginCommandError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}", error.message);
            error.code
        }
    }
}

// ── Internal FFI status codes (not exposed to plugin authors) ────────────────

const SERVICE_STATUS_OK: i32 = 0;
const SERVICE_STATUS_INVALID_ARGUMENT: i32 = 2;
const SERVICE_STATUS_DECODE_FAILED: i32 = 3;
const SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const SERVICE_STATUS_ENCODE_FAILED: i32 = 5;
const SERVICE_STATUS_PLUGIN_UNAVAILABLE: i32 = 70;

// ── Plugin trait ─────────────────────────────────────────────────────────────

/// The core trait that every bmux plugin implements.
///
/// All five methods have default implementations, so a plugin only needs to
/// override the methods relevant to its functionality:
///
/// - [`run_command`](Self::run_command) — handle CLI commands declared in `plugin.toml`
/// - [`invoke_service`](Self::invoke_service) — handle inbound service calls from other plugins
/// - [`activate`](Self::activate) / [`deactivate`](Self::deactivate) — lifecycle hooks
/// - [`handle_event`](Self::handle_event) — react to system or plugin events
///
/// ## Error patterns
///
/// Commands and lifecycle hooks return `Result<i32, PluginCommandError>` where
/// the `i32` is an exit code (use [`EXIT_OK`], [`EXIT_ERROR`], etc.).  On
/// `Err`, the SDK prints the error message to stderr and returns the error's
/// exit code to the host.
///
/// Service handlers return [`ServiceResponse`] directly — a structured RPC
/// response with an optional error payload.  Use [`handle_service`](crate::handle_service)
/// or [`route_service!`](crate::route_service) to reduce boilerplate.
pub trait RustPlugin: Default + Send + 'static {
    /// Handle a CLI command declared in the plugin manifest.
    ///
    /// The default returns `Err(PluginCommandError::unknown_command(""))`.
    fn run_command(&mut self, _context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    /// Called when the plugin is activated by the host.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Called when the plugin is deactivated by the host.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Called when a subscribed event fires.
    ///
    /// The default returns `Ok(EXIT_OK)`.
    fn handle_event(&mut self, _event: PluginEvent) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    /// Handle an inbound service call from another plugin or the host.
    ///
    /// The default returns an "unsupported_service" error response.
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "unsupported_service",
            format!(
                "plugin '{}' does not implement service '{}:{}'",
                context.plugin_id, context.request.service.interface_id, context.request.operation,
            ),
        )
    }
}

// ── FFI helpers ──────────────────────────────────────────────────────────────

#[doc(hidden)]
pub fn plugin_instance<P: RustPlugin>(instance: &'static OnceLock<Mutex<P>>) -> &'static Mutex<P> {
    instance.get_or_init(|| Mutex::new(P::default()))
}

#[doc(hidden)]
pub fn manifest_toml_ptr(
    manifest_toml: &'static str,
    cached: &'static OnceLock<Option<CString>>,
) -> *const c_char {
    let cached = cached.get_or_init(|| CString::new(manifest_toml).ok());
    cached
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
            instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.run_command(payload))
            })
        },
    )
}

#[doc(hidden)]
pub fn activate_export<P: RustPlugin>(instance: &'static Mutex<P>, context: *const c_char) -> i32 {
    parse_json_input::<NativeLifecycleContext>(context, 2, 3).map_or_else(
        |code| code,
        |payload| {
            instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.activate(payload))
            })
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
            instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.deactivate(payload))
            })
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
            instance.lock().map_or(EXIT_UNAVAILABLE, |mut plugin| {
                result_to_exit_code(plugin.handle_event(payload))
            })
        },
    )
}

#[doc(hidden)]
pub fn invoke_service_export<P: RustPlugin>(
    instance: &'static Mutex<P>,
    input_ptr: *const u8,
    input_len: usize,
    output_ptr: *mut u8,
    output_capacity: usize,
    output_len: *mut usize,
) -> i32 {
    if input_ptr.is_null() || output_len.is_null() {
        return SERVICE_STATUS_INVALID_ARGUMENT;
    }

    let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
    let (request_id, context) = match decode_service_envelope::<NativeServiceContext>(
        input,
        ServiceEnvelopeKind::Request,
    ) {
        Ok(value) => value,
        Err(_) => return SERVICE_STATUS_DECODE_FAILED,
    };

    let response = match instance.lock() {
        Ok(mut plugin) => plugin.invoke_service(context),
        Err(_) => return SERVICE_STATUS_PLUGIN_UNAVAILABLE,
    };

    let encoded =
        match encode_service_envelope(request_id, ServiceEnvelopeKind::Response, &response) {
            Ok(value) => value,
            Err(_) => return SERVICE_STATUS_ENCODE_FAILED,
        };

    unsafe {
        *output_len = encoded.len();
    }

    if output_ptr.is_null() || encoded.len() > output_capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }

    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
    }

    SERVICE_STATUS_OK
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
