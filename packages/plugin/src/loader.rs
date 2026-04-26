use crate::{
    CapabilityProvider, DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_COMMAND_SYMBOL,
    DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL, DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
    DEFAULT_NATIVE_EVENT_SYMBOL, DEFAULT_NATIVE_SERVICE_SYMBOL, PluginDeclaration,
    PluginEntrypoint, PluginRegistry, RegisteredPlugin, ServiceCaller,
    discover_registered_plugins_in_roots, test_support::test_service_router,
};
use bmux_ipc::{
    InvokeServiceKind, Request as IpcRequest, Response as IpcResponse,
    ResponsePayload as IpcResponsePayload,
};
use bmux_perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing};
use bmux_plugin_sdk::{
    CORE_CLI_BRIDGE_PROTOCOL_V1, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    CoreCliCommandRequest, CoreCliCommandResponse, HostConnectionInfo, HostKernelBridge,
    HostKernelBridgeRequest, HostKernelBridgeResponse, HostMetadata, HostScope, LogWriteLevel,
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    PROCESS_RUNTIME_ENV_PERSISTENT_WORKER, PROCESS_RUNTIME_ENV_PLUGIN_ID,
    PROCESS_RUNTIME_ENV_PROTOCOL, PROCESS_RUNTIME_PROTOCOL_V1, PROCESS_RUNTIME_TRANSPORT_STDIO_V1,
    PluginCliCommandRequest, PluginCliCommandResponse, PluginError, PluginEvent,
    ProcessInvocationRequest, ProcessInvocationResponse, RegisteredService, Result,
    ServiceEnvelopeKind, ServiceKind, ServiceRequest, ServiceResponse, StaticPluginVtable,
    decode_process_invocation_response, decode_service_envelope, decode_service_message,
    encode_host_kernel_bridge_cli_command_payload,
    encode_host_kernel_bridge_plugin_command_payload, encode_process_invocation_request,
    encode_service_envelope, encode_service_message,
};
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{io::ErrorKind, os::unix::process::ExitStatusExt};
#[cfg(windows)]
use std::{io::ErrorKind, os::windows::process::ExitStatusExt};
use tracing::{debug, error, info, trace, warn};

type PluginEntryFn = unsafe extern "C" fn() -> *const c_char;
type NativeRunCommandFn = unsafe extern "C" fn(*const c_char, usize, *const *const c_char) -> i32;
type NativeRunCommandWithContextFn = unsafe extern "C" fn(*const u8, usize) -> i32;
type NativeLifecycleFn = unsafe extern "C" fn(*const u8, usize) -> i32;
type NativeEventFn = unsafe extern "C" fn(*const u8, usize) -> i32;
type NativeInvokeServiceFn =
    unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32;

const NATIVE_SERVICE_STATUS_OK: i32 = 0;
const NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const KERNEL_STATUS_OK: i32 = 0;
const KERNEL_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const PROCESS_PLUGIN_TIMEOUT_ENV_VAR: &str = "BMUX_PROCESS_PLUGIN_TIMEOUT_MS";
const PROCESS_PLUGIN_TIMEOUT_DEFAULT_MS: u64 = 30_000;

static LOCAL_STATIC_SERVICE_PROVIDER_CACHE: OnceLock<Mutex<BTreeMap<String, Arc<LoadedPlugin>>>> =
    OnceLock::new();

/// Backend that a [`LoadedPlugin`] uses to dispatch calls.
#[derive(Debug)]
enum PluginBackend {
    /// Dynamically loaded shared library (third-party / filesystem plugins).
    Dynamic(Library),
    /// Statically linked into the binary (bundled plugins behind feature flags).
    Static(StaticPluginVtable),
    /// External process plugin runtime.
    Process(ProcessPluginRuntime),
}

#[derive(Debug, Clone)]
struct ProcessPluginRuntime {
    command: String,
    args: Vec<String>,
    current_dir: Option<PathBuf>,
    persistent_worker: bool,
    persistent: Arc<Mutex<Option<PersistentProcessWorker>>>,
    metrics: Arc<ProcessRuntimeMetrics>,
}

#[derive(Debug, Default)]
struct ProcessRuntimeMetrics {
    one_shot_timeouts: AtomicU64,
    persistent_retries: AtomicU64,
    persistent_respawns: AtomicU64,
    persistent_timeouts: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProcessRuntimeMetricsSnapshot {
    one_shot_timeouts: u64,
    persistent_retries: u64,
    persistent_respawns: u64,
    persistent_timeouts: u64,
}

impl ProcessRuntimeMetrics {
    fn snapshot(&self) -> ProcessRuntimeMetricsSnapshot {
        ProcessRuntimeMetricsSnapshot {
            one_shot_timeouts: self.one_shot_timeouts.load(Ordering::Relaxed),
            persistent_retries: self.persistent_retries.load(Ordering::Relaxed),
            persistent_respawns: self.persistent_respawns.load(Ordering::Relaxed),
            persistent_timeouts: self.persistent_timeouts.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
struct PersistentProcessWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_capture: Arc<Mutex<Vec<u8>>>,
}

impl PersistentProcessWorker {
    fn stderr_snapshot(&self) -> Vec<u8> {
        self.stderr_capture
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PersistentProcessWorker {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => self.terminate(),
        }
    }
}

impl ProcessPluginRuntime {
    fn invoke(
        &self,
        plugin_id: &str,
        argv: &[String],
        request: &ProcessInvocationRequest,
    ) -> Result<(Option<ProcessInvocationResponse>, std::process::ExitStatus)> {
        if self.persistent_worker {
            return self.invoke_persistent(plugin_id, request);
        }
        self.invoke_one_shot(plugin_id, argv, request)
    }

    // Keep one-shot protocol handling in a single flow so timeout and stderr behavior stay obvious.
    #[allow(clippy::too_many_lines)]
    fn invoke_one_shot(
        &self,
        plugin_id: &str,
        argv: &[String],
        request: &ProcessInvocationRequest,
    ) -> Result<(Option<ProcessInvocationResponse>, std::process::ExitStatus)> {
        let frame = encode_process_invocation_request(request)?;

        let mut command = Command::new(&self.command);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        command.args(&self.args);
        command.args(argv);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.env(
            PROCESS_RUNTIME_ENV_PROTOCOL,
            PROCESS_RUNTIME_TRANSPORT_STDIO_V1,
        );
        command.env(PROCESS_RUNTIME_ENV_PLUGIN_ID, plugin_id);

        let mut child = command
            .spawn()
            .map_err(|error| PluginError::ProcessPluginSpawn {
                plugin_id: plugin_id.to_string(),
                command: self.command.clone(),
                details: error.to_string(),
            })?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(&frame)
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed writing process runtime request frame: {error}"),
                })?;
            stdin
                .flush()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed flushing process runtime stdin: {error}"),
                })?;
        }
        drop(child.stdin.take());

        let stdout_reader = child.stdout.take().map(read_stream_to_end).ok_or_else(|| {
            PluginError::ServiceProtocol {
                details: "process runtime stdout pipe missing".to_string(),
            }
        })?;
        let stderr_reader = child.stderr.take().map(read_stream_to_end).ok_or_else(|| {
            PluginError::ServiceProtocol {
                details: "process runtime stderr pipe missing".to_string(),
            }
        })?;

        let timeout = process_plugin_timeout();
        let started = Instant::now();
        let status = loop {
            if let Some(status) =
                child
                    .try_wait()
                    .map_err(|error| PluginError::ServiceProtocol {
                        details: format!("failed polling process runtime child: {error}"),
                    })?
            {
                break status;
            }
            if started.elapsed() >= timeout {
                self.metrics
                    .one_shot_timeouts
                    .fetch_add(1, Ordering::Relaxed);
                let _ = child.kill();
                let _ = child.wait();
                let stderr = join_reader(stderr_reader, "stderr")?;
                warn!(
                    plugin_id,
                    command = self.command,
                    timeout_ms = timeout.as_millis(),
                    metrics = ?self.metrics.snapshot(),
                    "[bmux-runtime-fault-json]{{\"kind\":\"one-shot-timeout\"}} [bmux-runtime-fault:one-shot-timeout] process runtime one-shot invocation timed out"
                );
                return Err(PluginError::ProcessPluginTimeout {
                    plugin_id: plugin_id.to_string(),
                    command: self.command.clone(),
                    timeout_ms: timeout.as_millis(),
                    details: summarize_stderr(&stderr),
                });
            }
            thread::sleep(Duration::from_millis(10));
        };

        let stdout = join_reader(stdout_reader, "stdout")?;
        let stderr = join_reader(stderr_reader, "stderr")?;

        if stdout.is_empty() {
            if status.success() {
                return Ok((None, status));
            }
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "process runtime exited with status {} without framed stdout response{}",
                    status,
                    summarize_stderr_suffix(&stderr)
                ),
            });
        }

        let response = decode_process_invocation_response(&stdout).map_err(|error| {
            PluginError::ServiceProtocol {
                details: format!(
                    "failed decoding process runtime stdout frame: {error}{}",
                    summarize_stderr_suffix(&stderr)
                ),
            }
        })?;
        Ok((Some(response), status))
    }

    // Keep retry/respawn state machine inline so persistent-worker recovery paths stay auditable.
    #[allow(clippy::too_many_lines)]
    fn invoke_persistent(
        &self,
        plugin_id: &str,
        request: &ProcessInvocationRequest,
    ) -> Result<(Option<ProcessInvocationResponse>, std::process::ExitStatus)> {
        let frame = encode_process_invocation_request(request)?;
        let timeout = process_plugin_timeout();
        let mut recovered_once = false;

        let mut guard = self
            .persistent
            .lock()
            .map_err(|_| PluginError::ServiceProtocol {
                details: "persistent worker mutex poisoned".to_string(),
            })?;
        for attempt in 0..=1 {
            if guard.is_none() {
                *guard = Some(self.spawn_persistent_worker(plugin_id)?);
            }

            let worker = guard.as_mut().ok_or_else(|| PluginError::ServiceProtocol {
                details: "persistent process worker unavailable".to_string(),
            })?;

            if worker
                .child
                .try_wait()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed polling persistent process worker: {error}"),
                })?
                .is_some()
            {
                if attempt == 0 {
                    self.metrics
                        .persistent_respawns
                        .fetch_add(1, Ordering::Relaxed);
                    warn!(
                        plugin_id,
                        command = self.command,
                        metrics = ?self.metrics.snapshot(),
                        "[bmux-runtime-fault-json]{{\"kind\":\"persistent-respawn\"}} [bmux-runtime-fault:persistent-respawn] persistent process worker exited; respawning"
                    );
                    Self::reset_persistent_worker(&mut guard, false);
                    recovered_once = true;
                    continue;
                }

                return Err(PluginError::ServiceProtocol {
                    details: format!(
                        "persistent process worker exited before handling request{}",
                        summarize_stderr_suffix(&worker.stderr_snapshot())
                    ),
                });
            }

            if let Err(error) = worker
                .stdin
                .write_all(&frame)
                .and_then(|()| worker.stdin.flush())
            {
                if attempt == 0 {
                    self.metrics
                        .persistent_retries
                        .fetch_add(1, Ordering::Relaxed);
                    warn!(
                        plugin_id,
                        command = self.command,
                        error = %error,
                        metrics = ?self.metrics.snapshot(),
                        "[bmux-runtime-fault-json]{{\"kind\":\"persistent-retry\"}} [bmux-runtime-fault:persistent-retry] persistent process worker write failed; recycling worker"
                    );
                    Self::reset_persistent_worker(&mut guard, true);
                    recovered_once = true;
                    continue;
                }

                return Err(PluginError::ServiceProtocol {
                    details: format!(
                        "failed writing persistent process request frame: {error}{}",
                        summarize_stderr_suffix(&worker.stderr_snapshot())
                    ),
                });
            }

            let response_bytes = match read_framed_payload_with_timeout(&mut worker.stdout, timeout)
            {
                Ok(frame_bytes) => frame_bytes,
                Err(error) => {
                    if attempt == 0 {
                        self.metrics
                            .persistent_retries
                            .fetch_add(1, Ordering::Relaxed);
                        if error.kind() == ErrorKind::TimedOut {
                            self.metrics
                                .persistent_timeouts
                                .fetch_add(1, Ordering::Relaxed);
                            warn!(
                                plugin_id,
                                command = self.command,
                                error = %error,
                                metrics = ?self.metrics.snapshot(),
                                "[bmux-runtime-fault-json]{{\"kind\":\"persistent-timeout\"}} [bmux-runtime-fault:persistent-timeout] persistent process worker read timed out; recycling worker"
                            );
                        } else {
                            warn!(
                                plugin_id,
                                command = self.command,
                                error = %error,
                                metrics = ?self.metrics.snapshot(),
                                "[bmux-runtime-fault-json]{{\"kind\":\"persistent-retry\"}} [bmux-runtime-fault:persistent-retry] persistent process worker read failed; recycling worker"
                            );
                        }
                        Self::reset_persistent_worker(&mut guard, true);
                        recovered_once = true;
                        continue;
                    }
                    return Err(PluginError::ServiceProtocol {
                        details: format!(
                            "failed reading persistent process response frame: {error}{}",
                            summarize_stderr_suffix(&worker.stderr_snapshot())
                        ),
                    });
                }
            };

            let response =
                decode_process_invocation_response(&response_bytes).map_err(|error| {
                    PluginError::ServiceProtocol {
                        details: format!(
                            "failed decoding persistent process response frame: {error}{}",
                            summarize_stderr_suffix(&worker.stderr_snapshot())
                        ),
                    }
                })?;

            let status = worker
                .child
                .try_wait()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed polling persistent process worker: {error}"),
                })?
                .unwrap_or_else(success_exit_status);
            if recovered_once {
                debug!(
                    plugin_id,
                    command = self.command,
                    metrics = ?self.metrics.snapshot(),
                    "persistent process worker request recovered after retry/respawn"
                );
            }
            drop(guard);
            return Ok((Some(response), status));
        }

        Err(PluginError::ServiceProtocol {
            details: "persistent process worker unavailable".to_string(),
        })
    }

    fn reset_persistent_worker(guard: &mut Option<PersistentProcessWorker>, terminate: bool) {
        if let Some(mut stale_worker) = guard.take()
            && terminate
        {
            stale_worker.terminate();
        }
    }

    fn spawn_persistent_worker(&self, plugin_id: &str) -> Result<PersistentProcessWorker> {
        let mut command = Command::new(&self.command);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        command.args(&self.args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.env(
            PROCESS_RUNTIME_ENV_PROTOCOL,
            PROCESS_RUNTIME_TRANSPORT_STDIO_V1,
        );
        command.env(PROCESS_RUNTIME_ENV_PLUGIN_ID, plugin_id);
        command.env(PROCESS_RUNTIME_ENV_PERSISTENT_WORKER, "1");

        let mut child = command
            .spawn()
            .map_err(|error| PluginError::ProcessPluginSpawn {
                plugin_id: plugin_id.to_string(),
                command: self.command.clone(),
                details: error.to_string(),
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "persistent process stdin pipe missing".to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "persistent process stdout pipe missing".to_string(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "persistent process stderr pipe missing".to_string(),
            })?;

        let stderr_capture = Arc::new(Mutex::new(Vec::new()));
        let stderr_capture_clone = Arc::clone(&stderr_capture);
        let _stderr_thread = thread::spawn(move || {
            let mut reader = stderr;
            let mut buffer = Vec::new();
            if reader.read_to_end(&mut buffer).is_ok()
                && let Ok(mut captured) = stderr_capture_clone.lock()
            {
                *captured = buffer;
            }
        });

        Ok(PersistentProcessWorker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_capture,
        })
    }
}

fn process_plugin_timeout() -> Duration {
    let timeout_ms = std::env::var(PROCESS_PLUGIN_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(PROCESS_PLUGIN_TIMEOUT_DEFAULT_MS);
    Duration::from_millis(timeout_ms)
}

fn read_framed_payload_with_timeout(
    reader: &mut BufReader<ChildStdout>,
    timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let started = Instant::now();
    let mut header = [0_u8; 12];
    let mut read = 0_usize;
    while read < header.len() {
        match reader.read(&mut header[read..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }
            Ok(n) => read += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => (),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err(std::io::Error::new(ErrorKind::TimedOut, "read timeout"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }

    if &header[..8] != bmux_plugin_sdk::PROCESS_RUNTIME_MAGIC_V1 {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "invalid process frame magic",
        ));
    }
    let payload_len = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
    let mut payload = vec![0_u8; payload_len];
    reader.read_exact(&mut payload)?;

    let mut frame = header.to_vec();
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[cfg(unix)]
fn success_exit_status() -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn success_exit_status() -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(0)
}

fn read_stream_to_end<R: Read + Send + 'static>(
    mut reader: R,
) -> thread::JoinHandle<Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| PluginError::ServiceProtocol {
                details: format!("failed reading process runtime stream: {error}"),
            })?;
        Ok(bytes)
    })
}

fn join_reader(handle: thread::JoinHandle<Result<Vec<u8>>>, stream: &str) -> Result<Vec<u8>> {
    handle.join().map_err(|_| PluginError::ServiceProtocol {
        details: format!("process runtime {stream} reader thread panicked"),
    })?
}

fn summarize_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.is_empty() {
        "no stderr output".to_string()
    } else {
        text
    }
}

fn summarize_stderr_suffix(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    if text.is_empty() {
        String::new()
    } else {
        format!("; stderr: {text}")
    }
}

thread_local! {
    static COMMAND_OUTCOME_CAPTURE: RefCell<Option<bmux_plugin_sdk::PluginCommandOutcome>> = const { RefCell::new(None) };
}

fn begin_command_outcome_capture() {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        *slot.borrow_mut() = Some(bmux_plugin_sdk::PluginCommandOutcome::default());
    });
}

fn finish_command_outcome_capture() -> bmux_plugin_sdk::PluginCommandOutcome {
    COMMAND_OUTCOME_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CorePluginSettingsRequest {
    plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CorePluginSettingsResponse {
    #[serde(
        serialize_with = "serialize_toml_option",
        deserialize_with = "deserialize_toml_option"
    )]
    settings: Option<toml::Value>,
}

#[allow(clippy::ref_option)]
fn serialize_toml_option<S: serde::Serializer>(
    value: &Option<toml::Value>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    let text: Option<String> = value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(serde::ser::Error::custom)?;
    text.serialize(serializer)
}

fn deserialize_toml_option<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<toml::Value>, D::Error> {
    let text: Option<String> = Option::deserialize(deserializer)?;
    text.map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn local_static_service_provider_cache() -> &'static Mutex<BTreeMap<String, Arc<LoadedPlugin>>> {
    LOCAL_STATIC_SERVICE_PROVIDER_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn load_static_service_provider_cached(
    provider_plugin_id: &str,
    host: &HostMetadata,
    available_capability_map: &BTreeMap<HostScope, CapabilityProvider>,
) -> Result<Option<Arc<LoadedPlugin>>> {
    let Some(vtable) = crate::static_vtable(provider_plugin_id) else {
        return Ok(None);
    };

    if let Ok(cache) = local_static_service_provider_cache().lock()
        && let Some(loaded) = cache.get(provider_plugin_id)
    {
        return Ok(Some(Arc::clone(loaded)));
    }

    let manifest_ptr = (vtable.entry)();
    if manifest_ptr.is_null() {
        return Err(PluginError::NullPluginEntry {
            plugin_id: provider_plugin_id.to_string(),
            symbol: "static_vtable::entry".to_string(),
        });
    }
    let manifest_cstr = unsafe { std::ffi::CStr::from_ptr(manifest_ptr) };
    let manifest_text = manifest_cstr
        .to_str()
        .map_err(|_| PluginError::InvalidPluginEntry {
            plugin_id: provider_plugin_id.to_string(),
            symbol: "static_vtable::entry".to_string(),
            details: "embedded manifest is not valid UTF-8".to_string(),
        })?;
    let embedded_manifest = crate::PluginManifest::from_toml_str(manifest_text)?;
    let declaration = embedded_manifest.to_declaration()?;
    let synthetic = RegisteredPlugin {
        search_root: PathBuf::new(),
        manifest_path: PathBuf::new(),
        manifest: embedded_manifest,
        declaration,
        bundled_static: true,
    };
    let loaded = Arc::new(load_static_plugin(
        &synthetic,
        vtable,
        host,
        available_capability_map,
    )?);

    if let Ok(mut cache) = local_static_service_provider_cache().lock() {
        let entry = cache
            .entry(provider_plugin_id.to_string())
            .or_insert_with(|| Arc::clone(&loaded));
        return Ok(Some(Arc::clone(entry)));
    }

    Ok(Some(loaded))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageGetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageGetResponse {
    value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageSetRequest {
    key: String,
    value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PluginStateKey {
    data_dir: String,
    plugin_id: String,
    key: String,
}

static STORAGE_CACHE: OnceLock<Mutex<BTreeMap<PluginStateKey, Option<Vec<u8>>>>> = OnceLock::new();
static VOLATILE_STATE: OnceLock<Mutex<BTreeMap<PluginStateKey, Vec<u8>>>> = OnceLock::new();

fn storage_cache() -> &'static Mutex<BTreeMap<PluginStateKey, Option<Vec<u8>>>> {
    STORAGE_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn volatile_state() -> &'static Mutex<BTreeMap<PluginStateKey, Vec<u8>>> {
    VOLATILE_STATE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn plugin_state_key(connection: &HostConnectionInfo, plugin_id: &str, key: &str) -> PluginStateKey {
    PluginStateKey {
        data_dir: connection.data_dir.clone(),
        plugin_id: plugin_id.to_string(),
        key: key.to_string(),
    }
}

impl ServiceCaller for NativeCommandContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            self.caller_client_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        execute_kernel_request(self.host_kernel_bridge, request)
    }
}

impl ServiceCaller for NativeLifecycleContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            None,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        execute_kernel_request(self.host_kernel_bridge, request)
    }
}

impl ServiceCaller for NativeServiceContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            self.caller_client_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        execute_kernel_request(self.host_kernel_bridge, request)
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
/// Low-level byte-encoded service dispatch used by every
/// [`ServiceCaller`] impl on the context types
/// ([`NativeCommandContext`], [`NativeLifecycleContext`],
/// [`NativeServiceContext`]).
///
/// This is exposed publicly so typed service providers can construct
/// a standalone [`ServiceCaller`] that lives as long as the typed
/// handle (rather than piggy-backing on a short-lived context). The
/// args mirror the fields a context holds and would normally pass
/// through to this helper.
///
/// # Errors
///
/// Returns any error produced by capability validation, service lookup,
/// or the underlying provider plugin.
pub fn call_service_raw(
    caller_plugin_id: &str,
    caller_client_id: Option<uuid::Uuid>,
    required_capabilities: &[String],
    provided_capabilities: &[String],
    services: &[RegisteredService],
    available_capabilities: &[String],
    enabled_plugins: &[String],
    plugin_search_roots: &[String],
    host: &HostMetadata,
    connection: &HostConnectionInfo,
    host_kernel_bridge: Option<HostKernelBridge>,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
    capability: &str,
    kind: ServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    // Test-only hook: when a `TestServiceRouter` is active on the
    // current thread (installed via `install_test_service_router`),
    // route all service calls through it before any capability
    // checks or plugin-discovery logic. This lets plugin unit tests
    // construct a `NativeServiceContext` without setting up a real
    // plugin loader. Production paths never install one, so the
    // overhead is a single `thread_local!` check.
    if let Some(router) = test_service_router() {
        return router(
            caller_plugin_id,
            caller_client_id,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        );
    }

    let capability = HostScope::new(capability)?;
    let allowed = required_capabilities
        .iter()
        .chain(provided_capabilities.iter())
        .filter_map(|value| HostScope::new(value).ok())
        .any(|entry| entry == capability);
    if !allowed {
        return Err(PluginError::CapabilityAccessDenied {
            plugin_id: caller_plugin_id.to_string(),
            capability: capability.as_str().to_string(),
            operation: "call_service",
            hint: Some(
                bmux_plugin_sdk::CapabilityAccessDeniedHint::declare_required_capability(
                    capability.as_str(),
                ),
            ),
        });
    }

    let service = services
        .iter()
        .find(|service| {
            service.capability == capability
                && service.kind == kind
                && service.interface_id == interface_id
        })
        .cloned()
        .ok_or(PluginError::UnsupportedHostOperation {
            operation: "call_service",
        })?;

    if matches!(service.provider, bmux_plugin_sdk::ProviderId::Host) {
        return handle_core_service_call(
            caller_plugin_id,
            connection,
            &service,
            operation,
            &payload,
            host_kernel_bridge,
            plugin_settings_map,
        );
    }

    let provider_plugin_id = match &service.provider {
        bmux_plugin_sdk::ProviderId::Plugin(plugin_id) => plugin_id.clone(),
        bmux_plugin_sdk::ProviderId::Host => {
            unreachable!("host services should be handled earlier")
        }
    };

    // Consult the process-level `ServiceLocationMap` to decide whether
    // this process owns the provider (`Local` → continue to in-process
    // dispatch below) or must forward the call over IPC (`Remote` →
    // wrap in `Request::InvokeService` and ship through the host
    // kernel bridge). Providers with no recorded location are treated
    // as local for backward compatibility with tests and pre-bootstrap
    // paths; real runtime code paths mark every known plugin before
    // any typed service call fires.
    if matches!(
        crate::global_service_locations().get(&provider_plugin_id),
        Some(crate::ServiceLocation::Remote)
    ) {
        return dispatch_remote_typed_service(
            host_kernel_bridge,
            &provider_plugin_id,
            capability.as_str(),
            kind,
            interface_id,
            operation,
            payload,
        );
    }

    let available_capability_map = available_capabilities
        .iter()
        .filter_map(|value| HostScope::new(value).ok())
        .map(|capability| {
            let provider = CapabilityProvider {
                capability: capability.clone(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            };
            (capability, provider)
        })
        .collect::<BTreeMap<_, _>>();

    // Bundled static providers are already registered in-process. Keep
    // them off the filesystem discovery path entirely and reuse the
    // loaded dispatch wrapper across local plugin-to-plugin calls.
    let loaded = if let Some(loaded) =
        load_static_service_provider_cached(&provider_plugin_id, host, &available_capability_map)?
    {
        loaded
    } else {
        let search_roots = plugin_search_roots
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let registry = discover_registered_plugins_in_roots(&search_roots)?;
        let registered = registry.get(&provider_plugin_id).ok_or_else(|| {
            PluginError::MissingServiceProvider {
                provider_plugin_id: provider_plugin_id.clone(),
                capability: service.capability.as_str().to_string(),
                interface_id: service.interface_id.clone(),
            }
        })?;
        Arc::new(load_registered_plugin(
            registered,
            host,
            &available_capability_map,
        )?)
    };
    let response = loaded.invoke_service(&NativeServiceContext {
        plugin_id: provider_plugin_id,
        request: ServiceRequest {
            caller_plugin_id: caller_plugin_id.to_string(),
            service: service.clone(),
            operation: operation.to_string(),
            payload,
        },
        required_capabilities: loaded
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: loaded
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: services.to_vec(),
        available_capabilities: available_capabilities.to_vec(),
        enabled_plugins: enabled_plugins.to_vec(),
        plugin_search_roots: plugin_search_roots.to_vec(),
        host: host.clone(),
        connection: connection.clone(),
        settings: plugin_settings_map
            .get(loaded.declaration.id.as_str())
            .cloned(),
        plugin_settings_map: plugin_settings_map.clone(),
        caller_client_id,
        host_kernel_bridge,
    })?;

    if let Some(error) = response.error {
        return Err(PluginError::ServiceInvocationFailed {
            provider_plugin_id: service.provider.to_string(),
            capability: service.capability.as_str().to_string(),
            interface_id: service.interface_id,
            operation: operation.to_string(),
            code: error.code,
            message: error.message,
        });
    }

    Ok(response.payload)
}

#[allow(clippy::too_many_lines)]
fn handle_core_service_call(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    service: &RegisteredService,
    operation: &str,
    payload: &[u8],
    host_kernel_bridge: Option<HostKernelBridge>,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
) -> Result<Vec<u8>> {
    match (service.interface_id.as_str(), operation) {
        ("config-query/v1", "plugin_settings") => {
            let request: CorePluginSettingsRequest = decode_service_message(payload)?;
            let settings = plugin_settings_map.get(&request.plugin_id).cloned();
            encode_service_message(&CorePluginSettingsResponse { settings })
        }
        ("storage-query/v1", "get") => {
            let total_started = Instant::now();
            let decode_started = Instant::now();
            let request: CoreStorageGetRequest = decode_service_message(payload)?;
            let decode_us = decode_started.elapsed().as_micros();
            let validate_started = Instant::now();
            validate_storage_key(&request.key)?;
            let validate_us = validate_started.elapsed().as_micros();
            let cache_key = plugin_state_key(connection, caller_plugin_id, &request.key);
            let cache_started = Instant::now();
            let cached = storage_cache()
                .lock()
                .ok()
                .and_then(|cache| cache.get(&cache_key).cloned());
            let cache_us = cache_started.elapsed().as_micros();
            let mut fs_us = 0_u128;
            let cache_hit = cached.is_some();
            let value = if let Some(value) = cached {
                value
            } else {
                let fs_started = Instant::now();
                let path = storage_file_path(connection, caller_plugin_id, &request.key);
                let value = match fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(PluginError::ServiceProtocol {
                            details: format!("failed reading storage value: {error}"),
                        });
                    }
                };
                fs_us = fs_started.elapsed().as_micros();
                if let Ok(mut cache) = storage_cache().lock() {
                    cache.insert(cache_key, value.clone());
                }
                value
            };
            let encode_started = Instant::now();
            let response = encode_service_message(&CoreStorageGetResponse {
                value: value.clone(),
            });
            let encode_us = encode_started.elapsed().as_micros();
            emit_phase_timing(
                PhaseChannel::Storage,
                &PhasePayload::new("storage.get")
                    .field("plugin_id", caller_plugin_id)
                    .field("key", request.key.as_str())
                    .field("cache_hit", cache_hit)
                    .field("value_len", value.as_ref().map_or(0, Vec::len))
                    .field("decode_us", decode_us)
                    .field("validate_us", validate_us)
                    .field("cache_us", cache_us)
                    .field("fs_us", fs_us)
                    .field("encode_us", encode_us)
                    .field("total_us", total_started.elapsed().as_micros())
                    .finish(),
            );
            response
        }
        ("storage-command/v1", "set") => {
            let total_started = Instant::now();
            let decode_started = Instant::now();
            let request: CoreStorageSetRequest = decode_service_message(payload)?;
            let decode_us = decode_started.elapsed().as_micros();
            let validate_started = Instant::now();
            validate_storage_key(&request.key)?;
            let validate_us = validate_started.elapsed().as_micros();
            let fs_started = Instant::now();
            let path = storage_file_path(connection, caller_plugin_id, &request.key);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed creating storage directory: {error}"),
                })?;
            }
            fs::write(path, &request.value).map_err(|error| PluginError::ServiceProtocol {
                details: format!("failed writing storage value: {error}"),
            })?;
            let fs_us = fs_started.elapsed().as_micros();
            let cache_started = Instant::now();
            if let Ok(mut cache) = storage_cache().lock() {
                cache.insert(
                    plugin_state_key(connection, caller_plugin_id, &request.key),
                    Some(request.value.clone()),
                );
            }
            let cache_us = cache_started.elapsed().as_micros();
            let encode_started = Instant::now();
            let response = encode_service_message(&());
            let encode_us = encode_started.elapsed().as_micros();
            emit_phase_timing(
                PhaseChannel::Storage,
                &PhasePayload::new("storage.set")
                    .field("plugin_id", caller_plugin_id)
                    .field("key", request.key.as_str())
                    .field("value_len", request.value.len())
                    .field("decode_us", decode_us)
                    .field("validate_us", validate_us)
                    .field("fs_us", fs_us)
                    .field("cache_us", cache_us)
                    .field("encode_us", encode_us)
                    .field("total_us", total_started.elapsed().as_micros())
                    .finish(),
            );
            response
        }
        ("volatile-state-query/v1", "get") => {
            let total_started = Instant::now();
            let request: bmux_plugin_sdk::VolatileStateGetRequest =
                decode_service_message(payload)?;
            validate_storage_key(&request.key)?;
            let map_started = Instant::now();
            let value = volatile_state().lock().ok().and_then(|map| {
                map.get(&plugin_state_key(
                    connection,
                    caller_plugin_id,
                    &request.key,
                ))
                .cloned()
            });
            let map_us = map_started.elapsed().as_micros();
            let response = encode_service_message(&bmux_plugin_sdk::VolatileStateGetResponse {
                value: value.clone(),
            });
            emit_phase_timing(
                PhaseChannel::Storage,
                &PhasePayload::new("volatile_state.get")
                    .field("plugin_id", caller_plugin_id)
                    .field("key", request.key.as_str())
                    .field("value_len", value.as_ref().map_or(0, Vec::len))
                    .field("map_us", map_us)
                    .field("total_us", total_started.elapsed().as_micros())
                    .finish(),
            );
            response
        }
        ("volatile-state-command/v1", "set") => {
            let total_started = Instant::now();
            let request: bmux_plugin_sdk::VolatileStateSetRequest =
                decode_service_message(payload)?;
            validate_storage_key(&request.key)?;
            let map_started = Instant::now();
            if let Ok(mut map) = volatile_state().lock() {
                map.insert(
                    plugin_state_key(connection, caller_plugin_id, &request.key),
                    request.value.clone(),
                );
            }
            let map_us = map_started.elapsed().as_micros();
            let response = encode_service_message(&());
            emit_phase_timing(
                PhaseChannel::Storage,
                &PhasePayload::new("volatile_state.set")
                    .field("plugin_id", caller_plugin_id)
                    .field("key", request.key.as_str())
                    .field("value_len", request.value.len())
                    .field("map_us", map_us)
                    .field("total_us", total_started.elapsed().as_micros())
                    .finish(),
            );
            response
        }
        ("volatile-state-command/v1", "clear") => {
            let total_started = Instant::now();
            let request: bmux_plugin_sdk::VolatileStateClearRequest =
                decode_service_message(payload)?;
            validate_storage_key(&request.key)?;
            let map_started = Instant::now();
            if let Ok(mut map) = volatile_state().lock() {
                map.remove(&plugin_state_key(
                    connection,
                    caller_plugin_id,
                    &request.key,
                ));
            }
            let map_us = map_started.elapsed().as_micros();
            let response = encode_service_message(&());
            emit_phase_timing(
                PhaseChannel::Storage,
                &PhasePayload::new("volatile_state.clear")
                    .field("plugin_id", caller_plugin_id)
                    .field("key", request.key.as_str())
                    .field("map_us", map_us)
                    .field("total_us", total_started.elapsed().as_micros())
                    .finish(),
            );
            response
        }
        ("logging-command/v1", "write") => {
            let request: bmux_plugin_sdk::LogWriteRequest = decode_service_message(payload)?;
            emit_plugin_log(caller_plugin_id, &request)?;
            encode_service_message(&())
        }
        (CORE_CLI_COMMAND_INTERFACE_V1, CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1) => {
            let request: CoreCliCommandRequest = decode_service_message(payload)?;
            let response = execute_cli_command_request(host_kernel_bridge, &request)?;
            encode_service_message(&response)
        }
        (CORE_CLI_COMMAND_INTERFACE_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1) => {
            let request: PluginCliCommandRequest = decode_service_message(payload)?;
            let response = execute_plugin_command_request(host_kernel_bridge, &request)?;
            encode_service_message(&response)
        }
        _ => Err(PluginError::UnsupportedHostOperation {
            operation: "call_service",
        }),
    }
}

fn emit_plugin_log(
    caller_plugin_id: &str,
    request: &bmux_plugin_sdk::LogWriteRequest,
) -> Result<()> {
    let requested_target = request
        .target
        .as_deref()
        .filter(|entry| !entry.trim().is_empty())
        .unwrap_or(caller_plugin_id);
    let message = request.message.trim();
    if message.is_empty() {
        return Err(PluginError::ServiceProtocol {
            details: "log message cannot be empty".to_string(),
        });
    }

    match request.level {
        LogWriteLevel::Error => {
            error!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Warn => {
            warn!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Info => {
            info!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Debug => {
            debug!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Trace => {
            trace!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
    }

    Ok(())
}

/// Forward a typed plugin service call to whichever process owns the
/// activated provider by wrapping it in [`IpcRequest::InvokeService`]
/// and shipping it through the host kernel bridge.
///
/// The server's `Request::InvokeService` handler dispatches through
/// its activated `service_registry`, so callers get the same handler
/// behavior they would see if the provider were local in this
/// process.
fn dispatch_remote_typed_service(
    host_kernel_bridge: Option<HostKernelBridge>,
    provider_plugin_id: &str,
    capability: &str,
    kind: ServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let invoke_kind = match kind {
        ServiceKind::Query => InvokeServiceKind::Query,
        ServiceKind::Command => InvokeServiceKind::Command,
        ServiceKind::Event => {
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "cannot forward Event typed call to remote provider '{provider_plugin_id}' — events are delivered via the event bus, not synchronous InvokeService"
                ),
            });
        }
    };
    let request = IpcRequest::InvokeService {
        capability: capability.to_string(),
        kind: invoke_kind,
        interface_id: interface_id.to_string(),
        operation: operation.to_string(),
        payload,
    };
    let response_payload = execute_kernel_request(host_kernel_bridge, request)?;
    match response_payload {
        IpcResponsePayload::ServiceInvoked { payload } => Ok(payload),
        other => Err(PluginError::ServiceProtocol {
            details: format!(
                "unexpected kernel response for remote service call to '{provider_plugin_id}': {other:?}"
            ),
        }),
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn execute_kernel_request(
    host_kernel_bridge: Option<HostKernelBridge>,
    request: IpcRequest,
) -> Result<IpcResponsePayload> {
    let bridge = host_kernel_bridge.ok_or(PluginError::UnsupportedHostOperation {
        operation: "call_host_kernel",
    })?;
    let encoded_request =
        bmux_ipc::encode(&request).map_err(|error| PluginError::ServiceProtocol {
            details: format!("failed encoding kernel request: {error}"),
        })?;
    let encoded_response = invoke_host_kernel_bridge(bridge, encoded_request)?;
    let response: IpcResponse =
        bmux_ipc::decode(&encoded_response).map_err(|error| PluginError::ServiceProtocol {
            details: format!("failed decoding kernel response: {error}"),
        })?;
    match response {
        IpcResponse::Ok(payload) => Ok(payload),
        IpcResponse::Err(error) => Err(PluginError::ServiceProtocol {
            details: format!("kernel request failed: {}", error.message),
        }),
    }
}

fn execute_cli_command_request(
    host_kernel_bridge: Option<HostKernelBridge>,
    request: &CoreCliCommandRequest,
) -> Result<CoreCliCommandResponse> {
    if request.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported core CLI bridge request protocol version: {}",
                request.protocol_version
            ),
        });
    }
    let bridge = host_kernel_bridge.ok_or(PluginError::UnsupportedHostOperation {
        operation: "call_service",
    })?;
    let payload = encode_host_kernel_bridge_cli_command_payload(request)?;
    let encoded_response = invoke_host_kernel_bridge(bridge, payload)?;
    let response: CoreCliCommandResponse = decode_service_message(&encoded_response)?;
    if response.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported core CLI bridge response protocol version: {}",
                response.protocol_version
            ),
        });
    }
    Ok(response)
}

fn execute_plugin_command_request(
    host_kernel_bridge: Option<HostKernelBridge>,
    request: &PluginCliCommandRequest,
) -> Result<PluginCliCommandResponse> {
    if request.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported plugin CLI bridge request protocol version: {}",
                request.protocol_version
            ),
        });
    }
    let bridge = host_kernel_bridge.ok_or(PluginError::UnsupportedHostOperation {
        operation: "call_service",
    })?;
    let payload = encode_host_kernel_bridge_plugin_command_payload(request)?;
    let encoded_response = invoke_host_kernel_bridge(bridge, payload)?;
    let response: PluginCliCommandResponse = decode_service_message(&encoded_response)?;
    if response.protocol_version != CORE_CLI_BRIDGE_PROTOCOL_V1 {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported plugin CLI bridge response protocol version: {}",
                response.protocol_version
            ),
        });
    }
    if let Some(error) = response.error.clone() {
        return Err(PluginError::ServiceProtocol { details: error });
    }
    Ok(response)
}

fn invoke_host_kernel_bridge(bridge: HostKernelBridge, payload: Vec<u8>) -> Result<Vec<u8>> {
    let request = encode_service_message(&HostKernelBridgeRequest { payload })?;
    let mut output = vec![0u8; request.len().saturating_mul(4).max(1024)];
    let mut output_len = 0usize;

    let status = bridge.invoke(
        request.as_ptr(),
        request.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut output_len,
    );

    if status == KERNEL_STATUS_BUFFER_TOO_SMALL {
        output.resize(output_len, 0);
        let status = bridge.invoke(
            request.as_ptr(),
            request.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
        );
        if status != KERNEL_STATUS_OK {
            return Err(PluginError::ServiceProtocol {
                details: format!("kernel bridge invocation failed with status {status}"),
            });
        }
    } else if status != KERNEL_STATUS_OK {
        return Err(PluginError::ServiceProtocol {
            details: format!("kernel bridge invocation failed with status {status}"),
        });
    }

    output.truncate(output_len);
    let response: HostKernelBridgeResponse = decode_service_message(&output)?;
    Ok(response.payload)
}

fn storage_file_path(connection: &HostConnectionInfo, plugin_id: &str, key: &str) -> PathBuf {
    PathBuf::from(&connection.data_dir)
        .join("plugin-storage")
        .join(plugin_id)
        .join(format!("{key}.bin"))
}

fn validate_storage_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(PluginError::ServiceProtocol {
            details: "storage key must be non-empty and use [A-Za-z0-9._-]".to_string(),
        });
    }
    Ok(())
}

pub struct LoadedPlugin {
    pub registered: RegisteredPlugin,
    pub declaration: PluginDeclaration,
    backend: PluginBackend,
}

impl LoadedPlugin {
    /// Collect the typed service registrations exposed by this plugin.
    ///
    /// For statically-linked bundled plugins that share the process
    /// address space, this invokes the SDK's `register_typed_services`
    /// hook via the static vtable and returns the populated registry.
    ///
    /// `context` is forwarded to the plugin's hook so providers that
    /// need host access from inside their trait methods can capture
    /// the bridge and capability metadata at registration time.
    ///
    /// Dynamic (`dlopen`) and out-of-process backends cannot currently
    /// register in-process typed providers; this method returns an
    /// empty registry for those backends.
    #[must_use]
    pub fn collect_typed_services(
        &self,
        context: bmux_plugin_sdk::TypedServiceRegistrationContext<'_>,
    ) -> bmux_plugin_sdk::TypedServiceRegistry {
        match &self.backend {
            PluginBackend::Static(vtable) => (vtable.register_typed_services)(context),
            PluginBackend::Dynamic(_) | PluginBackend::Process(_) => {
                bmux_plugin_sdk::TypedServiceRegistry::new()
            }
        }
    }
    fn ensure_process_protocol_version(operation: &str, protocol_version: u16) -> Result<()> {
        if protocol_version == PROCESS_RUNTIME_PROTOCOL_V1 {
            return Ok(());
        }
        Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported process runtime {operation} response protocol version: {protocol_version}"
            ),
        })
    }

    fn unexpected_process_response(
        operation: &str,
        response: &ProcessInvocationResponse,
    ) -> PluginError {
        PluginError::ServiceProtocol {
            details: format!(
                "unexpected process runtime response for {operation} invocation: {response:?}"
            ),
        }
    }

    fn process_error_to_result(
        operation: &str,
        protocol_version: u16,
        details: String,
    ) -> Result<()> {
        Self::ensure_process_protocol_version(operation, protocol_version)?;
        Err(PluginError::ServiceProtocol { details })
    }

    fn invoke_process(
        &self,
        runtime: &ProcessPluginRuntime,
        argv: &[String],
        request: &ProcessInvocationRequest,
    ) -> Result<(Option<ProcessInvocationResponse>, std::process::ExitStatus)> {
        runtime.invoke(self.declaration.id.as_str(), argv, request)
    }

    #[must_use]
    pub fn commands(&self) -> &[bmux_plugin_sdk::PluginCommand] {
        &self.declaration.commands
    }

    #[must_use]
    pub fn supports_command(&self, command_name: &str) -> bool {
        self.declaration
            .commands
            .iter()
            .any(|command| command.name == command_name)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    ///
    /// **Note:** Static plugins always require a command context.  Calling
    /// this method (which passes `context: None`) on a static plugin will
    /// return [`PluginError::NativeCommandSymbol`].  Use
    /// [`run_command_with_context`](Self::run_command_with_context) instead.
    pub fn run_command(&self, command_name: &str, arguments: &[String]) -> Result<i32> {
        self.run_command_with_context(command_name, arguments, None)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    pub fn run_command_with_context(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<i32> {
        let (status, _) =
            self.run_command_with_context_and_outcome(command_name, arguments, context)?;
        Ok(status)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    ///
    pub fn run_command_with_context_and_outcome(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<(i32, bmux_plugin_sdk::PluginCommandOutcome)> {
        if !self.supports_command(command_name) {
            return Err(PluginError::UnknownPluginCommand {
                plugin_id: self.declaration.id.as_str().to_string(),
                command: command_name.to_string(),
            });
        }

        if let Some(context) = context {
            let payload = encode_service_message(context).map_err(|_| {
                PluginError::InvalidNativeCommandInput {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    field: "context",
                }
            })?;

            match &self.backend {
                PluginBackend::Static(vtable) => {
                    begin_command_outcome_capture();
                    let _ = bmux_plugin_sdk::take_last_command_error();
                    let status = (vtable.run_command_with_context)(payload.as_ptr(), payload.len());
                    let mut outcome = finish_command_outcome_capture();
                    if let Some(error) = bmux_plugin_sdk::take_last_command_error() {
                        outcome.error_message = Some(error.message);
                    }
                    return Ok((status, outcome));
                }
                PluginBackend::Dynamic(library) => {
                    if let Ok(command_symbol) = unsafe {
                        library.get::<NativeRunCommandWithContextFn>(
                            DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL.as_bytes(),
                        )
                    } {
                        begin_command_outcome_capture();
                        let _ = bmux_plugin_sdk::take_last_command_error();
                        let status = unsafe { command_symbol(payload.as_ptr(), payload.len()) };
                        let mut outcome = finish_command_outcome_capture();
                        if let Some(error) = bmux_plugin_sdk::take_last_command_error() {
                            outcome.error_message = Some(error.message);
                        }
                        return Ok((status, outcome));
                    }
                }
                PluginBackend::Process(_) => {
                    return self.run_process_command(command_name, arguments, Some(context));
                }
            }
        }

        if matches!(self.backend, PluginBackend::Process(_)) {
            return self.run_process_command(command_name, arguments, None);
        }

        // Fallback: use the legacy run_command symbol (dynamic only)
        let PluginBackend::Dynamic(library) = &self.backend else {
            return Err(PluginError::NativeCommandSymbol {
                plugin_id: self.declaration.id.as_str().to_string(),
                symbol: DEFAULT_NATIVE_COMMAND_SYMBOL.to_string(),
                details: "static plugins require context-based command dispatch".to_string(),
            });
        };

        let command_name =
            CString::new(command_name).map_err(|_| PluginError::InvalidNativeCommandInput {
                plugin_id: self.declaration.id.as_str().to_string(),
                field: "command_name",
            })?;
        let argument_values = arguments
            .iter()
            .map(|argument| {
                CString::new(argument.as_str()).map_err(|_| {
                    PluginError::InvalidNativeCommandInput {
                        plugin_id: self.declaration.id.as_str().to_string(),
                        field: "arguments",
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let argument_ptrs = argument_values
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();

        let command_symbol: Symbol<'_, NativeRunCommandFn> = unsafe {
            library.get(DEFAULT_NATIVE_COMMAND_SYMBOL.as_bytes())
        }
        .map_err(|error| PluginError::NativeCommandSymbol {
            plugin_id: self.declaration.id.as_str().to_string(),
            symbol: DEFAULT_NATIVE_COMMAND_SYMBOL.to_string(),
            details: error.to_string(),
        })?;

        let status = unsafe {
            command_symbol(
                command_name.as_ptr(),
                argument_ptrs.len(),
                argument_ptrs.as_ptr(),
            )
        };

        Ok((status, bmux_plugin_sdk::PluginCommandOutcome::default()))
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn activate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_ACTIVATE_SYMBOL, context)
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn deactivate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_DEACTIVATE_SYMBOL, context)
    }

    #[must_use]
    pub fn receives_event(&self, event: &PluginEvent) -> bool {
        self.declaration.event_subscriptions.is_empty()
            || self
                .declaration
                .event_subscriptions
                .iter()
                .any(|subscription| subscription.matches(event))
    }

    /// # Errors
    ///
    /// Returns an error when the event symbol cannot be loaded or the event
    /// payload cannot be encoded.
    ///
    pub fn dispatch_event(&self, event: &PluginEvent) -> Result<Option<i32>> {
        if !self.receives_event(event) {
            return Ok(None);
        }

        let payload =
            encode_service_message(event).map_err(|_| PluginError::InvalidNativeEventInput {
                plugin_id: self.declaration.id.as_str().to_string(),
            })?;

        let status = match &self.backend {
            PluginBackend::Static(vtable) => (vtable.handle_event)(payload.as_ptr(), payload.len()),
            PluginBackend::Dynamic(library) => {
                let event_symbol: Symbol<'_, NativeEventFn> =
                    unsafe { library.get(DEFAULT_NATIVE_EVENT_SYMBOL.as_bytes()) }.map_err(
                        |error| PluginError::NativeEventSymbol {
                            plugin_id: self.declaration.id.as_str().to_string(),
                            symbol: DEFAULT_NATIVE_EVENT_SYMBOL.to_string(),
                            details: error.to_string(),
                        },
                    )?;
                unsafe { event_symbol(payload.as_ptr(), payload.len()) }
            }
            PluginBackend::Process(runtime) => {
                let request = ProcessInvocationRequest::Event {
                    protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
                    plugin_id: self.declaration.id.as_str().to_string(),
                    event: event.clone(),
                };
                let empty_argv: Vec<String> = Vec::new();
                let (response, _) = self.invoke_process(runtime, &empty_argv, &request)?;
                match response {
                    None => return Ok(None),
                    Some(ProcessInvocationResponse::Event {
                        protocol_version,
                        status,
                    }) => {
                        Self::ensure_process_protocol_version("event", protocol_version)?;
                        return Ok(status);
                    }
                    Some(ProcessInvocationResponse::Error {
                        protocol_version,
                        details,
                        status,
                    }) => {
                        Self::ensure_process_protocol_version("error", protocol_version)?;
                        if let Some(status) = status {
                            return Ok(Some(status));
                        }
                        return Err(PluginError::ServiceProtocol { details });
                    }
                    Some(other) => {
                        return Err(Self::unexpected_process_response("event", &other));
                    }
                }
            }
        };

        Ok(Some(status))
    }

    /// # Errors
    ///
    /// Returns an error when the service symbol cannot be loaded, the service
    /// payload cannot be encoded, or the plugin returns invalid transport data.
    ///
    /// # Panics
    ///
    /// Panics if the resolved dynamic library symbol is unexpectedly `None`
    /// for a `Dynamic` backend (should not happen in practice).
    pub fn invoke_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        if let PluginBackend::Process(runtime) = &self.backend {
            return self.invoke_process_service(runtime, context);
        }

        self.invoke_native_service(context)
    }

    fn invoke_process_service(
        &self,
        runtime: &ProcessPluginRuntime,
        context: &NativeServiceContext,
    ) -> Result<ServiceResponse> {
        let request = ProcessInvocationRequest::Service {
            protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
            plugin_id: self.declaration.id.as_str().to_string(),
            context: context.clone(),
        };
        let empty_argv: Vec<String> = Vec::new();
        let (response, _) = self.invoke_process(runtime, &empty_argv, &request)?;
        match response {
            Some(ProcessInvocationResponse::Service {
                protocol_version,
                response,
            }) => {
                Self::ensure_process_protocol_version("service", protocol_version)?;
                Ok(response)
            }
            Some(ProcessInvocationResponse::Error {
                protocol_version,
                details,
                status: _,
            }) => {
                Self::process_error_to_result("service", protocol_version, details)?;
                unreachable!("process_error_to_result always returns Err")
            }
            None => Err(PluginError::UnsupportedPluginRuntime {
                plugin_id: self.declaration.id.as_str().to_string(),
                runtime: "process-services".to_string(),
            }),
            Some(other) => Err(Self::unexpected_process_response("service", &other)),
        }
    }

    fn invoke_native_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        let total_started = Instant::now();
        let backend = match &self.backend {
            PluginBackend::Static(_) => "static",
            PluginBackend::Dynamic(_) => "dynamic",
            PluginBackend::Process(_) => "process",
        };
        let encode_started = Instant::now();
        let payload = encode_service_envelope(0, ServiceEnvelopeKind::Request, context)?;
        let encode_us = encode_started.elapsed().as_micros();

        let resolved_symbol = match &self.backend {
            PluginBackend::Dynamic(library) => {
                let sym: Symbol<'_, NativeInvokeServiceFn> =
                    unsafe { library.get(DEFAULT_NATIVE_SERVICE_SYMBOL.as_bytes()) }.map_err(
                        |error| PluginError::NativeServiceSymbol {
                            plugin_id: self.declaration.id.as_str().to_string(),
                            symbol: DEFAULT_NATIVE_SERVICE_SYMBOL.to_string(),
                            details: error.to_string(),
                        },
                    )?;
                Some(sym)
            }
            PluginBackend::Static(_) | PluginBackend::Process(_) => None,
        };

        let call_service = |payload: &[u8], output: &mut [u8], output_len: &mut usize| -> i32 {
            match &self.backend {
                PluginBackend::Static(vtable) => (vtable.invoke_service)(
                    payload.as_ptr(),
                    payload.len(),
                    output.as_mut_ptr(),
                    output.len(),
                    output_len,
                ),
                PluginBackend::Dynamic(_) => {
                    let service_fn = resolved_symbol
                        .as_ref()
                        .expect("resolved_symbol is Some for Dynamic backend");
                    unsafe {
                        service_fn(
                            payload.as_ptr(),
                            payload.len(),
                            output.as_mut_ptr(),
                            output.len(),
                            output_len,
                        )
                    }
                }
                PluginBackend::Process(_) => NATIVE_SERVICE_STATUS_OK,
            }
        };

        let mut output = vec![0_u8; 4096];
        let mut output_len = 0_usize;
        let call_started = Instant::now();
        let mut status = call_service(&payload, &mut output, &mut output_len);
        if status == NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL {
            output.resize(output_len.max(output.len() * 2), 0);
            status = call_service(&payload, &mut output, &mut output_len);
        }
        let call_us = call_started.elapsed().as_micros();

        if status != NATIVE_SERVICE_STATUS_OK {
            return Err(PluginError::NativeServiceInvocation {
                plugin_id: self.declaration.id.as_str().to_string(),
                status,
            });
        }

        if output_len > output.len() {
            return Err(PluginError::InvalidNativeServiceOutput {
                plugin_id: self.declaration.id.as_str().to_string(),
                details: format!(
                    "service returned {output_len} bytes into {} byte buffer",
                    output.len(),
                ),
            });
        }
        output.truncate(output_len);

        let decode_started = Instant::now();
        let (_, response) =
            decode_service_envelope::<ServiceResponse>(&output, ServiceEnvelopeKind::Response)?;
        let decode_us = decode_started.elapsed().as_micros();
        emit_phase_timing(
            PhaseChannel::Service,
            &PhasePayload::new("plugin.native_service_invoke")
                .field("plugin_id", self.declaration.id.as_str())
                .field("backend", backend)
                .service_fields(
                    context.request.service.capability.as_str(),
                    format!("{:?}", context.request.service.kind),
                    context.request.service.interface_id.as_str(),
                    context.request.operation.as_str(),
                )
                .field("request_payload_len", context.request.payload.len())
                .field("encoded_request_len", payload.len())
                .field("encoded_response_len", output_len)
                .field("encode_us", encode_us)
                .field("call_us", call_us)
                .field("decode_us", decode_us)
                .field("total_us", total_started.elapsed().as_micros())
                .finish(),
        );
        Ok(response)
    }

    fn run_lifecycle_symbol(&self, symbol: &str, context: &NativeLifecycleContext) -> Result<i32> {
        let payload = encode_service_message(context).map_err(|_| {
            PluginError::InvalidNativeLifecycleInput {
                plugin_id: self.declaration.id.as_str().to_string(),
            }
        })?;

        let status = match &self.backend {
            PluginBackend::Static(vtable) => {
                let func = if symbol == DEFAULT_NATIVE_ACTIVATE_SYMBOL {
                    vtable.activate
                } else if symbol == DEFAULT_NATIVE_DEACTIVATE_SYMBOL {
                    vtable.deactivate
                } else {
                    return Err(PluginError::NativeLifecycleSymbol {
                        plugin_id: self.declaration.id.as_str().to_string(),
                        symbol: symbol.to_string(),
                        details: "unknown lifecycle symbol for static plugin".to_string(),
                    });
                };
                func(payload.as_ptr(), payload.len())
            }
            PluginBackend::Dynamic(library) => {
                let lifecycle_symbol: Symbol<'_, NativeLifecycleFn> = unsafe {
                    library.get(symbol.as_bytes())
                }
                .map_err(|error| PluginError::NativeLifecycleSymbol {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: symbol.to_string(),
                    details: error.to_string(),
                })?;
                unsafe { lifecycle_symbol(payload.as_ptr(), payload.len()) }
            }
            PluginBackend::Process(runtime) => {
                let request = ProcessInvocationRequest::Lifecycle {
                    protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: symbol.to_string(),
                    context: context.clone(),
                };
                let empty_argv: Vec<String> = Vec::new();
                let (response, status) = self.invoke_process(runtime, &empty_argv, &request)?;
                match response {
                    Some(ProcessInvocationResponse::Lifecycle {
                        protocol_version,
                        status,
                    }) => {
                        Self::ensure_process_protocol_version("lifecycle", protocol_version)?;
                        status
                    }
                    Some(ProcessInvocationResponse::Error {
                        protocol_version,
                        details,
                        status,
                    }) => {
                        Self::ensure_process_protocol_version("error", protocol_version)?;
                        if let Some(status) = status {
                            status
                        } else {
                            return Err(PluginError::ServiceProtocol { details });
                        }
                    }
                    None => status.code().unwrap_or(0),
                    Some(other) => {
                        return Err(Self::unexpected_process_response("lifecycle", &other));
                    }
                }
            }
        };

        Ok(status)
    }

    fn run_process_command(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<(i32, bmux_plugin_sdk::PluginCommandOutcome)> {
        let PluginBackend::Process(runtime) = &self.backend else {
            return Err(PluginError::ServiceProtocol {
                details: "run_process_command called for non-process backend".to_string(),
            });
        };

        let request = ProcessInvocationRequest::Command {
            protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
            plugin_id: self.declaration.id.as_str().to_string(),
            command_name: command_name.to_string(),
            arguments: arguments.to_vec(),
            context: context.cloned(),
        };
        let argv = std::iter::once(command_name.to_string())
            .chain(arguments.iter().cloned())
            .collect::<Vec<_>>();
        let (response, status) = self.invoke_process(runtime, &argv, &request)?;

        if let Some(response) = response {
            return match response {
                ProcessInvocationResponse::Command {
                    protocol_version,
                    status,
                    outcome,
                } => {
                    Self::ensure_process_protocol_version("command", protocol_version)?;
                    Ok((status, outcome.unwrap_or_default()))
                }
                ProcessInvocationResponse::Error {
                    protocol_version,
                    details,
                    status: _,
                } => {
                    Self::process_error_to_result("command", protocol_version, details)?;
                    unreachable!("process_error_to_result always returns Err")
                }
                other => Err(Self::unexpected_process_response("command", &other)),
            };
        }

        Ok((
            status.code().unwrap_or(1),
            bmux_plugin_sdk::PluginCommandOutcome::default(),
        ))
    }
}

#[derive(Debug, Default)]
pub struct NativePluginLoader;

impl NativePluginLoader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible, missing, fails to load,
    /// or returns a descriptor that conflicts with its manifest.
    pub fn load_registered_plugin(
        &self,
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
    ) -> Result<LoadedPlugin> {
        PluginRegistry::validate_registered_plugin(
            registered_plugin,
            host,
            available_capabilities,
        )?;

        if let PluginEntrypoint::Process {
            command,
            args,
            persistent_worker,
        } = &registered_plugin.declaration.entrypoint
        {
            return Ok(LoadedPlugin {
                registered: registered_plugin.clone(),
                declaration: registered_plugin.declaration.clone(),
                backend: PluginBackend::Process(ProcessPluginRuntime {
                    command: command.clone(),
                    args: args.clone(),
                    current_dir: registered_plugin
                        .manifest_path
                        .parent()
                        .filter(|path| !path.as_os_str().is_empty())
                        .map(Path::to_path_buf),
                    persistent_worker: *persistent_worker,
                    persistent: Arc::new(Mutex::new(None)),
                    metrics: Arc::new(ProcessRuntimeMetrics::default()),
                }),
            });
        }

        let entry_path = registered_plugin
            .manifest
            .resolve_entry_path(
                registered_plugin
                    .manifest_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .ok_or_else(|| PluginError::MissingEntryPath {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            })?;
        let library = unsafe { Library::new(&entry_path) }.map_err(|error| {
            PluginError::NativeLibraryLoad {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                path: entry_path.clone(),
                details: error.to_string(),
            }
        })?;

        let declaration = load_native_declaration(&library, registered_plugin)?;
        PluginRegistry::validate_registered_plugin(
            &RegisteredPlugin {
                declaration: declaration.clone(),
                ..registered_plugin.clone()
            },
            host,
            available_capabilities,
        )?;
        compare_manifest_and_embedded(registered_plugin, &declaration)?;

        Ok(LoadedPlugin {
            registered: registered_plugin.clone(),
            declaration,
            backend: PluginBackend::Dynamic(library),
        })
    }
}

/// # Errors
///
/// Returns an error when the plugin cannot be loaded.
pub fn load_registered_plugin(
    registered_plugin: &RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    NativePluginLoader::new().load_registered_plugin(
        registered_plugin,
        host,
        available_capabilities,
    )
}

/// Load a statically-linked bundled plugin from its vtable.
///
/// This bypasses filesystem discovery and `dlopen` entirely.  The plugin's
/// manifest TOML is obtained by calling the vtable's `entry` function pointer
/// directly, and the resulting [`LoadedPlugin`] dispatches all subsequent calls
/// through the same vtable.
///
/// # Errors
///
/// Returns an error when the manifest cannot be parsed or validated.
pub fn load_static_plugin(
    registered_plugin: &RegisteredPlugin,
    vtable: StaticPluginVtable,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    let manifest_ptr = (vtable.entry)();
    if manifest_ptr.is_null() {
        return Err(PluginError::NullPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: "static_vtable::entry".to_string(),
        });
    }
    let manifest_cstr = unsafe { CStr::from_ptr(manifest_ptr) };
    let manifest_text = manifest_cstr
        .to_str()
        .map_err(|_| PluginError::InvalidPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: "static_vtable::entry".to_string(),
            details: "embedded manifest is not valid UTF-8".to_string(),
        })?;

    let embedded_manifest = crate::PluginManifest::from_toml_str(manifest_text)?;
    let declaration = embedded_manifest.to_declaration()?;

    // Validate against host capabilities (skip entry file existence check
    // since there is no file -- the plugin is compiled into the binary).
    let synthetic = RegisteredPlugin {
        declaration: declaration.clone(),
        ..registered_plugin.clone()
    };
    PluginRegistry::validate_static_plugin(&synthetic, host, available_capabilities)?;

    compare_manifest_and_embedded(registered_plugin, &declaration)?;

    Ok(LoadedPlugin {
        registered: registered_plugin.clone(),
        declaration,
        backend: PluginBackend::Static(vtable),
    })
}

/// Load a statically-linked plugin whose declaration was already parsed and
/// registered by the host from the same embedded manifest.
///
/// # Errors
///
/// Returns an error when the registered declaration is incompatible with the
/// current host or declared available capabilities.
pub fn load_trusted_static_plugin(
    registered_plugin: &RegisteredPlugin,
    vtable: StaticPluginVtable,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    PluginRegistry::validate_static_plugin(registered_plugin, host, available_capabilities)?;
    Ok(LoadedPlugin {
        registered: registered_plugin.clone(),
        declaration: registered_plugin.declaration.clone(),
        backend: PluginBackend::Static(vtable),
    })
}

fn load_native_declaration(
    library: &Library,
    registered_plugin: &RegisteredPlugin,
) -> Result<PluginDeclaration> {
    let symbol_name = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.as_bytes(),
        PluginEntrypoint::Process { .. } => {
            return Err(PluginError::UnsupportedPluginRuntime {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                runtime: "process".to_string(),
            });
        }
    };

    let descriptor_symbol: Symbol<'_, PluginEntryFn> = unsafe { library.get(symbol_name) }
        .map_err(|error| PluginError::NativeEntrySymbol {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: match &registered_plugin.declaration.entrypoint {
                PluginEntrypoint::Native { symbol } => symbol.clone(),
                PluginEntrypoint::Process { .. } => "process-entrypoint".to_string(),
            },
            details: error.to_string(),
        })?;

    let descriptor_ptr = unsafe { descriptor_symbol() };
    let symbol = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.clone(),
        PluginEntrypoint::Process { .. } => "process-entrypoint".to_string(),
    };
    if descriptor_ptr.is_null() {
        return Err(PluginError::NullPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol,
        });
    }

    let manifest_text = unsafe { CStr::from_ptr(descriptor_ptr) }
        .to_str()
        .map_err(|_| PluginError::InvalidPluginEntryUtf8 {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: symbol.clone(),
        })?;

    let embedded_manifest =
        crate::PluginManifest::from_toml_str(manifest_text).map_err(|error| {
            PluginError::InvalidPluginEntry {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                symbol: symbol.clone(),
                details: error.to_string(),
            }
        })?;

    embedded_manifest.to_declaration()
}

fn compare_manifest_and_embedded(
    registered_plugin: &RegisteredPlugin,
    declaration: &PluginDeclaration,
) -> Result<()> {
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "id",
        registered_plugin.declaration.id.as_str(),
        declaration.id.as_str(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "display_name",
        &registered_plugin.declaration.display_name,
        &declaration.display_name,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_version",
        &registered_plugin.declaration.plugin_version,
        &declaration.plugin_version,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_api",
        &registered_plugin.declaration.plugin_api.to_string(),
        &declaration.plugin_api.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "native_abi",
        &registered_plugin.declaration.native_abi.to_string(),
        &declaration.native_abi.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provider_priority",
        &registered_plugin.declaration.provider_priority.to_string(),
        &declaration.provider_priority.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "required_capabilities",
        &format!("{:?}", registered_plugin.declaration.required_capabilities),
        &format!("{:?}", declaration.required_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_capabilities",
        &format!("{:?}", registered_plugin.declaration.provided_capabilities),
        &format!("{:?}", declaration.provided_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_features",
        &format!("{:?}", registered_plugin.declaration.provided_features),
        &format!("{:?}", declaration.provided_features),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "services",
        &serde_json::to_string(&registered_plugin.declaration.services)
            .expect("plugin services should serialize"),
        &serde_json::to_string(&declaration.services).expect("plugin services should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "commands",
        &serde_json::to_string(&registered_plugin.declaration.commands)
            .expect("plugin commands should serialize"),
        &serde_json::to_string(&declaration.commands).expect("plugin commands should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "event_subscriptions",
        &serde_json::to_string(&registered_plugin.declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
        &serde_json::to_string(&declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "dependencies",
        &serde_json::to_string(&registered_plugin.declaration.dependencies)
            .expect("plugin dependencies should serialize"),
        &serde_json::to_string(&declaration.dependencies)
            .expect("plugin dependencies should serialize"),
    )?;

    Ok(())
}

fn ensure_match(
    plugin_id: &str,
    field: &'static str,
    manifest_value: &str,
    embedded_value: &str,
) -> Result<()> {
    if manifest_value == embedded_value {
        Ok(())
    } else {
        Err(PluginError::ManifestMismatch {
            plugin_id: plugin_id.to_string(),
            field,
            manifest_value: manifest_value.to_string(),
            embedded_value: embedded_value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadedPlugin, PluginBackend};
    use crate::{PluginEntrypoint, PluginManifest, PluginRegistry, ServiceCaller};
    use bmux_plugin_sdk::{
        ApiVersion, DEFAULT_NATIVE_ENTRY_SYMBOL, HostMetadata, NativeLifecycleContext,
        NativeServiceContext, PluginEvent, PluginEventKind, PluginEventSubscription,
        ServiceEnvelopeKind, ServiceResponse, decode_service_envelope, decode_service_message,
        encode_service_envelope, encode_service_message,
    };
    use libloading::Library;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::c_char;
    use std::process::{Command, Stdio};
    use std::ptr;
    use std::thread;
    use std::time::Duration;

    thread_local! {
        static KERNEL_REQUESTS: RefCell<Vec<bmux_ipc::Request>> = const { RefCell::new(Vec::new()) };
        static OMIT_CURRENT_CLIENT_FROM_LIST: Cell<bool> = const { Cell::new(false) };
    }

    const TEST_MANIFEST_TEXT: &str = concat!(
        "id = \"test.plugin\"\n",
        "name = \"Test Plugin\"\n",
        "version = \"0.1.0\"\n",
        "entry = \"unused.dylib\"\n",
        "required_capabilities = [\"bmux.commands\"]\n\n",
        "[[commands]]\n",
        "name = \"hello\"\n",
        "summary = \"hello\"\n",
        "execution = \"provider_exec\"\n",
        "\0"
    );

    #[cfg(unix)]
    const PERSISTENT_WORKER_REUSE_SCRIPT: &str = r"#!/bin/sh
printf 'BMUXPRC1\000\000\000\004\000\001\002\000'
sleep 0.05
printf 'BMUXPRC1\000\000\000\004\000\001\004\000'
sleep 60
";

    #[cfg(unix)]
    const PERSISTENT_WORKER_RESPAWN_SCRIPT: &str = r"#!/bin/sh
printf 'BMUXPRC1\000\000\000\004\000\001\002\000'
";

    #[cfg(unix)]
    const PERSISTENT_WORKER_DROP_SCRIPT_TEMPLATE: &str = r#"#!/bin/sh
printf '%s' "$$" > {pid_file:?}
printf 'BMUXPRC1\000\000\000\004\000\001\002\000'
sleep 60
"#;

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_entry_v1() -> *const c_char {
        TEST_MANIFEST_TEXT.as_ptr().cast()
    }

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_invoke_service_v1(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let (request_id, context) =
            decode_service_envelope::<NativeServiceContext>(input, ServiceEnvelopeKind::Request)
                .expect("service request should decode");
        let response = ServiceResponse::ok(context.request.payload);
        let encoded = encode_service_envelope(request_id, ServiceEnvelopeKind::Response, &response)
            .expect("service response should encode");
        unsafe {
            *output_len = encoded.len();
        }
        if output_ptr.is_null() || encoded.len() > output_capacity {
            return 4;
        }
        unsafe {
            ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
        }
        0
    }

    #[allow(clippy::too_many_lines)]
    unsafe extern "C" fn test_host_kernel_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let bridge_request: super::HostKernelBridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        if let Ok(Some(command_request)) =
            bmux_plugin_sdk::decode_host_kernel_bridge_cli_command_payload(&bridge_request.payload)
        {
            let exit_code = if command_request.command_path
                == vec!["playbook".to_string(), "run".to_string()]
            {
                0
            } else {
                11
            };
            let response = super::CoreCliCommandResponse::new(exit_code);
            let Ok(encoded) = encode_service_message(&super::HostKernelBridgeResponse {
                payload: encode_service_message(&response).expect("response should encode"),
            }) else {
                return 1;
            };
            unsafe {
                *output_len = encoded.len();
            }
            if output_ptr.is_null() || encoded.len() > output_capacity {
                return 4;
            }
            unsafe {
                ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            }
            return 0;
        }

        if let Ok(Some(command_request)) =
            bmux_plugin_sdk::decode_host_kernel_bridge_plugin_command_payload(
                &bridge_request.payload,
            )
        {
            let exit_code = if command_request.plugin_id == "bmux.windows"
                && command_request.command_name == "new-window"
            {
                0
            } else {
                12
            };
            let response = super::PluginCliCommandResponse::new(exit_code);
            let Ok(encoded) = encode_service_message(&super::HostKernelBridgeResponse {
                payload: encode_service_message(&response).expect("response should encode"),
            }) else {
                return 1;
            };
            unsafe {
                *output_len = encoded.len();
            }
            if output_ptr.is_null() || encoded.len() > output_capacity {
                return 4;
            }
            unsafe {
                ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
            }
            return 0;
        }

        let kernel_request: bmux_ipc::Request = match bmux_ipc::decode(&bridge_request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        KERNEL_REQUESTS.with(|log| log.borrow_mut().push(kernel_request.clone()));

        let kernel_response = bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: "unsupported kernel request in test bridge".to_string(),
        });
        let Ok(encoded_kernel_response) = bmux_ipc::encode(&kernel_response) else {
            return 1;
        };
        let Ok(output_message) = encode_service_message(&super::HostKernelBridgeResponse {
            payload: encoded_kernel_response,
        }) else {
            return 1;
        };

        let required_len = output_message.len();
        if required_len > output_capacity {
            unsafe {
                *output_len = required_len;
            }
            return 4;
        }

        unsafe {
            ptr::copy_nonoverlapping(output_message.as_ptr(), output_ptr, required_len);
            *output_len = required_len;
        }
        0
    }

    #[test]
    fn loaded_plugin_reports_declared_commands() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: PluginManifest::from_toml_str(TEST_MANIFEST_TEXT.trim_end_matches('\0'))
                .expect("manifest should parse")
                .to_declaration()
                .expect("declaration should build"),
            backend: PluginBackend::Dynamic(library),
        };

        assert_eq!(loaded.commands().len(), 1);
        assert!(loaded.supports_command("hello"));
        assert!(loaded.run_command("missing", &[]).is_err());
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: u32) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn load_persistent_worker_plugin(
        plugin_id: &str,
        plugin_name: &str,
        script_path: &std::path::Path,
        entry_command: &str,
    ) -> LoadedPlugin {
        let manifest = PluginManifest::from_toml_str(&format!(
            r#"
id = "{}"
name = "{}"
version = "0.1.0"
runtime = "process"
entry = "{}"
entry_args = ["{}"]
process_persistent_worker = true

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
            plugin_id,
            plugin_name,
            entry_command,
            script_path.display()
        ))
        .expect("manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");
        let registered = registry
            .get(plugin_id)
            .expect("plugin should register")
            .clone();

        super::load_registered_plugin(
            &registered,
            &HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            &BTreeMap::new(),
        )
        .expect("process runtime worker plugin should load")
    }

    fn process_runtime_metrics_snapshot(
        loaded: &LoadedPlugin,
    ) -> super::ProcessRuntimeMetricsSnapshot {
        match &loaded.backend {
            PluginBackend::Process(runtime) => runtime.metrics.snapshot(),
            PluginBackend::Dynamic(_) | PluginBackend::Static(_) => {
                panic!("expected process backend")
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_runtime_plugin_loads_and_runs_command() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "process.plugin"
name = "Process Plugin"
version = "0.1.0"
runtime = "process"
entry = "true"

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");
        let registered = registry
            .get("process.plugin")
            .expect("plugin should register")
            .clone();

        let loaded = super::load_registered_plugin(
            &registered,
            &HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            &BTreeMap::new(),
        )
        .expect("process runtime plugin should load");

        let args: Vec<String> = Vec::new();
        let status = loaded
            .run_command("hello", &args)
            .expect("process runtime command should run");
        assert_eq!(status, 0);
    }

    #[cfg(unix)]
    #[test]
    fn process_runtime_persistent_worker_reuses_single_process() {
        let temp_root = std::env::temp_dir().join(format!(
            "bmux-persistent-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_root).expect("temp test root should be created");
        let script_path = temp_root.join("worker.sh");
        std::fs::write(&script_path, PERSISTENT_WORKER_REUSE_SCRIPT)
            .expect("worker script should be written");

        let loaded = load_persistent_worker_plugin(
            "process.worker.plugin",
            "Process Worker Plugin",
            &script_path,
            "sh",
        );

        let args: Vec<String> = Vec::new();
        let first = loaded
            .run_command("hello", &args)
            .expect("first worker command should run");
        let second = loaded
            .run_command("hello", &args)
            .expect("second worker command should run");

        assert_eq!(first, 1);
        assert_eq!(second, 2);

        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[cfg(unix)]
    #[test]
    fn process_runtime_persistent_worker_respawns_after_worker_exit() {
        let temp_root = std::env::temp_dir().join(format!(
            "bmux-persistent-worker-respawn-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_root).expect("temp test root should be created");
        let script_path = temp_root.join("worker_once.sh");
        std::fs::write(&script_path, PERSISTENT_WORKER_RESPAWN_SCRIPT)
            .expect("worker script should be written");

        let loaded = load_persistent_worker_plugin(
            "process.worker.respawn.plugin",
            "Process Worker Respawn Plugin",
            &script_path,
            "sh",
        );

        let args: Vec<String> = Vec::new();
        let first = loaded
            .run_command("hello", &args)
            .expect("first worker command should run");
        let second = loaded
            .run_command("hello", &args)
            .expect("second worker command should run after respawn");

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        let metrics = process_runtime_metrics_snapshot(&loaded);
        assert!(metrics.persistent_respawns + metrics.persistent_retries >= 1);

        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[cfg(unix)]
    #[test]
    fn process_runtime_persistent_worker_drop_terminates_child_process() {
        let temp_root = std::env::temp_dir().join(format!(
            "bmux-persistent-worker-drop-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_root).expect("temp test root should be created");
        let script_path = temp_root.join("worker_hang.sh");
        let pid_path = temp_root.join("worker.pid");
        let script_contents = PERSISTENT_WORKER_DROP_SCRIPT_TEMPLATE.replace(
            "{pid_file:?}",
            &format!("{:?}", pid_path.display().to_string()),
        );
        std::fs::write(&script_path, script_contents).expect("worker script should be written");

        let loaded = load_persistent_worker_plugin(
            "process.worker.drop.plugin",
            "Process Worker Drop Plugin",
            &script_path,
            "sh",
        );

        let args: Vec<String> = Vec::new();
        let first = loaded
            .run_command("hello", &args)
            .expect("first worker command should run");
        assert_eq!(first, 1);

        let mut worker_pid: Option<u32> = None;
        for _ in 0..100 {
            if let Ok(pid_text) = std::fs::read_to_string(&pid_path)
                && let Ok(parsed) = pid_text.trim().parse::<u32>()
            {
                worker_pid = Some(parsed);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let worker_pid = worker_pid.expect("worker pid should be written");
        assert!(unix_process_exists(worker_pid));

        drop(loaded);

        let mut exited_after_drop = false;
        for _ in 0..200 {
            if !unix_process_exists(worker_pid) {
                exited_after_drop = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            exited_after_drop,
            "persistent worker should terminate when plugin runtime drops"
        );

        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn lifecycle_context_serializes_settings_and_host() {
        let context = NativeLifecycleContext {
            plugin_id: "test.plugin".to_string(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: Some(toml::Value::String("enabled".to_string())),
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        let json = serde_json::to_string(&context).expect("context should serialize");
        assert!(json.contains("test.plugin"));
        assert!(json.contains("bmux"));
        assert!(json.contains("enabled"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_capability() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.permissions.read")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Plugin("bmux.permissions".to_string()),
            }],
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                bmux_plugin_sdk::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing capability should fail");
        assert!(error.to_string().contains("bmux.permissions.read"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_registration() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.permissions.read".to_string()],
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                bmux_plugin_sdk::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("call_service"));
    }

    #[test]
    fn command_context_calls_core_config_service() {
        let mut plugin_settings_map = BTreeMap::new();
        plugin_settings_map.insert(
            "caller.plugin".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "greeting".to_string(),
                toml::Value::String("hello".to_string()),
            )])),
        );
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.config.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.config.read")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface_id: "config-query/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.config.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map,
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        };

        let response = context
            .call_service_raw(
                "bmux.config.read",
                bmux_plugin_sdk::ServiceKind::Query,
                "config-query/v1",
                "plugin_settings",
                encode_service_message(&super::CorePluginSettingsRequest {
                    plugin_id: "caller.plugin".to_string(),
                })
                .expect("request should encode"),
            )
            .expect("core config service should succeed");
        let response: super::CorePluginSettingsResponse =
            decode_service_message(&response).expect("response should decode");
        assert_eq!(
            response.settings,
            Some(toml::Value::Table(toml::map::Map::from_iter([(
                "greeting".to_string(),
                toml::Value::String("hello".to_string()),
            )])))
        );
    }

    #[test]
    fn command_context_calls_core_storage_service() {
        let storage_root =
            std::env::temp_dir().join(format!("bmux-plugin-storage-test-{}", uuid::Uuid::new_v4()));
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.storage".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![
                bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bmux.storage")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "storage-command/v1".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Host,
                },
                bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bmux.storage")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "storage-query/v1".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Host,
                },
            ],
            available_capabilities: vec!["bmux.storage".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: storage_root.to_string_lossy().to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        };

        context
            .call_service_raw(
                "bmux.storage",
                bmux_plugin_sdk::ServiceKind::Command,
                "storage-command/v1",
                "set",
                encode_service_message(&super::CoreStorageSetRequest {
                    key: "theme".to_string(),
                    value: b"sunset".to_vec(),
                })
                .expect("set request should encode"),
            )
            .expect("core storage set should succeed");

        let bytes = context
            .call_service_raw(
                "bmux.storage",
                bmux_plugin_sdk::ServiceKind::Query,
                "storage-query/v1",
                "get",
                encode_service_message(&super::CoreStorageGetRequest {
                    key: "theme".to_string(),
                })
                .expect("get request should encode"),
            )
            .expect("core storage get should succeed");
        let response: super::CoreStorageGetResponse =
            decode_service_message(&bytes).expect("get response should decode");
        assert_eq!(response.value, Some(b"sunset".to_vec()));

        let _ = std::fs::remove_dir_all(storage_root);
    }

    #[test]
    fn command_context_calls_core_logging_service() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "log".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.logs.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.logs.write")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "logging-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.logs.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        };

        let response: () = context
            .call_service(
                "bmux.logs.write",
                bmux_plugin_sdk::ServiceKind::Command,
                "logging-command/v1",
                "write",
                &bmux_plugin_sdk::LogWriteRequest {
                    level: bmux_plugin_sdk::LogWriteLevel::Info,
                    message: "hello from plugin".to_string(),
                    target: Some("plugin.test".to_string()),
                },
            )
            .expect("core logging service should succeed");
        assert_eq!(response, ());
    }

    #[test]
    fn command_context_calls_core_cli_command_service_via_kernel_bridge() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "proxy-cli".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.commands".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.commands")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "cli-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.commands".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: bmux_plugin_sdk::CoreCliCommandResponse = context
            .call_service(
                bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                bmux_plugin_sdk::ServiceKind::Command,
                bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
                &bmux_plugin_sdk::CoreCliCommandRequest::new(
                    vec!["playbook".to_string(), "run".to_string()],
                    Vec::new(),
                ),
            )
            .expect("core cli command service should succeed");
        assert_eq!(response.exit_code, 0);
    }

    #[test]
    fn command_context_rejects_unsupported_core_cli_request_version() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "proxy-cli".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.commands".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.commands")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "cli-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.commands".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let mut request = bmux_plugin_sdk::CoreCliCommandRequest::new(
            vec!["playbook".to_string(), "run".to_string()],
            Vec::new(),
        );
        request.protocol_version = bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1 + 1;

        let error = context
            .call_service::<_, bmux_plugin_sdk::CoreCliCommandResponse>(
                bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                bmux_plugin_sdk::ServiceKind::Command,
                bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
                &request,
            )
            .expect_err("unsupported protocol version should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported core CLI bridge request protocol version")
        );
    }

    #[test]
    fn command_context_calls_plugin_command_service_via_kernel_bridge() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "proxy-plugin".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.commands".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.commands")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "cli-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.commands".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: bmux_plugin_sdk::PluginCliCommandResponse = context
            .call_service(
                bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                bmux_plugin_sdk::ServiceKind::Command,
                bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
                &bmux_plugin_sdk::PluginCliCommandRequest::new(
                    "bmux.windows".to_string(),
                    "new-window".to_string(),
                    Vec::new(),
                ),
            )
            .expect("plugin command service should succeed");
        assert_eq!(response.exit_code, 0);
    }

    #[test]
    fn command_context_rejects_unsupported_plugin_cli_request_version() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "proxy-plugin".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.commands".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![bmux_plugin_sdk::RegisteredService {
                capability: bmux_plugin_sdk::HostScope::new("bmux.commands")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "cli-command/v1".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.commands".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let mut request = bmux_plugin_sdk::PluginCliCommandRequest::new(
            "bmux.windows".to_string(),
            "new-window".to_string(),
            Vec::new(),
        );
        request.protocol_version = bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1 + 1;

        let error = context
            .call_service::<_, bmux_plugin_sdk::PluginCliCommandResponse>(
                bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                bmux_plugin_sdk::ServiceKind::Command,
                bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
                &request,
            )
            .expect_err("unsupported protocol version should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported plugin CLI bridge request protocol version")
        );
    }

    #[test]
    fn native_service_context_roundtrips_through_service_envelope() {
        let context = NativeServiceContext {
            plugin_id: "bmux.permissions".to_string(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "example.native".to_string(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("bmux.permissions.read")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "permission-query/v1".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin("bmux.permissions".to_string()),
                },
                operation: "list".to_string(),
                payload: vec![1, 2, 3],
            },
            required_capabilities: vec!["bmux.permissions.read".to_string()],
            provided_capabilities: vec!["bmux.permissions.read".to_string()],
            services: Vec::new(),
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: Some(toml::toml! { mode = "service" }.into()),
            plugin_settings_map: BTreeMap::from([(
                "example.native".to_string(),
                toml::toml! { mode = "service" }.into(),
            )]),
            caller_client_id: None,
            host_kernel_bridge: None,
        };

        let bytes = encode_service_envelope(7, ServiceEnvelopeKind::Request, &context)
            .expect("context should encode");
        let (request_id, decoded): (u64, NativeServiceContext) =
            decode_service_envelope(&bytes, ServiceEnvelopeKind::Request)
                .expect("context should decode");
        assert_eq!(request_id, 7);
        assert_eq!(decoded, context);
    }

    #[test]
    fn loaded_plugin_filters_events_by_subscription() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"

[[event_subscriptions]]
kinds = ["bmux.core/server-started"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: crate::PluginDeclaration {
                id: crate::PluginId::new("test.plugin").expect("plugin id should parse"),
                display_name: "Test Plugin".to_string(),
                plugin_version: "0.1.0".to_string(),
                plugin_api: bmux_plugin_sdk::VersionRange::at_least(ApiVersion::new(1, 0)),
                native_abi: bmux_plugin_sdk::VersionRange::at_least(ApiVersion::new(1, 0)),
                entrypoint: PluginEntrypoint::Native {
                    symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
                },
                description: None,
                homepage: None,
                provider_priority: 0,
                execution_class: crate::PluginExecutionClass::NativeStandard,
                owns_namespaces: BTreeSet::new(),
                owns_paths: BTreeSet::new(),
                required_capabilities: BTreeSet::new(),
                provided_capabilities: BTreeSet::new(),
                provided_features: BTreeSet::new(),
                services: Vec::new(),
                commands: Vec::new(),
                event_subscriptions: vec![PluginEventSubscription::for_kind(
                    PluginEventKind::from_static("bmux.core/server-started"),
                )],
                event_publications: Vec::new(),
                dependencies: Vec::new(),
                lifecycle: crate::PluginLifecycle::default(),
                ready_signals: Vec::new(),
            },
            backend: PluginBackend::Dynamic(library),
        };

        assert!(loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::from_static("bmux.core/server-started"),
            payload: serde_json::Value::Null,
        }));
        assert!(!loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::from_static("bmux.core/server-stopping"),
            payload: serde_json::Value::Null,
        }));
    }

    #[test]
    fn production_loader_code_does_not_hardcode_domain_service_interfaces() {
        let source = include_str!("loader.rs")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or_default();
        assert!(!source.contains("permission-query/v1"));
        assert!(!source.contains("permission-command/v1"));
        // Historic interface ids from before the typed-dispatch migration.
        assert!(!source.contains("window-query/v1"));
        assert!(!source.contains("window-command/v1"));
        // Current typed interface ids also must not be hardcoded in the
        // loader; they belong to plugin manifests and BPDL-generated
        // bindings.
        assert!(!source.contains("\"windows-state\""));
        assert!(!source.contains("\"windows-commands\""));
        assert!(!source.contains("\"windows-events\""));
    }

    // ── Remote typed-service dispatch tests ──────────────────────────
    //
    // These tests exercise `dispatch_remote_typed_service` in isolation:
    // they install a fake `HostKernelBridge` function pointer, verify
    // that a typed call is encoded as `Request::InvokeService` with the
    // supplied capability / kind / interface / operation / payload, and
    // that a `ResponsePayload::ServiceInvoked` reply is decoded back to
    // the caller. The goal is to lock in the contract the rest of the
    // runtime (attach / CLI) depends on: a `ServiceLocation::Remote`
    // provider must round-trip through `Request::InvokeService` over
    // the host kernel bridge.

    use bmux_ipc::{
        ErrorCode, ErrorResponse, InvokeServiceKind as TestInvokeKind, Request as TestIpcRequest,
        Response as TestIpcResponse, ResponsePayload as TestIpcResponsePayload,
    };
    use bmux_plugin_sdk::{
        HostKernelBridge as TestHostKernelBridge, HostKernelBridgeRequest as TestBridgeRequest,
        HostKernelBridgeResponse as TestBridgeResponse, ServiceKind as TestServiceKind,
        decode_service_message as test_decode_service_message,
        encode_service_message as test_encode_service_message,
    };

    thread_local! {
        static LAST_REMOTE_REQUEST: RefCell<Option<TestIpcRequest>> = const { RefCell::new(None) };
        static NEXT_REMOTE_RESPONSE: RefCell<Option<TestIpcResponse>> = const { RefCell::new(None) };
    }

    /// Reset thread-local bridge fixtures so tests don't leak state
    /// into each other.
    fn reset_remote_bridge_slots() {
        LAST_REMOTE_REQUEST.with(|slot| slot.borrow_mut().take());
        NEXT_REMOTE_RESPONSE.with(|slot| slot.borrow_mut().take());
    }

    /// Test bridge: decodes the inbound `HostKernelBridgeRequest` into
    /// a `Request`, stashes it for assertions, then encodes the
    /// configured `Response` back into a `HostKernelBridgeResponse`.
    ///
    /// # Safety
    ///
    /// Called only via `HostKernelBridge::from_fn` in these tests. The
    /// runtime FFI contract matches the real `host_kernel_bridge`
    /// shape.
    unsafe extern "C" fn test_remote_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let Ok(bridge_request) = test_decode_service_message::<TestBridgeRequest>(input) else {
            return 3;
        };
        let Ok(inner_request) = bmux_ipc::decode::<TestIpcRequest>(&bridge_request.payload) else {
            return 3;
        };
        LAST_REMOTE_REQUEST.with(|slot| {
            *slot.borrow_mut() = Some(inner_request);
        });

        let response = NEXT_REMOTE_RESPONSE
            .with(|slot| slot.borrow().clone())
            .unwrap_or_else(|| {
                TestIpcResponse::Ok(TestIpcResponsePayload::ServiceInvoked {
                    payload: Vec::new(),
                })
            });
        let Ok(encoded_response) = bmux_ipc::encode(&response) else {
            return 5;
        };
        let bridge_response = TestBridgeResponse {
            payload: encoded_response,
        };
        let Ok(encoded) = test_encode_service_message(&bridge_response) else {
            return 5;
        };
        unsafe { *output_len = encoded.len() };
        if output_ptr.is_null() || encoded.len() > output_capacity {
            return 4;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
        }
        0
    }

    #[test]
    fn dispatch_remote_typed_service_round_trips_payload_and_encodes_invoke_service() {
        reset_remote_bridge_slots();
        NEXT_REMOTE_RESPONSE.with(|slot| {
            *slot.borrow_mut() = Some(TestIpcResponse::Ok(
                TestIpcResponsePayload::ServiceInvoked {
                    payload: b"remote-reply".to_vec(),
                },
            ));
        });
        let bridge = TestHostKernelBridge::from_fn(test_remote_bridge);

        let response = super::dispatch_remote_typed_service(
            Some(bridge),
            "bmux.contexts",
            "bmux.contexts.read",
            TestServiceKind::Query,
            "contexts-state",
            "list-contexts",
            b"args-bytes".to_vec(),
        )
        .expect("remote dispatch should succeed");

        assert_eq!(response, b"remote-reply".to_vec());

        LAST_REMOTE_REQUEST.with(|slot| {
            let captured = slot.borrow().clone().expect("bridge saw a request");
            match captured {
                TestIpcRequest::InvokeService {
                    capability,
                    kind,
                    interface_id,
                    operation,
                    payload,
                } => {
                    assert_eq!(capability, "bmux.contexts.read");
                    assert_eq!(kind, TestInvokeKind::Query);
                    assert_eq!(interface_id, "contexts-state");
                    assert_eq!(operation, "list-contexts");
                    assert_eq!(payload, b"args-bytes".to_vec());
                }
                other => panic!("expected InvokeService, got {other:?}"),
            }
        });
    }

    #[test]
    fn dispatch_remote_typed_service_maps_command_kind() {
        reset_remote_bridge_slots();
        NEXT_REMOTE_RESPONSE.with(|slot| {
            *slot.borrow_mut() = Some(TestIpcResponse::Ok(
                TestIpcResponsePayload::ServiceInvoked {
                    payload: Vec::new(),
                },
            ));
        });
        let bridge = TestHostKernelBridge::from_fn(test_remote_bridge);

        super::dispatch_remote_typed_service(
            Some(bridge),
            "bmux.contexts",
            "bmux.contexts.write",
            TestServiceKind::Command,
            "contexts-commands",
            "create-context",
            Vec::new(),
        )
        .expect("remote dispatch should succeed");

        LAST_REMOTE_REQUEST.with(|slot| {
            let captured = slot.borrow().clone().expect("bridge saw a request");
            if let TestIpcRequest::InvokeService { kind, .. } = captured {
                assert_eq!(kind, TestInvokeKind::Command);
            } else {
                panic!("expected InvokeService");
            }
        });
    }

    #[test]
    fn dispatch_remote_typed_service_rejects_event_kind() {
        reset_remote_bridge_slots();
        let bridge = TestHostKernelBridge::from_fn(test_remote_bridge);

        let err = super::dispatch_remote_typed_service(
            Some(bridge),
            "bmux.contexts",
            "bmux.contexts.read",
            TestServiceKind::Event,
            "contexts-events",
            "emit",
            Vec::new(),
        )
        .expect_err("event kind should not forward as InvokeService");
        let msg = err.to_string();
        assert!(msg.contains("Event"), "unexpected message: {msg}");
    }

    #[test]
    fn dispatch_remote_typed_service_propagates_server_error_response() {
        reset_remote_bridge_slots();
        NEXT_REMOTE_RESPONSE.with(|slot| {
            *slot.borrow_mut() = Some(TestIpcResponse::Err(ErrorResponse {
                code: ErrorCode::NotFound,
                message: "no provider".to_string(),
            }));
        });
        let bridge = TestHostKernelBridge::from_fn(test_remote_bridge);

        let err = super::dispatch_remote_typed_service(
            Some(bridge),
            "bmux.contexts",
            "bmux.contexts.read",
            TestServiceKind::Query,
            "contexts-state",
            "list-contexts",
            Vec::new(),
        )
        .expect_err("error response should propagate");
        let msg = err.to_string();
        assert!(msg.contains("no provider"), "unexpected message: {msg}");
    }

    #[test]
    fn dispatch_remote_typed_service_requires_kernel_bridge() {
        reset_remote_bridge_slots();
        let err = super::dispatch_remote_typed_service(
            None,
            "bmux.contexts",
            "bmux.contexts.read",
            TestServiceKind::Query,
            "contexts-state",
            "list-contexts",
            Vec::new(),
        )
        .expect_err("no bridge should yield error");
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported host operation"),
            "unexpected message: {msg}"
        );
    }
}
