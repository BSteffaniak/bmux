//! Invocation-owned generic host builtins shared with the legacy adapter.
use crate::PluginId;
use crate::loader::{
    CorePluginSettingsRequest, CorePluginSettingsResponse, CoreStorageGetRequest,
    CoreStorageGetResponse, CoreStorageSetRequest, PluginStateKey, plugin_state_key,
};
use bmux_perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing};
use bmux_plugin_runtime::InvocationScope;
use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_INTERFACE_V1, CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
    CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1, HostConnectionInfo, HostScope, HostServiceRequest,
    LogWriteLevel, PluginError, PluginHostServices, ProviderId, RegisteredService, Result,
    decode_service_message, encode_service_message,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};
use tracing::{debug, error, info, trace, warn};

fn protocol(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

/// Immutable selected-host bindings plus instance-local volatile storage.
///
/// Construct once per selected plugin instance, then share with its invocations.
/// CLI callbacks are host-private executors, not a plugin kernel grant.
pub struct InvocationHostBuiltins {
    owner: PluginId,
    connection: HostConnectionInfo,
    services: Vec<RegisteredService>,
    granted: Vec<HostScope>,
    settings: BTreeMap<String, toml::Value>,
    cli: Option<PluginHostServices>,
    volatile: Mutex<BTreeMap<PluginStateKey, Vec<u8>>>,
    contracts: BTreeMap<String, Vec<u8>>,
}

impl InvocationHostBuiltins {
    /// Bind only values selected by trusted host configuration, never payload identity.
    ///
    /// # Errors
    /// Rejects invalid owner path segments and absent selected storage roots.
    pub fn new(
        owner: PluginId,
        connection: HostConnectionInfo,
        services: Vec<RegisteredService>,
        granted_capabilities: Vec<HostScope>,
        settings: BTreeMap<String, toml::Value>,
        kernel: Option<PluginHostServices>,
    ) -> Result<Self> {
        if owner.as_str().is_empty()
            || owner.as_str() == "."
            || owner.as_str() == ".."
            || !owner
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            || connection.data_dir.is_empty()
        {
            return Err(protocol("invalid selected builtin owner or storage root"));
        }
        Ok(Self {
            owner,
            connection,
            services,
            granted: granted_capabilities,
            settings,
            cli: kernel,
            volatile: Mutex::new(BTreeMap::new()),
            contracts: BTreeMap::new(),
        })
    }

    /// Bind actual negotiated provider contract evidence, not caller-supplied hashes.
    ///
    /// # Errors
    /// Rejects invalid interface IDs or non-SHA256 contract evidence.
    pub fn with_contracts(mut self, contracts: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        if contracts
            .iter()
            .any(|(id, hash)| id.is_empty() || hash.len() != 32)
        {
            return Err(protocol("invalid builtin contract evidence"));
        }
        self.contracts = contracts;
        Ok(self)
    }

    /// Dispatch advertised generic host mechanisms, never plugin product services.
    ///
    /// # Errors
    /// Fails closed on undeclared services, denied capabilities, checked-contract
    /// mismatch, closed invocation, I/O failure or unsupported host operations.
    /// Only an advertised plugin provider returns `Ok(None)` for remote routing.
    pub fn try_invoke(
        &self,
        invocation: &InvocationScope,
        request: &HostServiceRequest<'_>,
    ) -> Result<Option<Vec<u8>>> {
        if request.payload.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol("builtin request exceeds transport bound"));
        }
        if !self
            .granted
            .iter()
            .any(|scope| scope.as_str() == request.capability)
        {
            return Err(protocol(
                "host builtin capability not granted by selected instance",
            ));
        }
        let service = self
            .services
            .iter()
            .find(|service| {
                service.capability.as_str() == request.capability
                    && service.kind == request.kind
                    && service.interface_id == request.interface
            })
            .ok_or_else(|| protocol("host service was not advertised by selected configuration"))?;
        let _admission = invocation
            .try_admit()
            .map_err(|error| protocol(error.to_string()))?;
        if matches!(service.provider, ProviderId::Plugin(_)) {
            return Ok(None);
        }
        let checked = bmux_plugin_sdk::service_contract::is_checked(request.payload);
        let payload = if checked {
            let hash = self
                .contracts
                .get(request.interface)
                .ok_or_else(|| protocol("checked builtin contract not negotiated"))?;
            bmux_plugin_sdk::service_contract::decode_for(request.interface, hash, request.payload)?
        } else {
            request.payload
        };
        let cli = |operation: &str, payload: &[u8]| {
            self.cli
                .as_ref()
                .ok_or(PluginError::UnsupportedHostOperation {
                    operation: "builtin_cli",
                })?
                .invoke(HostServiceRequest {
                    capability: request.capability,
                    kind: request.kind,
                    interface: request.interface,
                    operation,
                    payload,
                })
        };
        let response = handle_core_service_call(&BuiltinCall {
            owner: self.owner.as_str(),
            connection: &self.connection,
            service,
            operation: request.operation,
            payload,
            volatile: &self.volatile,
            cli: &cli,
            settings: &self.settings,
        })?;
        let response = if checked {
            bmux_plugin_sdk::service_contract::encode_response(&response)
        } else {
            response
        };
        if response.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol(
                "builtin response exceeds transport bound; do not replay operation",
            ));
        }
        Ok(Some(response))
    }
}

type BuiltinCli<'a> = dyn Fn(&str, &[u8]) -> Result<Vec<u8>> + 'a;

pub struct BuiltinCall<'a> {
    pub owner: &'a str,
    pub connection: &'a HostConnectionInfo,
    pub service: &'a RegisteredService,
    pub operation: &'a str,
    pub payload: &'a [u8],
    pub volatile: &'a Mutex<BTreeMap<PluginStateKey, Vec<u8>>>,
    pub cli: &'a BuiltinCli<'a>,
    pub settings: &'a BTreeMap<String, toml::Value>,
}

pub fn handle_core_service_call(call: &BuiltinCall<'_>) -> Result<Vec<u8>> {
    let BuiltinCall {
        owner: caller_plugin_id,
        connection,
        service,
        operation,
        payload,
        volatile,
        cli,
        settings: plugin_settings_map,
    } = *call;

    match (service.interface_id.as_str(), operation) {
        ("config-query/v1", "plugin_settings") => {
            let request: CorePluginSettingsRequest = decode_service_message(payload)?;
            let settings = plugin_settings_map.get(&request.plugin_id).cloned();
            encode_service_message(&CorePluginSettingsResponse { settings })
        }
        ("storage-query/v1", "get") => storage_get(caller_plugin_id, connection, payload),
        ("storage-command/v1", "set") => storage_set(caller_plugin_id, connection, payload),
        ("volatile-state-query/v1", "get") => {
            volatile_get(caller_plugin_id, connection, payload, volatile)
        }
        ("volatile-state-command/v1", "set") => {
            volatile_set(caller_plugin_id, connection, payload, volatile)
        }
        ("volatile-state-command/v1", "clear") => {
            volatile_clear(caller_plugin_id, connection, payload, volatile)
        }
        ("logging-command/v1", "write") => {
            let request: bmux_plugin_sdk::LogWriteRequest = decode_service_message(payload)?;
            emit_plugin_log(caller_plugin_id, &request)?;
            encode_service_message(&())
        }
        (
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1 | CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
        ) => cli(operation, payload),
        _ => Err(PluginError::UnsupportedHostOperation {
            operation: "call_service",
        }),
    }
}

fn storage_get(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let total_started = Instant::now();
    let decode_started = Instant::now();
    let request: CoreStorageGetRequest = decode_service_message(payload)?;
    let decode_us = decode_started.elapsed().as_micros();
    let validate_started = Instant::now();
    validate_storage_key(&request.key)?;
    let validate_us = validate_started.elapsed().as_micros();
    // Durable storage is authoritative across independent host processes.
    // A process-local positive or negative cache cannot be coherent with
    // domain-owned cross-process mutation transactions.
    let cache_us = 0_u128;
    let fs_started = Instant::now();
    let path = storage_file_path(connection, caller_plugin_id, &request.key);
    let value = match fs::File::open(path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take((bmux_plugin_sdk::abi::MAX_BUFFER_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| protocol(error.to_string()))?;
            if bytes.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
                return Err(protocol("stored value exceeds transport bound"));
            }
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(PluginError::ServiceProtocol {
                details: format!("failed reading storage value: {error}"),
            });
        }
    };
    let fs_us = fs_started.elapsed().as_micros();
    let cache_hit = false;
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

fn storage_set(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    payload: &[u8],
) -> Result<Vec<u8>> {
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
    write_storage_value(&path, &request.value).map_err(|error| PluginError::ServiceProtocol {
        details: format!("failed writing storage value: {error}"),
    })?;
    let fs_us = fs_started.elapsed().as_micros();
    let cache_us = 0_u128;
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

fn volatile_get(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    payload: &[u8],
    volatile: &Mutex<BTreeMap<PluginStateKey, Vec<u8>>>,
) -> Result<Vec<u8>> {
    let total_started = Instant::now();
    let request: bmux_plugin_sdk::VolatileStateGetRequest = decode_service_message(payload)?;
    validate_storage_key(request.key.as_str())?;
    let map_started = Instant::now();
    let value = {
        let map = volatile
            .lock()
            .map_err(|_| protocol("volatile state lock poisoned"))?;
        map.get(&plugin_state_key(
            connection,
            caller_plugin_id,
            request.key.as_str(),
        ))
        .cloned()
    };
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

fn volatile_set(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    payload: &[u8],
    volatile: &Mutex<BTreeMap<PluginStateKey, Vec<u8>>>,
) -> Result<Vec<u8>> {
    let total_started = Instant::now();
    let request: bmux_plugin_sdk::VolatileStateSetRequest = decode_service_message(payload)?;
    validate_storage_key(request.key.as_str())?;
    let map_started = Instant::now();
    {
        let mut map = volatile
            .lock()
            .map_err(|_| protocol("volatile state lock poisoned"))?;
        let key = plugin_state_key(connection, caller_plugin_id, request.key.as_str());
        let previous = map.get(&key).map_or(0, Vec::len);
        let bytes = map.values().map(Vec::len).sum::<usize>() - previous + request.value.len();
        if (!map.contains_key(&key) && map.len() >= 1024) || bytes > 16 * 1024 * 1024 {
            return Err(protocol("volatile state capacity exceeded"));
        }
        map.insert(
            plugin_state_key(connection, caller_plugin_id, request.key.as_str()),
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

fn volatile_clear(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    payload: &[u8],
    volatile: &Mutex<BTreeMap<PluginStateKey, Vec<u8>>>,
) -> Result<Vec<u8>> {
    let total_started = Instant::now();
    let request: bmux_plugin_sdk::VolatileStateClearRequest = decode_service_message(payload)?;
    validate_storage_key(request.key.as_str())?;
    let map_started = Instant::now();
    {
        let mut map = volatile
            .lock()
            .map_err(|_| protocol("volatile state lock poisoned"))?;
        map.remove(&plugin_state_key(
            connection,
            caller_plugin_id,
            request.key.as_str(),
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

pub fn emit_plugin_log(
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

pub fn write_storage_value(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::filesystem::replace(path, bytes)
}

pub fn storage_file_path(connection: &HostConnectionInfo, plugin_id: &str, key: &str) -> PathBuf {
    PathBuf::from(&connection.data_dir)
        .join("plugin-storage")
        .join(plugin_id)
        .join(format!("{key}.bin"))
}

pub fn validate_storage_key(key: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_runtime::{
        ActivationScope, InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId,
    };
    use bmux_plugin_sdk::{
        ServiceKind, StorageGetRequest, StorageGetResponse, StorageKey, StorageSetRequest,
    };

    fn scope() -> (ActivationScope, InvocationScope) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        (activation, invocation)
    }

    fn builtins(root: &Path, owner: &str) -> InvocationHostBuiltins {
        let cap = HostScope::new("bmux.storage").unwrap();
        InvocationHostBuiltins::new(
            PluginId::new(owner).unwrap(),
            HostConnectionInfo {
                config_dir: root.display().to_string(),
                config_dir_candidates: Vec::new(),
                runtime_dir: root.display().to_string(),
                data_dir: root.display().to_string(),
                state_dir: root.display().to_string(),
            },
            vec![
                RegisteredService {
                    capability: cap.clone(),
                    kind: ServiceKind::Query,
                    interface_id: "storage-query/v1".into(),
                    provider: ProviderId::Host,
                },
                RegisteredService {
                    capability: cap.clone(),
                    kind: ServiceKind::Command,
                    interface_id: "storage-command/v1".into(),
                    provider: ProviderId::Host,
                },
            ],
            vec![cap],
            BTreeMap::new(),
            None,
        )
        .unwrap()
    }

    fn read(
        host: &InvocationHostBuiltins,
        invocation: &InvocationScope,
    ) -> Result<StorageGetResponse> {
        let payload =
            encode_service_message(&StorageGetRequest::new(StorageKey::new("value").unwrap()))?;
        let bytes = host
            .try_invoke(
                invocation,
                &HostServiceRequest {
                    capability: "bmux.storage",
                    kind: ServiceKind::Query,
                    interface: "storage-query/v1",
                    operation: "get",
                    payload: &payload,
                },
            )?
            .unwrap();
        decode_service_message(&bytes)
    }

    #[tokio::test]
    async fn durable_storage_uses_exact_selected_owner_path_and_no_host_cache() {
        let root = tempfile::tempdir().unwrap();
        let a = builtins(root.path(), "example.a");
        let b = builtins(root.path(), "example.b");
        let (activation, invocation) = scope();
        let payload = encode_service_message(&StorageSetRequest::new(
            StorageKey::new("value").unwrap(),
            vec![1, 2],
        ))
        .unwrap();
        a.try_invoke(
            &invocation,
            &HostServiceRequest {
                capability: "bmux.storage",
                kind: ServiceKind::Command,
                interface: "storage-command/v1",
                operation: "set",
                payload: &payload,
            },
        )
        .unwrap();
        let path = root.path().join("plugin-storage/example.a/value.bin");
        assert_eq!(fs::read(&path).unwrap(), vec![1, 2]);
        assert_eq!(read(&a, &invocation).unwrap().value, Some(vec![1, 2]));
        assert_eq!(read(&b, &invocation).unwrap().value, None);
        fs::write(&path, [3]).unwrap();
        assert_eq!(read(&a, &invocation).unwrap().value, Some(vec![3]));
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    #[tokio::test]
    async fn closed_scope_unknown_interface_and_storage_io_fail_explicitly() {
        let root = tempfile::tempdir().unwrap();
        let host = builtins(root.path(), "example.a");
        let (activation, invocation) = scope();
        let path = root.path().join("plugin-storage/example.a/value.bin");
        fs::create_dir_all(&path).unwrap();
        assert!(read(&host, &invocation).is_err());
        assert!(
            host.try_invoke(
                &invocation,
                &HostServiceRequest {
                    capability: "bmux.storage",
                    kind: ServiceKind::Query,
                    interface: "unknown/v1",
                    operation: "get",
                    payload: &[]
                }
            )
            .is_err()
        );
        invocation.close().unwrap();
        assert!(read(&host, &invocation).is_err());
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    #[tokio::test]
    async fn only_advertised_plugin_provider_is_eligible_for_remote_routing() {
        let root = tempfile::tempdir().unwrap();
        let mut host = builtins(root.path(), "example.a");
        let (activation, invocation) = scope();
        host.services[0].provider = ProviderId::Plugin("example.provider".into());
        let payload =
            encode_service_message(&StorageGetRequest::new(StorageKey::new("value").unwrap()))
                .unwrap();
        let request = || HostServiceRequest {
            capability: "bmux.storage",
            kind: ServiceKind::Query,
            interface: "storage-query/v1",
            operation: "get",
            payload: &payload,
        };
        assert_eq!(host.try_invoke(&invocation, &request()).unwrap(), None);
        host.granted.clear();
        assert!(host.try_invoke(&invocation, &request()).is_err());
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    #[tokio::test]
    async fn cli_builtin_requires_explicit_private_executor_without_storage_fallback() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        let mut host = builtins(root.path(), "example.a");
        let (activation, invocation) = scope();
        let cap = HostScope::new(bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY).unwrap();
        host.services[0].capability = cap.clone();
        host.granted = vec![cap];
        host.services[0].interface_id = CORE_CLI_COMMAND_INTERFACE_V1.into();
        host.services[0].kind = ServiceKind::Command;
        let request = || HostServiceRequest {
            capability: bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY,
            kind: ServiceKind::Command,
            interface: CORE_CLI_COMMAND_INTERFACE_V1,
            operation: CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
            payload: b"opaque-cli-request",
        };
        assert!(host.try_invoke(&invocation, &request()).is_err());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        host.cli = Some(PluginHostServices::new(move |request| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.payload, b"opaque-cli-request");
            Err(protocol("authenticated CLI executor denied"))
        }));
        assert!(
            host.try_invoke(&invocation, &request())
                .unwrap_err()
                .to_string()
                .contains("authenticated CLI executor denied")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    #[tokio::test]
    async fn checked_builtin_uses_host_evidence_not_payload_claim() {
        let root = tempfile::tempdir().unwrap();
        let host = builtins(root.path(), "example.a");
        let (activation, invocation) = scope();
        let raw =
            encode_service_message(&StorageGetRequest::new(StorageKey::new("value").unwrap()))
                .unwrap();
        let payload =
            bmux_plugin_sdk::service_contract::encode("storage-query/v1", &[1; 32], &raw).unwrap();
        let request = || HostServiceRequest {
            capability: "bmux.storage",
            kind: ServiceKind::Query,
            interface: "storage-query/v1",
            operation: "get",
            payload: &payload,
        };
        assert!(host.try_invoke(&invocation, &request()).is_err());
        let host = host
            .with_contracts(BTreeMap::from([("storage-query/v1".into(), vec![1; 32])]))
            .unwrap();
        let response = host.try_invoke(&invocation, &request()).unwrap().unwrap();
        let response: StorageGetResponse = decode_service_message(
            bmux_plugin_sdk::service_contract::decode_response(&response).unwrap(),
        )
        .unwrap();
        assert_eq!(response.value, None);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }
}
