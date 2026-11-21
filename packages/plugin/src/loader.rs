#[path = "loader/scoped.rs"]
pub mod scoped;

#[path = "executable_identity.rs"]
mod executable_identity;
pub use executable_identity::{executable_sha256, retain_executable};

use crate::{
    CapabilityProvider, PluginDeclaration, PluginEntrypoint, PluginRegistry, RegisteredPlugin,
    ServiceCaller, discover_registered_plugins_in_roots, test_support::test_service_router,
};
use bmux_ipc::{
    InvokeServiceKind, Request as IpcRequest, Response as IpcResponse,
    ResponsePayload as IpcResponsePayload,
};
use bmux_perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing};
use bmux_plugin_runtime::{ConcurrencyGate, EffectiveConcurrencyPolicy};
use bmux_plugin_sdk::{
    CORE_CLI_BRIDGE_PROTOCOL_V1, CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
    CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1, CoreCliCommandRequest, CoreCliCommandResponse,
    HostConnectionInfo, HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse,
    HostMetadata, HostScope, NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    PROCESS_RUNTIME_ENV_PERSISTENT_WORKER, PROCESS_RUNTIME_ENV_PLUGIN_ID,
    PROCESS_RUNTIME_ENV_PROTOCOL, PROCESS_RUNTIME_PROTOCOL_V1, PROCESS_RUNTIME_TRANSPORT_STDIO_V1,
    PluginCliCommandRequest, PluginCliCommandResponse, PluginError, PluginEvent,
    PluginInvocationId, ProcessInvocationRequest, ProcessInvocationResponse, RegisteredService,
    Result, ServiceKind, ServiceRequest, ServiceResponse, decode_process_invocation_response,
    decode_service_message, encode_host_kernel_bridge_cli_command_payload,
    encode_host_kernel_bridge_plugin_command_payload, encode_process_invocation_request,
    encode_service_message,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
use tracing::{debug, warn};

const KERNEL_STATUS_OK: i32 = 0;
const KERNEL_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const PROCESS_PLUGIN_TIMEOUT_ENV_VAR: &str = "BMUX_PROCESS_PLUGIN_TIMEOUT_MS";
const PROCESS_PLUGIN_TIMEOUT_DEFAULT_MS: u64 = 30_000;

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
        let started = Instant::now();
        let result = if self.persistent_worker {
            self.invoke_persistent(plugin_id, request)
        } else {
            self.invoke_one_shot(plugin_id, argv, request)
        };
        let mut payload = PhasePayload::new("plugin.process.invoke")
            .field("plugin_id", plugin_id)
            .field(
                "runtime",
                if self.persistent_worker {
                    "persistent"
                } else {
                    "one_shot"
                },
            )
            .field("operation", process_request_operation(request))
            .field("total_us", started.elapsed().as_micros());
        match &result {
            Ok((_response, status)) => {
                payload = payload.field("status", status.code().unwrap_or(0));
            }
            Err(error) => {
                payload = payload.field("error", error.to_string());
            }
        }
        emit_phase_timing(PhaseChannel::Plugin, &payload.finish());
        result
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
                    timeout_ms: Box::new(timeout.as_millis()),
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

const fn process_request_operation(request: &ProcessInvocationRequest) -> &'static str {
    match request {
        ProcessInvocationRequest::Command { .. } => "command",
        ProcessInvocationRequest::Lifecycle { .. } => "lifecycle",
        ProcessInvocationRequest::Event { .. } => "event",
        ProcessInvocationRequest::Service { .. } => "service",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorePluginSettingsRequest {
    pub(crate) plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorePluginSettingsResponse {
    #[serde(
        serialize_with = "serialize_toml_option",
        deserialize_with = "deserialize_toml_option"
    )]
    pub(crate) settings: Option<toml::Value>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreStorageGetRequest {
    pub(crate) key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreStorageGetResponse {
    pub(crate) value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreStorageSetRequest {
    pub(crate) key: String,
    pub(crate) value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PluginStateKey {
    pub(crate) data_dir: String,
    pub(crate) plugin_id: String,
    pub(crate) key: String,
}

static VOLATILE_STATE: OnceLock<Mutex<BTreeMap<PluginStateKey, Vec<u8>>>> = OnceLock::new();

fn volatile_state() -> &'static Mutex<BTreeMap<PluginStateKey, Vec<u8>>> {
    VOLATILE_STATE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn plugin_state_key(
    connection: &HostConnectionInfo,
    plugin_id: &str,
    key: &str,
) -> PluginStateKey {
    PluginStateKey {
        data_dir: connection.data_dir.clone(),
        plugin_id: plugin_id.to_string(),
        key: key.to_string(),
    }
}

// Preserve selected invocation authority rather than consulting an inventory.
async fn selected_context_availability(
    port: Option<&bmux_plugin_sdk::PluginHostServices>,
    cancellation: Option<&bmux_plugin_sdk::CancellationToken>,
    key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
) -> Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability> {
    let check_cancellation = || {
        if cancellation.is_some_and(bmux_plugin_sdk::CancellationToken::is_cancelled) {
            Err(PluginError::InvocationCancelled)
        } else {
            Ok(())
        }
    };
    check_cancellation()?;
    key.validate()?;
    let port = port.ok_or(PluginError::UnsupportedHostOperation {
        operation: "selected_service_availability",
    })?;
    let presence = port.selected_service_availability(key).await?;
    check_cancellation()?;
    Ok(presence)
}

impl ServiceCaller for NativeCommandContext {
    fn selected_service_availability(
        &self,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(selected_context_availability(
            self.host_service_port.as_ref(),
            None,
            key,
        ))
    }
    fn host_connection(&self) -> Option<&HostConnectionInfo> {
        Some(&self.connection)
    }

    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let port = self
            .host_service_port
            .as_ref()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "invocation-bound command host service port not selected".into(),
            })?;
        port.invoke(bmux_plugin_sdk::HostServiceRequest {
            capability,
            kind,
            interface: interface_id,
            operation,
            payload: &payload,
        })
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        let port = self
            .host_service_port
            .as_ref()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "invocation-bound command kernel port not selected".into(),
            })?;
        execute_selected_context_kernel(port, &request)
    }
}

impl ServiceCaller for NativeLifecycleContext {
    fn selected_service_availability(
        &self,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(selected_context_availability(
            self.host_service_port.as_ref(),
            None,
            key,
        ))
    }
    fn host_connection(&self) -> Option<&HostConnectionInfo> {
        Some(&self.connection)
    }

    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        if let Some(port) = &self.host_service_port {
            return port.invoke(bmux_plugin_sdk::HostServiceRequest {
                capability,
                kind,
                interface: interface_id,
                operation,
                payload: &payload,
            });
        }
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
            self.host_kernel_bridge.clone(),
            &self.plugin_settings_map,
            None,
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
        execute_kernel_request(self.host_kernel_bridge.clone(), request)
    }
}

impl ServiceCaller for NativeServiceContext {
    fn selected_service_availability(
        &self,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(selected_context_availability(
            self.host_service_port.as_ref(),
            Some(&self.cancellation),
            key,
        ))
    }
    fn host_connection(&self) -> Option<&HostConnectionInfo> {
        Some(&self.connection)
    }

    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        if self.cancellation.is_cancelled() {
            return Err(PluginError::ServiceProtocol {
                details: "service invocation cancelled before nested dispatch".into(),
            });
        }
        let port = self
            .host_service_port
            .as_ref()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "invocation-bound host service port not selected".into(),
            })?;
        port.invoke(bmux_plugin_sdk::HostServiceRequest {
            capability,
            kind,
            interface: interface_id,
            operation,
            payload: &payload,
        })
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        if self.cancellation.is_cancelled() {
            return Err(PluginError::ServiceProtocol {
                details: "service invocation cancelled before kernel dispatch".into(),
            });
        }
        let port = self
            .host_service_port
            .as_ref()
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: "invocation-bound host kernel port not selected".into(),
            })?;
        execute_selected_context_kernel(port, &request)
    }
}

// Adapt only the selected invocation capability. Neither missing grants nor
// failed/oversized responses authorize replay through a legacy executable bridge.
pub fn execute_selected_context_kernel(
    port: &bmux_plugin_sdk::PluginHostServices,
    request: &bmux_ipc::Request,
) -> Result<bmux_ipc::ResponsePayload> {
    let protocol = |details: String| PluginError::ServiceProtocol { details };
    let request = bmux_ipc::encode(request).map_err(|error| protocol(error.to_string()))?;
    if request.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
        return Err(protocol(
            "kernel request exceeds negotiated buffer bound".into(),
        ));
    }
    let response = port.execute_kernel_encoded(&request)?;
    if response.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
        return Err(protocol(
            "kernel response exceeds negotiated buffer bound; do not replay operation".into(),
        ));
    }
    bmux_ipc::decode(&response).map_err(|error| protocol(error.to_string()))
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
    cancellation: Option<&bmux_plugin_sdk::CancellationToken>,
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
        .ok_or_else(|| PluginError::ServiceProtocol {
            details: format!(
                "no advertised {kind:?} service for capability '{capability}' and interface '{interface_id}'; requested operation '{operation}' was not dispatched"
            ),
        })?;

    if matches!(service.provider, bmux_plugin_sdk::ProviderId::Host) {
        return handle_core_service_call(
            caller_plugin_id,
            connection,
            &service,
            operation,
            &payload,
            host_kernel_bridge.as_ref(),
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
    // dispatch below) or must route the call to a selected endpoint.
    // The existing one-server path uses the host kernel bridge endpoint.
    // Other endpoint identities are rejected until an endpoint dispatcher
    // is registered rather than silently reaching the wrong server.
    // Providers with no recorded location are treated as local for backward
    // compatibility with tests and pre-bootstrap paths; real runtime code
    // marks every known plugin before any typed service call fires.
    if let Some(crate::ServiceLocation::Remote { endpoint }) =
        crate::global_service_locations().get(&provider_plugin_id)
    {
        if !endpoint.is_host_kernel() {
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "service endpoint '{endpoint}' has no registered typed-service dispatcher"
                ),
            });
        }
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

    // Native providers are reachable only through the caller's selected host
    // service port. Only an explicitly process-packaged provider is started on
    // demand; nothing else is loaded or substituted here.
    let search_roots = plugin_search_roots
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let registry = discover_registered_plugins_in_roots(&search_roots)?;
    let registered =
        registry
            .get(&provider_plugin_id)
            .ok_or_else(|| PluginError::MissingServiceProvider {
                provider_plugin_id: provider_plugin_id.clone(),
                capability: service.capability.as_str().to_string(),
                interface_id: service.interface_id.clone(),
            })?;
    if !matches!(
        registered.declaration.entrypoint,
        PluginEntrypoint::Process { .. }
    ) {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "native provider '{provider_plugin_id}' is reachable only through a selected host service port"
            ),
        });
    }
    let loaded = load_process_plugin(registered, host, &available_capability_map)?;
    let mut provider_context = NativeServiceContext {
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
            .chain(loaded.declaration.optional_capabilities.iter())
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
        cancellation: cancellation.cloned().unwrap_or_default(),
        host_kernel_bridge: None,
        host_service_port: None,
    };
    provider_context.host_kernel_bridge = host_kernel_bridge;
    let response = loaded.invoke_service(&provider_context)?;

    if let Some(error) = response.error {
        return Err(PluginError::ServiceInvocationFailed {
            provider_plugin_id: service.provider.to_string().into(),
            capability: service.capability.as_str().to_string(),
            interface_id: service.interface_id,
            operation: operation.to_string().into(),
            code: error.code.into(),
            message: error.message.into(),
        });
    }

    Ok(response.payload)
}

fn handle_core_service_call(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    service: &RegisteredService,
    operation: &str,
    payload: &[u8],
    host_kernel_bridge: Option<&HostKernelBridge>,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
) -> Result<Vec<u8>> {
    crate::host_builtins::handle_core_service_call(&crate::host_builtins::BuiltinCall {
        owner: caller_plugin_id,
        connection,
        service,
        operation,
        payload,
        volatile: volatile_state(),
        cli: &|operation, payload| match operation {
            CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1 => {
                encode_service_message(&execute_cli_command_request(
                    host_kernel_bridge.cloned(),
                    &decode_service_message(payload)?,
                )?)
            }
            CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1 => {
                encode_service_message(&execute_plugin_command_request(
                    host_kernel_bridge.cloned(),
                    &decode_service_message(payload)?,
                )?)
            }
            _ => Err(PluginError::UnsupportedHostOperation {
                operation: "call_service",
            }),
        },
        settings: plugin_settings_map,
    })
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
    let encoded_response = invoke_host_kernel_bridge(&bridge, encoded_request)?;
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
    let encoded_response = invoke_host_kernel_bridge(&bridge, payload)?;
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
    let encoded_response = invoke_host_kernel_bridge(&bridge, payload)?;
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

fn invoke_host_kernel_bridge(bridge: &HostKernelBridge, payload: Vec<u8>) -> Result<Vec<u8>> {
    let mut request = HostKernelBridgeRequest::new(payload);
    request.context_token = bridge.token();
    let request = encode_service_message(&request)?;
    let mut output = vec![0u8; request.len().saturating_mul(4).max(1024)];
    let mut output_len = 0usize;

    // SAFETY: the bridge came from this process's host; the request buffer is
    // readable for its length, `output` is writable for `output.len()` bytes and
    // the length out-parameter is a live local, all valid for the call.
    let status = unsafe {
        bridge.invoke(
            request.as_ptr(),
            request.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
        )
    };

    if status == KERNEL_STATUS_BUFFER_TOO_SMALL {
        output.resize(output_len, 0);
        // SAFETY: the bridge came from this process's host; the request buffer is
        // readable for its length, `output` is writable for `output.len()` bytes and
        // the length out-parameter is a live local, all valid for the call.
        let status = unsafe {
            bridge.invoke(
                request.as_ptr(),
                request.len(),
                output.as_mut_ptr(),
                output.len(),
                &raw mut output_len,
            )
        };
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

#[cfg(test)]
fn storage_file_path(connection: &HostConnectionInfo, plugin_id: &str, key: &str) -> PathBuf {
    crate::host_builtins::storage_file_path(connection, plugin_id, key)
}

#[cfg(test)]
fn validate_storage_key(key: &str) -> Result<()> {
    crate::host_builtins::validate_storage_key(key)
}

/// Process lifecycle hook names carried by the process wire protocol. They keep
/// their historical values for protocol compatibility; nothing resolves them
/// as native symbols.
const PROCESS_ACTIVATE_HOOK: &str = "bmux_plugin_activate_v1";
const PROCESS_DEACTIVATE_HOOK: &str = "bmux_plugin_deactivate_v1";

/// An explicitly process-packaged plugin. Native implementations never load
/// here: bundled and dynamic native plugins use the scoped definition loader.
pub struct LoadedPlugin {
    pub registered: RegisteredPlugin,
    pub declaration: PluginDeclaration,
    runtime: ProcessPluginRuntime,
    concurrency_gate: ConcurrencyGate,
}

impl LoadedPlugin {
    #[must_use]
    pub const fn effective_concurrency_policy(&self) -> EffectiveConcurrencyPolicy {
        EffectiveConcurrencyPolicy::new(self.declaration.concurrency)
    }

    #[must_use]
    pub fn manifest_contributions(&self) -> Vec<bmux_plugin_sdk::PluginContribution> {
        self.declaration
            .commands
            .iter()
            .cloned()
            .map(bmux_plugin_sdk::PluginContribution::command)
            .collect()
    }

    /// Process plugins have no activation-time contribution hook.
    ///
    /// # Errors
    ///
    /// Never fails; the result type matches scoped executions.
    pub const fn collect_activation_contributions(
        &self,
    ) -> bmux_plugin_sdk::Result<Vec<bmux_plugin_sdk::PluginContribution>> {
        Ok(Vec::new())
    }

    /// Collect manifest contributions, rejecting duplicates deterministically.
    ///
    /// # Errors
    ///
    /// Returns when duplicate contribution IDs are found.
    pub fn collect_contributions(&self) -> Result<Vec<bmux_plugin_sdk::PluginContribution>> {
        let mut registrar = bmux_plugin_sdk::ContributionRegistrar::new();
        for contribution in self
            .manifest_contributions()
            .into_iter()
            .chain(self.collect_activation_contributions()?)
        {
            registrar
                .register(contribution)
                .map_err(|details| PluginError::ServiceProtocol {
                    details: format!(
                        "plugin '{}' contribution registration failed: {details}",
                        self.declaration.id.as_str()
                    ),
                })?;
        }
        Ok(registrar.into_contributions())
    }

    /// Out-of-process plugins cannot register in-process typed providers.
    #[must_use]
    pub fn collect_typed_services(
        &self,
        _context: bmux_plugin_sdk::TypedServiceRegistrationContext<'_>,
    ) -> bmux_plugin_sdk::TypedServiceRegistry {
        bmux_plugin_sdk::TypedServiceRegistry::new()
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
        argv: &[String],
        request: &ProcessInvocationRequest,
    ) -> Result<(Option<ProcessInvocationResponse>, std::process::ExitStatus)> {
        // v1's data envelope is preserved, but executable same-address-space
        // bridge capabilities must never be sent to an external process.
        let mut request = request.clone();
        match &mut request {
            ProcessInvocationRequest::Command {
                context: Some(context),
                ..
            } => {
                context.host_kernel_bridge = None;
            }
            ProcessInvocationRequest::Lifecycle { context, .. } => {
                context.host_kernel_bridge = None;
            }
            ProcessInvocationRequest::Service { context, .. } => {
                context.host_kernel_bridge = None;
            }
            _ => {}
        }
        self.runtime
            .invoke(self.declaration.id.as_str(), argv, &request)
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
    /// Returns an error when the plugin does not declare the command or the
    /// process invocation fails.
    pub fn run_command(&self, command_name: &str, arguments: &[String]) -> Result<i32> {
        self.run_command_with_context(command_name, arguments, None)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command or the
    /// process invocation fails.
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
    /// Returns an error when the plugin does not declare the command or the
    /// process invocation fails.
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
        self.run_process_command(command_name, arguments, context)
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle payload cannot be encoded or the
    /// process invocation fails.
    pub fn activate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle(PROCESS_ACTIVATE_HOOK, context)
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle payload cannot be encoded or the
    /// process invocation fails.
    pub fn deactivate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle(PROCESS_DEACTIVATE_HOOK, context)
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
    /// Returns an error when the event payload cannot be encoded or the
    /// process invocation fails.
    pub fn dispatch_event(&self, event: &PluginEvent) -> Result<Option<i32>> {
        if !self.receives_event(event) {
            return Ok(None);
        }
        let request = ProcessInvocationRequest::Event {
            protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
            invocation_id: PluginInvocationId::new(),
            plugin_id: self.declaration.id.as_str().to_string(),
            event: event.clone(),
        };
        let empty_argv: Vec<String> = Vec::new();
        let (response, _) = self.invoke_process(&empty_argv, &request)?;
        match response {
            None => Ok(None),
            Some(ProcessInvocationResponse::Event {
                protocol_version,
                status,
                ..
            }) => {
                Self::ensure_process_protocol_version("event", protocol_version)?;
                Ok(status)
            }
            Some(ProcessInvocationResponse::Error {
                protocol_version,
                details,
                status,
                ..
            }) => {
                Self::ensure_process_protocol_version("error", protocol_version)?;
                status.map_or(Err(PluginError::ServiceProtocol { details }), |status| {
                    Ok(Some(status))
                })
            }
            Some(other) => Err(Self::unexpected_process_response("event", &other)),
        }
    }

    /// # Errors
    ///
    /// Returns an error when the payload uses checked contracts (unsupported by
    /// the process protocol) or the process invocation fails.
    pub fn invoke_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        let _permit =
            self.concurrency_gate
                .acquire()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: error.to_string(),
                })?;
        if bmux_plugin_sdk::service_contract::is_checked(&context.request.payload) {
            return Err(PluginError::ServiceProtocol {
                details: "checked service contracts unsupported by process provider".into(),
            });
        }
        self.invoke_process_service(context)
    }

    fn invoke_process_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        let invocation_id = context.cancellation.invocation_id.clone();
        let request = ProcessInvocationRequest::Service {
            protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
            invocation_id: invocation_id.clone(),
            cancellation: context.cancellation.clone(),
            plugin_id: self.declaration.id.as_str().to_string(),
            context: context.clone(),
        };
        let empty_argv: Vec<String> = Vec::new();
        let (response, _) = self.invoke_process(&empty_argv, &request)?;
        match response {
            Some(ProcessInvocationResponse::Service {
                protocol_version,
                invocation_id: response_invocation_id,
                response,
            }) => {
                Self::ensure_process_protocol_version("service", protocol_version)?;
                if response_invocation_id != invocation_id {
                    return Err(PluginError::ServiceProtocol {
                        details: format!(
                            "process service response invocation id mismatch: expected {invocation_id}, got {response_invocation_id}",
                        ),
                    });
                }
                Ok(response)
            }
            Some(ProcessInvocationResponse::Error {
                protocol_version,
                details,
                ..
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

    fn run_lifecycle(&self, hook: &str, context: &NativeLifecycleContext) -> Result<i32> {
        let total_started = Instant::now();
        let encode_started = Instant::now();
        let context = context.clone();
        // The lifecycle context serializes exactly the frozen native ABI 1.0
        // fields, so one encoding serves both ABI 1.0 and 1.1 plugins (see
        // `lifecycle_context_serializes_settings_and_host`).
        let payload = encode_service_message(&context).map_err(|_| {
            PluginError::InvalidNativeLifecycleInput {
                plugin_id: self.declaration.id.as_str().to_string(),
            }
        })?;
        let encode_us = encode_started.elapsed().as_micros();

        let call_started = Instant::now();
        let status = {
            let invocation_id = PluginInvocationId::new();
            let request = ProcessInvocationRequest::Lifecycle {
                protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
                invocation_id,
                plugin_id: self.declaration.id.as_str().to_string(),
                symbol: hook.to_string(),
                context,
            };
            let empty_argv: Vec<String> = Vec::new();
            let (response, status) = self.invoke_process(&empty_argv, &request)?;
            match response {
                Some(ProcessInvocationResponse::Lifecycle {
                    protocol_version,
                    status,
                    ..
                }) => {
                    Self::ensure_process_protocol_version("lifecycle", protocol_version)?;
                    status
                }
                Some(ProcessInvocationResponse::Error {
                    protocol_version,
                    details,
                    status,
                    ..
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
        };
        let call_us = call_started.elapsed().as_micros();

        emit_phase_timing(
            PhaseChannel::Plugin,
            &PhasePayload::new("plugin.lifecycle.invoke")
                .field("plugin_id", self.declaration.id.as_str())
                .field("hook", lifecycle_hook_name(hook))
                .field("backend", "process")
                .field("encoded_context_len", payload.len())
                .field("encode_us", encode_us)
                .field("call_us", call_us)
                .field("status", status)
                .field("total_us", total_started.elapsed().as_micros())
                .finish(),
        );

        Ok(status)
    }

    fn run_process_command(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<(i32, bmux_plugin_sdk::PluginCommandOutcome)> {
        let request = ProcessInvocationRequest::Command {
            protocol_version: PROCESS_RUNTIME_PROTOCOL_V1,
            invocation_id: PluginInvocationId::new(),
            plugin_id: self.declaration.id.as_str().to_string(),
            command_name: command_name.to_string(),
            arguments: arguments.to_vec(),
            context: context.cloned(),
        };
        let argv = std::iter::once(command_name.to_string())
            .chain(arguments.iter().cloned())
            .collect::<Vec<_>>();
        let (response, status) = self.invoke_process(&argv, &request)?;

        if let Some(response) = response {
            return match response {
                ProcessInvocationResponse::Command {
                    protocol_version,
                    status,
                    outcome,
                    ..
                } => {
                    Self::ensure_process_protocol_version("command", protocol_version)?;
                    Ok((status, outcome.unwrap_or_default()))
                }
                ProcessInvocationResponse::Error {
                    protocol_version,
                    details,
                    ..
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

#[allow(clippy::missing_const_for_fn)] // const str matching is not stable on this toolchain.
fn lifecycle_hook_name(hook: &str) -> &'static str {
    match hook {
        PROCESS_ACTIVATE_HOOK => "activate",
        PROCESS_DEACTIVATE_HOOK => "deactivate",
        _ => "unknown",
    }
}

#[derive(Debug, Clone)]
pub struct NativePluginLoader {
    bundled_executable: Arc<scoped::BundledExecutableCache>,
}

impl Default for NativePluginLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl NativePluginLoader {
    /// Resolve an already-selected candidate into a fresh scoped execution. This
    /// path selects native v2 explicitly and never falls back to legacy symbols.
    ///
    /// # Safety
    /// The selected artifact must be trusted native code.
    ///
    /// # Errors
    /// Selected artifact validation or scoped creation fails.
    pub unsafe fn load_selected_execution(
        selected: &crate::SelectedPluginCandidate,
        runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
        available_capabilities: &[String],
        callback_runtime: tokio::runtime::Handle,
    ) -> Result<Arc<scoped::LoadedExecution>> {
        let artifact = unsafe { scoped::DefinitionArtifact::load_selected(selected) }?;
        let execution = artifact.create(runtime, selected.role(), available_capabilities)?;
        execution.bind_callback_runtime(callback_runtime)?;
        Ok(execution)
    }

    /// Load with this startup/runtime owner's retained executable observation.
    /// Clones share only the immutable artifact observation, never instances.
    /// Dynamic artifacts are still copied and hashed independently on every load.
    /// Use a new loader owner when deliberately selecting rebuilt executable bytes.
    ///
    /// # Safety
    /// The selected artifact must be trusted native code.
    ///
    /// # Errors
    /// Executable observation, selected artifact validation, or scoped creation fails.
    pub unsafe fn load_selected_execution_with_owner(
        &self,
        selected: &crate::SelectedPluginCandidate,
        runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
        available_capabilities: &[String],
        callback_runtime: tokio::runtime::Handle,
        cli_command: Option<&str>,
    ) -> Result<Arc<scoped::LoadedExecution>> {
        let artifact = unsafe {
            scoped::DefinitionArtifact::load_selected_with_owner(selected, &self.bundled_executable)
        }?;
        let execution = artifact.create_for_command(
            runtime,
            selected.role(),
            available_capabilities,
            cli_command,
        )?;
        execution.bind_callback_runtime(callback_runtime)?;
        Ok(execution)
    }

    /// Create from a host-local factory for an already-selected bundled artifact.
    /// The supplied definition does not replace the global registration. Its full
    /// embedded declaration is validated against selection and this loader's
    /// retained executable identity before any factory runs. Dynamic selections
    /// reject the override rather than changing packaging or falling back.
    ///
    /// # Safety
    /// The factory must be trusted code belonging to the selected bundled executable.
    /// Metadata validation alone cannot prove the provenance of executable closures.
    ///
    /// # Errors
    /// Returns an error for a dynamic selection, executable observation or metadata
    /// mismatch, unavailable role/capabilities, or failed scoped construction.
    pub unsafe fn load_selected_execution_with_definition(
        &self,
        selected: &crate::SelectedPluginCandidate,
        runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
        available_capabilities: &[String],
        definition: bmux_plugin_sdk::definition::PluginDefinition,
        callback_runtime: tokio::runtime::Handle,
        cli_command: Option<&str>,
    ) -> Result<Arc<scoped::LoadedExecution>> {
        let artifact = unsafe {
            scoped::DefinitionArtifact::load_selected_with_definition(
                selected,
                &self.bundled_executable,
                Some(definition),
            )
        }?;
        let execution = artifact.create_for_command(
            runtime,
            selected.role(),
            available_capabilities,
            cli_command,
        )?;
        execution.bind_callback_runtime(callback_runtime)?;
        Ok(execution)
    }

    /// Inert unified static artifact; creation is explicit and host-scoped.
    #[must_use]
    pub fn definition_artifact(
        definition: bmux_plugin_sdk::definition::PluginDefinition,
    ) -> scoped::DefinitionArtifact {
        scoped::DefinitionArtifact::from_definition(definition)
    }

    /// Explicit v2 selection. No v1 reinterpretation or activation during discovery.
    ///
    /// # Safety
    /// The selected library must satisfy the trusted native descriptor ABI.
    ///
    /// # Errors
    /// Loading or native descriptor validation fails.
    pub unsafe fn load_definition_artifact(path: &Path) -> Result<scoped::DefinitionArtifact> {
        unsafe { scoped::DefinitionArtifact::load(path) }
    }

    /// Register inert static factories, never process-global implementation state.
    ///
    /// # Errors
    /// The definition is duplicated or the registry is poisoned.
    pub fn register_definition(
        definition: bmux_plugin_sdk::definition::PluginDefinition,
    ) -> Result<()> {
        crate::static_definition_registry::register_static_definition(definition)
    }

    /// Look up an inert registered static definition without creating an instance.
    ///
    /// # Errors
    /// The static definition registry is poisoned.
    pub fn registered_definition(plugin_id: &str) -> Result<Option<scoped::DefinitionArtifact>> {
        crate::static_definition_registry::static_definition(plugin_id)
            .map(|definition| definition.map(scoped::DefinitionArtifact::from_definition))
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            bundled_executable: Arc::new(scoped::BundledExecutableCache::default()),
        }
    }

    /// Start an explicitly process-packaged plugin. Native plugins are never
    /// loaded here; they use the scoped selected-execution paths above.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible or is not a process plugin.
    pub fn load_process_plugin(
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
                runtime: ProcessPluginRuntime {
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
                },
                concurrency_gate: concurrency_gate_for(&registered_plugin.declaration),
            });
        }

        Err(PluginError::UnsupportedPluginRuntime {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            runtime: "native outside the scoped definition loader".to_string(),
        })
    }
}

/// # Errors
///
/// Returns an error when the plugin is incompatible or is not a process plugin.
pub fn load_process_plugin(
    registered_plugin: &RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    NativePluginLoader::new().load_process_plugin(registered_plugin, host, available_capabilities)
}

fn concurrency_gate_for(declaration: &PluginDeclaration) -> ConcurrencyGate {
    ConcurrencyGate::new(declaration.concurrency)
}

#[cfg(test)]
mod tests {
    use super::{ConcurrencyGate, LoadedPlugin};
    use crate::{PluginManifest, PluginRegistry, ServiceCaller};
    use bmux_plugin_sdk::{
        ApiVersion, HostMetadata, NativeLifecycleContext, NativeServiceContext, PluginEvent,
        PluginEventKind, ServiceEnvelopeKind, decode_service_envelope, decode_service_message,
        encode_service_envelope, encode_service_message,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    use std::ptr;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::Duration;

    thread_local! {
        static KERNEL_REQUESTS: RefCell<Vec<bmux_ipc::Request>> = const { RefCell::new(Vec::new()) };
        static OMIT_CURRENT_CLIENT_FROM_LIST: Cell<bool> = const { Cell::new(false) };
    }

    #[test]
    fn host_storage_validation_rejects_raw_invalid_keys() {
        assert!(super::validate_storage_key("theme_settings.performance").is_ok());
        assert!(super::validate_storage_key("theme_settings:performance").is_err());
        assert!(super::validate_storage_key("bad/key").is_err());
        assert!(super::validate_storage_key("").is_err());
    }

    #[cfg(unix)]
    const PERSISTENT_WORKER_REUSE_SCRIPT: &str = r"#!/bin/sh
printf 'BMUXPRC1\000\000\000\004\000\001\002\000'
sleep 0.05
printf 'BMUXPRC1\000\000\000\004\000\001\004\000'
sleep 60
";

    // Consume one complete request before replying. An unsolicited response
    // followed by immediate exit races the host's pre-request child-exit check
    // and does not model a request/response worker.
    #[cfg(unix)]
    const PERSISTENT_WORKER_RESPAWN_SCRIPT: &str = r"
import struct
import sys


def read_exact(length):
    data = bytearray()
    while len(data) < length:
        part = sys.stdin.buffer.read(length - len(data))
        if not part:
            raise EOFError('incomplete process request frame')
        data.extend(part)
    return bytes(data)


header = read_exact(12)
if header[:8] != b'BMUXPRC1':
    raise ValueError('invalid process request magic')
length = struct.unpack('>I', header[8:])[0]
if length > 16 * 1024 * 1024:
    raise ValueError('process request exceeds fixture bound')
read_exact(length)
sys.stdout.buffer.write(b'BMUXPRC1\x00\x00\x00\x04\x00\x01\x02\x00')
sys.stdout.buffer.flush()
";

    #[cfg(unix)]
    const PERSISTENT_WORKER_DROP_SCRIPT_TEMPLATE: &str = r#"#!/bin/sh
printf '%s' "$$" > {pid_file:?}
printf 'BMUXPRC1\000\000\000\004\000\001\002\000'
sleep 60
"#;

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
            let exit_code = if command_request.plugin_id == "bmux.tabs"
                && command_request.command_name == "new-tab"
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

    /// A process-packaged plugin that is never started: tests below exercise
    /// only declaration-level behavior.
    fn unstarted_process_test_plugin(manifest_toml: &str) -> LoadedPlugin {
        let manifest = PluginManifest::from_toml_str(manifest_toml).expect("manifest should parse");
        let declaration = manifest.to_declaration().expect("declaration should build");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");
        LoadedPlugin {
            registered: registry
                .get(declaration.id.as_str())
                .expect("plugin should exist")
                .clone(),
            declaration,
            runtime: super::ProcessPluginRuntime {
                command: "unused-test-process".to_string(),
                args: Vec::new(),
                current_dir: None,
                persistent_worker: false,
                persistent: std::sync::Arc::new(std::sync::Mutex::new(None)),
                metrics: std::sync::Arc::new(super::ProcessRuntimeMetrics::default()),
            },
            concurrency_gate: ConcurrencyGate::new(
                bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
            ),
        }
    }

    const PROCESS_COMMAND_MANIFEST: &str = r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
runtime = "process"
entry = "unused-test-process"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#;

    #[test]
    fn loaded_plugin_reports_declared_commands() {
        let loaded = unstarted_process_test_plugin(PROCESS_COMMAND_MANIFEST);
        assert_eq!(loaded.commands().len(), 1);
        assert!(loaded.supports_command("hello"));
        assert!(loaded.run_command("missing", &[]).is_err());
    }

    #[test]
    fn process_plugin_contributes_only_manifest_commands() {
        let loaded = unstarted_process_test_plugin(PROCESS_COMMAND_MANIFEST);
        let contributions = loaded
            .collect_contributions()
            .expect("contributions should collect");
        let ids: Vec<&str> = contributions
            .iter()
            .map(bmux_plugin_sdk::PluginContribution::id)
            .collect();
        assert_eq!(ids, ["command:hello"]);
    }

    #[test]
    fn native_plugins_never_load_through_the_process_loader() {
        let mut registry = PluginRegistry::new();
        registry
            .register_bundled_manifest(
                "id = 'test.native'\nname = 'Native'\nversion = '0.1.0'\n[plugin_api]\nminimum = '1.0'\n[native_abi]\nminimum = '1.0'\n",
            )
            .expect("manifest should register");
        let registered = registry.get("test.native").expect("plugin should exist");
        let error = super::load_process_plugin(
            registered,
            &HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 1),
            },
            &BTreeMap::new(),
        )
        .err()
        .expect("native plugin must be rejected");
        assert!(matches!(
            error,
            bmux_plugin_sdk::PluginError::UnsupportedPluginRuntime { .. }
        ));
    }

    #[test]
    fn cancelled_service_context_rejects_nested_dispatch_before_routing() {
        use crate::ServiceCaller;
        let mut context = native_test_service_context(Vec::new());
        context.cancellation.cancel();
        let error = context
            .call_service_raw(
                "test.capability",
                bmux_plugin_sdk::ServiceKind::Command,
                "test-command/v1",
                "create",
                Vec::new(),
            )
            .expect_err("cancelled parent must not dispatch child work");
        assert!(
            error
                .to_string()
                .contains("cancelled before nested dispatch")
        );
    }

    #[test]
    fn cancelled_service_context_rejects_kernel_dispatch_before_bridge_lookup() {
        use crate::ServiceCaller;
        let mut context = native_test_service_context(Vec::new());
        context.cancellation.cancel();
        let captured = crate::TypedServiceCaller::from_service_context(&context);
        assert!(
            captured
                .execute_kernel_request(bmux_ipc::Request::Ping)
                .unwrap_err()
                .to_string()
                .contains("cancelled before dispatch")
        );
        let error = context
            .execute_kernel_request(bmux_ipc::Request::Ping)
            .expect_err("cancelled invocation must not enter the kernel bridge");
        assert!(
            error
                .to_string()
                .contains("cancelled before kernel dispatch")
        );
    }

    #[test]
    fn native_service_nested_dispatch_requires_selected_port() {
        let context = native_test_service_context(Vec::new());
        let error = context
            .call_service_raw(
                "test.service",
                bmux_plugin_sdk::ServiceKind::Query,
                "test-service",
                "echo",
                vec![],
            )
            .unwrap_err();
        assert!(error.to_string().contains("host service port not selected"));
        let mut context =
            context.with_host_service_port(bmux_plugin_sdk::PluginHostServices::new(|request| {
                assert_eq!(request.operation, "echo");
                Ok(request.payload.to_vec())
            }));
        assert_eq!(
            context
                .call_service_raw(
                    "test.service",
                    bmux_plugin_sdk::ServiceKind::Query,
                    "test-service",
                    "echo",
                    vec![7]
                )
                .unwrap(),
            vec![7]
        );
        context.cancellation.cancel();
        assert!(
            context
                .call_service_raw(
                    "test.service",
                    bmux_plugin_sdk::ServiceKind::Query,
                    "test-service",
                    "echo",
                    vec![]
                )
                .is_err()
        );
    }

    pub(super) fn native_test_service_context(payload: Vec<u8>) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "test.plugin".to_string(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: bmux_plugin_sdk::HostScope::new("test.service")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "test-service".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin("test.plugin".to_string()),
                },
                operation: "echo".to_string(),
                payload,
            },
            required_capabilities: vec!["test.service".to_string()],
            provided_capabilities: vec!["test.service".to_string()],
            services: Vec::new(),
            available_capabilities: vec!["test.service".to_string()],
            enabled_plugins: vec!["test.plugin".to_string()],
            plugin_search_roots: Vec::new(),
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
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
            host_service_port: None,
        }
    }

    #[cfg(unix)]
    fn unix_process_exists(pid: u32) -> bool {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
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

        super::load_process_plugin(
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

    #[cfg(unix)]
    fn process_runtime_metrics_snapshot(
        loaded: &LoadedPlugin,
    ) -> super::ProcessRuntimeMetricsSnapshot {
        loaded.runtime.metrics.snapshot()
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
# Consume the one-shot request before exiting; `true` races the parent's pipe write.
entry = "sh"
entry_args = ["-c", "cat >/dev/null"]

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

        let loaded = super::load_process_plugin(
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
            "python3",
        );

        let args: Vec<String> = Vec::new();
        let first = loaded
            .run_command("hello", &args)
            .expect("first worker command should run");
        // Observe this exact owned child, not a PID probe or a guessed delay.
        // Keep a deadline so a broken fixture cannot hang the test indefinitely.
        let runtime = &loaded.runtime;
        {
            let mut guard = runtime.persistent.lock().expect("worker lock");
            let worker = guard.as_mut().expect("first worker retained");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = worker.child.try_wait().expect("poll owned first worker") {
                    assert!(status.success(), "first worker failed: {status}");
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "first worker did not exit"
                );
                thread::yield_now();
            }
            drop(guard);
        }
        let second = loaded
            .run_command("hello", &args)
            .expect("second worker command should run after respawn");

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        let metrics = process_runtime_metrics_snapshot(&loaded);
        assert!(metrics.persistent_respawns >= 1);
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
            host_service_port: None,
            capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
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
        let frozen = context.legacy_wire_snapshot();
        let payload = encode_service_message(&frozen).expect("legacy payload should encode");
        let legacy: bmux_plugin_sdk::NativeLifecycleContextV1 =
            decode_service_message(&payload).expect("frozen legacy consumer should decode");
        assert_eq!(legacy, frozen);
        // The frozen adapter also preserves positional encoding used by legacy
        // fixtures.
        let positional =
            bmux_ipc::encode(&frozen).expect("legacy positional snapshot should encode");
        let positional_legacy: bmux_plugin_sdk::NativeLifecycleContextV1 =
            bmux_ipc::decode(&positional).expect("legacy positional snapshot should decode");
        assert_eq!(positional_legacy, frozen);
        let modern: NativeLifecycleContext = decode_service_message(&payload)
            .expect("modern consumer should accept legacy typed fields");
        assert_eq!(modern, context);
        // Lifecycle dispatch sends one encoding to every plugin ABI, so the
        // current context must stay byte-identical to the frozen ABI 1.0 wire.
        let mut modern = modern;
        assert_eq!(
            encode_service_message(&modern).expect("context should encode"),
            payload,
            "ABI 1.0 payload must be byte-identical to the current lifecycle context"
        );
        modern.host.plugin_abi_version = ApiVersion::new(1, 1);
        assert_eq!(
            encode_service_message(&modern).expect("ABI 1.1 should encode"),
            encode_service_message(&modern.legacy_wire_snapshot())
                .expect("legacy snapshot should encode"),
            "ABI 1.1 lifecycle payload must match the frozen ABI 1.0 wire"
        );
    }

    // Test-selected host implementation, explicitly bound rather than discovered
    // by the command context. Exercise the generic host authorization separately.
    fn bind_command_test_host(
        mut context: super::NativeCommandContext,
    ) -> super::NativeCommandContext {
        let selected = context.clone();
        context.host_service_port =
            Some(bmux_plugin_sdk::PluginHostServices::new(move |request| {
                super::call_service_raw(
                    &selected.plugin_id,
                    selected.caller_client_id,
                    &selected.required_capabilities,
                    &selected.provided_capabilities,
                    &selected.services,
                    &selected.available_capabilities,
                    &selected.enabled_plugins,
                    &selected.plugin_search_roots,
                    &selected.host,
                    &selected.connection,
                    selected.host_kernel_bridge.clone(),
                    &selected.plugin_settings_map,
                    None,
                    request.capability,
                    request.kind,
                    request.interface,
                    request.operation,
                    request.payload.to_vec(),
                )
            }));
        context
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
                interface_id: "permissions-state".to_string(),
                provider: bmux_plugin_sdk::ProviderId::Plugin("bmux.permissions".to_string()),
            }],
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        assert!(
            context
                .call_service_raw(
                    "example.read",
                    bmux_plugin_sdk::ServiceKind::Query,
                    "example-state/v1",
                    "list",
                    Vec::new(),
                )
                .unwrap_err()
                .to_string()
                .contains("port not selected")
        );
        let context = bind_command_test_host(context);

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                bmux_plugin_sdk::ServiceKind::Query,
                "permissions-state",
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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                bmux_plugin_sdk::ServiceKind::Query,
                "permissions-state",
                "list",
                Vec::new(),
            )
            .expect_err("missing service registration should fail");
        let super::PluginError::ServiceProtocol { details } = error else {
            panic!("expected service availability error: {error}");
        };
        assert!(details.contains("no advertised Query service"));
        assert!(details.contains("permissions-state"));
        assert!(details.contains("was not dispatched"));
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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

        // A declared capability is not evidence that a particular interface
        // version or service kind is implemented. Reject before host dispatch.
        for (kind, interface) in [
            (bmux_plugin_sdk::ServiceKind::Query, "config-query/v2"),
            (bmux_plugin_sdk::ServiceKind::Command, "config-query/v1"),
        ] {
            let error = context
                .call_service_raw(
                    "bmux.config.read",
                    kind,
                    interface,
                    "plugin_settings",
                    Vec::new(),
                )
                .expect_err("unadvertised service must not dispatch");
            let super::PluginError::ServiceProtocol { details } = error else {
                panic!("expected explicit service availability error: {error}");
            };
            assert!(details.contains("no advertised"));
            assert!(details.contains("bmux.config.read"));
            assert!(details.contains(interface));
            assert!(details.contains("was not dispatched"));
        }

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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

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

        // Another host process can commit while this caller stays alive. Both
        // positive and negative observations must be refreshed from durable IO.
        let path = super::storage_file_path(&context.connection, "caller.plugin", "theme");
        for expected in [None, Some(b"other-host".to_vec())] {
            if let Some(bytes) = &expected {
                crate::host_builtins::write_storage_value(&path, bytes).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            let response: super::CoreStorageGetResponse = context
                .call_service(
                    "bmux.storage",
                    bmux_plugin_sdk::ServiceKind::Query,
                    "storage-query/v1",
                    "get",
                    &super::CoreStorageGetRequest {
                        key: "theme".into(),
                    },
                )
                .unwrap();
            assert_eq!(response.value, expected);
        }

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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

        let response: bmux_plugin_sdk::PluginCliCommandResponse = context
            .call_service(
                bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
                bmux_plugin_sdk::ServiceKind::Command,
                bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1,
                bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
                &bmux_plugin_sdk::PluginCliCommandRequest::new(
                    "bmux.tabs".to_string(),
                    "new-tab".to_string(),
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
            active_keybindings: Vec::new(),
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
            host_service_port: None,
        };
        let context = bind_command_test_host(context);

        let mut request = bmux_plugin_sdk::PluginCliCommandRequest::new(
            "bmux.tabs".to_string(),
            "new-tab".to_string(),
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
                    interface_id: "permissions-state".to_string(),
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
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
            host_service_port: None,
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
        let loaded = unstarted_process_test_plugin(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
runtime = "process"
entry = "unused-test-process"

[[event_subscriptions]]
kinds = ["bmux.core/server-started"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        );

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
        assert!(!source.contains("\"tabs-state\""));
        assert!(!source.contains("\"tabs-commands\""));
        assert!(!source.contains("\"tabs-events\""));
    }

    // ── Remote typed-service dispatch tests ──────────────────────────
    //
    // These tests exercise `dispatch_remote_typed_service` in isolation:
    // they install a fake `HostKernelBridge` function pointer, verify
    // that a typed call is encoded as `Request::InvokeService` with the
    // supplied capability / kind / interface / operation / payload, and
    // that a `ResponsePayload::ServiceInvoked` reply is decoded back to
    // the caller. The goal is to lock in the contract the rest of the
    // runtime (attach / CLI) depends on: a remote host-kernel endpoint
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
