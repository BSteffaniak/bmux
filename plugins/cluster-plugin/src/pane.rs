#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;

use bmux_clients_plugin_api::clients_state as api_clients_state;
use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands as api_attach_runtime_commands,
    pane_runtime_commands as api_pane_runtime_commands,
    pane_runtime_state as api_pane_runtime_state,
};
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
use bmux_plugin_sdk::{CoreCliCommandRequest, StorageGetRequest, StorageSetRequest};
use bmux_sessions_plugin_api::sessions_state as api_sessions_state;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: uuid::Uuid,
    pub name: Option<String>,
    pub client_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: uuid::Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    ById(uuid::Uuid),
    ByName(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSelector {
    ById(uuid::Uuid),
    ByIndex(u32),
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateResponse {
    pub id: uuid::Uuid,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelectRequest {
    pub selector: SessionSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelectResponse {
    pub session_id: uuid::Uuid,
    pub attach_token: uuid::Uuid,
    pub expires_at_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneListRequest {
    pub session: Option<SessionSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneListResponse {
    pub panes: Vec<PaneSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
    pub direction: PaneSplitDirection,
    pub name: Option<String>,
    pub command: PaneLaunchCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchResponse {
    pub id: uuid::Uuid,
    pub session_id: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCloseRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCloseResponse {
    pub id: uuid::Uuid,
    pub session_id: uuid::Uuid,
    pub session_closed: bool,
}

pub trait ClusterRuntimeOps {
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String>;
    fn session_list(&self) -> Result<SessionListResponse, String>;
    fn session_create(
        &self,
        request: &SessionCreateRequest,
    ) -> Result<SessionCreateResponse, String>;
    fn session_select(
        &self,
        request: &SessionSelectRequest,
    ) -> Result<SessionSelectResponse, String>;
    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse, String>;
    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse, String>;
    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse, String>;
    fn storage_get(
        &self,
        request: &StorageGetRequest,
    ) -> Result<bmux_plugin_sdk::StorageGetResponse, String>;
    fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String>;
}

impl<T: HostRuntimeApi + Sync> ClusterRuntimeOps for T {
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
        HostRuntimeApi::core_cli_command_run_path(self, request).map_err(|error| error.to_string())
    }

    fn session_list(&self) -> Result<SessionListResponse, String> {
        session_list(self)
    }

    fn session_create(
        &self,
        request: &SessionCreateRequest,
    ) -> Result<SessionCreateResponse, String> {
        session_create(self, request)
    }

    fn session_select(
        &self,
        request: &SessionSelectRequest,
    ) -> Result<SessionSelectResponse, String> {
        session_select(self, request)
    }

    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse, String> {
        pane_list(self, request)
    }

    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse, String> {
        pane_launch(self, request)
    }

    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse, String> {
        pane_close(self, request)
    }

    fn storage_get(
        &self,
        request: &StorageGetRequest,
    ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
        HostRuntimeApi::storage_get(self, request).map_err(|error| error.to_string())
    }

    fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String> {
        HostRuntimeApi::storage_set(self, request).map_err(|error| error.to_string())
    }
}

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

pub fn typed_service_error(operation: &'static str, err: impl std::fmt::Display) -> String {
    format!("{operation} failed: {err}")
}

fn session_selector_to_api(selector: &SessionSelector) -> api_sessions_state::SessionSelector {
    match selector {
        SessionSelector::ById(id) => api_sessions_state::SessionSelector {
            id: Some(*id),
            name: None,
        },
        SessionSelector::ByName(name) => api_sessions_state::SessionSelector {
            id: None,
            name: Some(name.clone()),
        },
    }
}

fn session_selector_to_attach_api(
    selector: &SessionSelector,
) -> api_attach_runtime_commands::SessionSelector {
    match selector {
        SessionSelector::ById(id) => api_attach_runtime_commands::SessionSelector {
            id: Some(*id),
            name: None,
        },
        SessionSelector::ByName(name) => api_attach_runtime_commands::SessionSelector {
            id: None,
            name: Some(name.clone()),
        },
    }
}

fn session_list(caller: &(impl ServiceCaller + Sync)) -> Result<SessionListResponse, String> {
    let mut client = dispatch_client(caller);
    let sessions = bmux_plugin::block_on_typed_dispatch(api_sessions_state::client::list_sessions(
        &mut client,
    ))
    .map_err(|err| typed_service_error("sessions-state/list-sessions", err))?
    .into_iter()
    .map(|session| SessionSummary {
        id: session.id,
        name: session.name,
        client_count: session.client_count,
    })
    .collect();
    Ok(SessionListResponse { sessions })
}

fn session_create(
    caller: &(impl ServiceCaller + Sync),
    request: &SessionCreateRequest,
) -> Result<SessionCreateResponse, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::new_session_with_runtime(
            &mut client,
            request.name.clone(),
        ),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/new-session-with-runtime", err))?;
    let ack = result.map_err(|err| format!("new-session-with-runtime failed: {err:?}"))?;
    Ok(SessionCreateResponse {
        id: ack.session_id,
        name: request.name.clone(),
    })
}

fn session_select(
    caller: &(impl ServiceCaller + Sync),
    request: &SessionSelectRequest,
) -> Result<SessionSelectResponse, String> {
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_attach_runtime_commands::client::attach_session(
            &mut client,
            session_selector_to_attach_api(&request.selector),
            true,
        ))
        .map_err(|err| typed_service_error("attach-runtime-commands/attach-session", err))?;
    let grant = result.map_err(|err| format!("attach-session failed: {err:?}"))?;
    Ok(SessionSelectResponse {
        session_id: grant.session_id,
        attach_token: grant.token,
        expires_at_epoch_ms: grant.expires_epoch_ms,
    })
}

pub fn resolve_session_uuid(
    caller: &(impl ServiceCaller + Sync),
    selector: Option<&SessionSelector>,
) -> Result<uuid::Uuid, String> {
    match selector {
        Some(SessionSelector::ById(id)) => Ok(*id),
        Some(SessionSelector::ByName(_)) => {
            let mut client = dispatch_client(caller);
            let result =
                bmux_plugin::block_on_typed_dispatch(api_sessions_state::client::get_session(
                    &mut client,
                    session_selector_to_api(selector.expect("selector present")),
                ))
                .map_err(|err| typed_service_error("sessions-state/get-session", err))?;
            result
                .map(|session| session.id)
                .map_err(|err| format!("session selector did not resolve: {err:?}"))
        }
        None => {
            let mut client = dispatch_client(caller);
            let result = bmux_plugin::block_on_typed_dispatch(
                api_clients_state::client::current_client(&mut client),
            )
            .map_err(|err| typed_service_error("clients-state/current-client", err))?;
            result
                .map_err(|err| format!("current client unavailable: {err:?}"))?
                .selected_session_id
                .ok_or_else(|| "current client has no selected session".to_string())
        }
    }
}

fn pane_list(
    caller: &(impl ServiceCaller + Sync),
    request: &PaneListRequest,
) -> Result<PaneListResponse, String> {
    let session_id = match request.session.as_ref() {
        Some(selector) => Some(resolve_session_uuid(caller, Some(selector))?),
        None => None,
    };
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(api_pane_runtime_state::client::list_panes(
        &mut client,
        session_id,
    ))
    .map_err(|err| typed_service_error("pane-runtime-state/list-panes", err))?;
    let panes = result
        .map_err(|err| format!("list-panes failed: {err:?}"))?
        .panes
        .into_iter()
        .enumerate()
        .map(|(index, pane)| PaneSummary {
            id: pane.id,
            index: u32::try_from(index).unwrap_or(0),
            name: pane.name,
            focused: pane.focused,
        })
        .collect();
    Ok(PaneListResponse { panes })
}

fn pane_target_uuid(selector: Option<&PaneSelector>) -> Option<uuid::Uuid> {
    selector.and_then(|selector| match selector {
        PaneSelector::ById(id) => Some(*id),
        PaneSelector::ByIndex(_) | PaneSelector::Active => None,
    })
}

const fn split_direction_name(direction: PaneSplitDirection) -> &'static str {
    match direction {
        PaneSplitDirection::Horizontal => "horizontal",
        PaneSplitDirection::Vertical => "vertical",
    }
}

fn pane_launch(
    caller: &(impl ServiceCaller + Sync),
    request: &PaneLaunchRequest,
) -> Result<PaneLaunchResponse, String> {
    let session_id = resolve_session_uuid(caller, request.session.as_ref())?;
    let target = pane_target_uuid(request.target.as_ref());
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::launch_pane(
            &mut client,
            session_id,
            target,
            split_direction_name(request.direction).to_string(),
            50,
            request.name.clone(),
            request.command.program.clone(),
            request.command.args.clone(),
            request.command.cwd.clone(),
        ))
        .map_err(|err| typed_service_error("pane-runtime-commands/launch-pane", err))?;
    let ack = result.map_err(|err| format!("launch-pane failed: {err:?}"))?;
    Ok(PaneLaunchResponse {
        id: ack.pane_id,
        session_id: ack.session_id,
    })
}

fn pane_close(
    caller: &(impl ServiceCaller + Sync),
    request: &PaneCloseRequest,
) -> Result<PaneCloseResponse, String> {
    let session_id = resolve_session_uuid(caller, request.session.as_ref())?;
    let target = pane_target_uuid(request.target.as_ref());
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::close_pane(&mut client, session_id, target),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/close-pane", err))?;
    let ack = result.map_err(|err| format!("close-pane failed: {err:?}"))?;
    Ok(PaneCloseResponse {
        id: ack.pane_id,
        session_id: ack.session_id,
        session_closed: false,
    })
}

#[derive(Debug, Clone)]
pub struct ClusterPaneNewArgs {
    pub host: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum PaneRetryRef {
    Active,
    Index(u32),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryFailurePolicy {
    Abort,
    Continue,
    Prompt,
}

#[derive(Debug, Clone)]
pub struct ClusterPaneRetryArgs {
    pub pane: PaneRetryRef,
    pub on_failure: RetryFailurePolicy,
    pub retries: u32,
}

#[derive(Debug, Clone)]
pub struct ClusterPaneMoveArgs {
    pub pane: PaneRetryRef,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPaneBinding {
    pub target: String,
    pub cluster: Option<String>,
    pub source: String,
    #[serde(default)]
    pub state: ClusterConnectionState,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    pub updated_at_unix_ms: u64,
}

pub fn execute_cluster_pane_new(
    caller: &impl ClusterRuntimeOps,
    args: ClusterPaneNewArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let host = args.host.as_str();
    run_health_probe(caller, host, HealthProbe::Test)
        .map_err(|error| format!("target '{host}' is not ready: {error}"))?;

    let pane_name = args.name.or_else(|| Some(format!("host:{host}")));
    let response = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: None,
            direction: PaneSplitDirection::Vertical,
            name: pane_name,
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    host.to_string(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), host.to_string())]),
            },
        })
        .map_err(|error| format!("failed to create cluster pane for '{host}': {error}"))?;

    let binding = ClusterPaneBinding {
        target: host.to_string(),
        cluster: None,
        source: "new".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: 0,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &response.id.to_string(), Some(&binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(response.id.to_string()),
            cluster: None,
            target: Some(host.to_string()),
            source: Some("new".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "pane launched for reconnecting host session".to_string(),
        },
    )?;
    let mut binding = binding;
    let _ = verify_launched_binding(
        caller,
        &response.id.to_string(),
        &mut binding,
        RetryFailurePolicy::Continue,
        0,
    )?;

    Ok(ClusterCommandPaneMutationResponse {
        target: host.to_string(),
        old_pane_id: None,
        old_name: None,
        new_pane_id: response.id.to_string(),
        session_id: response.session_id.to_string(),
    })
}

pub fn execute_cluster_pane_retry(
    caller: &impl ClusterRuntimeOps,
    args: &ClusterPaneRetryArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let list = caller
        .pane_list(&PaneListRequest { session: None })
        .map_err(|error| format!("failed listing panes: {error}"))?;

    let pane = resolve_retry_pane(&list.panes, &args.pane)?;
    let pane_id_text = pane.id.to_string();
    let binding = mark_retry_started(
        caller,
        &pane_id_text,
        resolve_cluster_binding_for_pane(caller, pane)?,
    )?;
    run_retry_probe_with_policy(caller, &pane_id_text, &binding, args)?;

    let launch = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
            direction: PaneSplitDirection::Vertical,
            name: pane.name.clone().or_else(|| {
                Some(format_pane_name(
                    binding.cluster.as_deref(),
                    &binding.target,
                ))
            }),
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    binding.target.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), binding.target.clone())]),
            },
        })
        .map_err(|error| format!("failed relaunching pane for '{}': {error}", binding.target))?;

    let new_binding = ClusterPaneBinding {
        target: binding.target.clone(),
        cluster: binding.cluster.clone(),
        source: "retry".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: binding.retry_count,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &launch.id.to_string(), Some(&new_binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(launch.id.to_string()),
            cluster: new_binding.cluster.clone(),
            target: Some(binding.target.clone()),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "retry launched replacement pane".to_string(),
        },
    )?;
    let mut new_binding = new_binding;
    let _ = verify_launched_binding(
        caller,
        &launch.id.to_string(),
        &mut new_binding,
        args.on_failure,
        args.retries,
    )?;

    caller
        .pane_close(&PaneCloseRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
        })
        .map_err(|error| format!("failed closing old pane {}: {error}", pane.id))?;
    set_cluster_pane_binding(caller, &pane.id.to_string(), None)?;

    Ok(ClusterCommandPaneMutationResponse {
        target: binding.target,
        old_pane_id: Some(pane.id.to_string()),
        old_name: pane.name.clone(),
        new_pane_id: launch.id.to_string(),
        session_id: launch.session_id.to_string(),
    })
}

pub fn mark_retry_started(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    mut binding: ClusterPaneBinding,
) -> Result<ClusterPaneBinding, String> {
    binding.source = "retry".to_string();
    binding.state = ClusterConnectionState::Retrying;
    binding.retry_count = binding.retry_count.saturating_add(1);
    binding.last_error = None;
    binding.updated_at_unix_ms = now_unix_ms();
    set_cluster_pane_binding(caller, pane_id, Some(&binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(pane_id.to_string()),
            cluster: binding.cluster.clone(),
            target: Some(binding.target.clone()),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Retrying,
            message: "retry started".to_string(),
        },
    )?;
    Ok(binding)
}

pub fn mark_retry_probe_failed(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &ClusterPaneBinding,
    error: &str,
) -> String {
    let failed = ClusterPaneBinding {
        target: binding.target.clone(),
        cluster: binding.cluster.clone(),
        source: "retry".to_string(),
        state: ClusterConnectionState::Degraded,
        retry_count: binding.retry_count,
        last_error: Some(error.to_string()),
        updated_at_unix_ms: now_unix_ms(),
    };
    let _ = set_cluster_pane_binding(caller, pane_id, Some(&failed));
    let _ = append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(pane_id.to_string()),
            cluster: failed.cluster,
            target: Some(failed.target),
            source: Some("retry".to_string()),
            state: ClusterConnectionState::Degraded,
            message: format!("retry health probe failed: {error}"),
        },
    );
    format!("target '{}' is not ready: {error}", binding.target)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPromptDecision {
    Retry,
    Continue,
    Abort,
}

pub fn run_retry_probe_with_policy(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &ClusterPaneBinding,
    args: &ClusterPaneRetryArgs,
) -> Result<(), String> {
    let mut remaining_retries = args.retries;
    loop {
        match run_health_probe(caller, &binding.target, HealthProbe::Test) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let message = mark_retry_probe_failed(caller, pane_id, binding, &error);
                if remaining_retries > 0 {
                    remaining_retries -= 1;
                    let _ = append_cluster_connection_event(
                        caller,
                        ClusterConnectionEvent {
                            ts_unix_ms: now_unix_ms(),
                            pane_id: Some(pane_id.to_string()),
                            cluster: binding.cluster.clone(),
                            target: Some(binding.target.clone()),
                            source: Some("retry".to_string()),
                            state: ClusterConnectionState::Retrying,
                            message: format!(
                                "retrying health probe (remaining retries: {remaining_retries})"
                            ),
                        },
                    );
                    continue;
                }

                match args.on_failure {
                    RetryFailurePolicy::Abort => return Err(message),
                    RetryFailurePolicy::Continue => {
                        let _ = append_cluster_connection_event(
                            caller,
                            ClusterConnectionEvent {
                                ts_unix_ms: now_unix_ms(),
                                pane_id: Some(pane_id.to_string()),
                                cluster: binding.cluster.clone(),
                                target: Some(binding.target.clone()),
                                source: Some("retry".to_string()),
                                state: ClusterConnectionState::Degraded,
                                message: "continuing launch despite failed health probe"
                                    .to_string(),
                            },
                        );
                        return Ok(());
                    }
                    RetryFailurePolicy::Prompt => {
                        match prompt_retry_decision(&binding.target, &error)
                            .unwrap_or(RetryPromptDecision::Abort)
                        {
                            RetryPromptDecision::Retry => {}
                            RetryPromptDecision::Continue => return Ok(()),
                            RetryPromptDecision::Abort => return Err(message),
                        }
                    }
                }
            }
        }
    }
}

pub fn verify_launched_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: &mut ClusterPaneBinding,
    on_failure: RetryFailurePolicy,
    retries: u32,
) -> Result<Option<String>, String> {
    let mut retries_remaining = retries;
    loop {
        match run_health_probe(caller, &binding.target, HealthProbe::Test) {
            Ok(()) => {
                binding.state = ClusterConnectionState::Ready;
                binding.last_error = None;
                binding.updated_at_unix_ms = now_unix_ms();
                set_cluster_pane_binding(caller, pane_id, Some(binding))?;
                append_cluster_connection_event(
                    caller,
                    ClusterConnectionEvent {
                        ts_unix_ms: now_unix_ms(),
                        pane_id: Some(pane_id.to_string()),
                        cluster: binding.cluster.clone(),
                        target: Some(binding.target.clone()),
                        source: Some(binding.source.clone()),
                        state: ClusterConnectionState::Ready,
                        message: "post-launch health probe passed".to_string(),
                    },
                )?;
                return Ok(None);
            }
            Err(error) => {
                binding.state = ClusterConnectionState::Degraded;
                binding.last_error = Some(error.clone());
                binding.updated_at_unix_ms = now_unix_ms();
                set_cluster_pane_binding(caller, pane_id, Some(binding))?;
                let reason = format!("post-launch health probe failed: {error}");
                append_cluster_connection_event(
                    caller,
                    ClusterConnectionEvent {
                        ts_unix_ms: now_unix_ms(),
                        pane_id: Some(pane_id.to_string()),
                        cluster: binding.cluster.clone(),
                        target: Some(binding.target.clone()),
                        source: Some(binding.source.clone()),
                        state: ClusterConnectionState::Degraded,
                        message: reason.clone(),
                    },
                )?;
                if retries_remaining > 0 {
                    retries_remaining -= 1;
                    append_cluster_connection_event(
                        caller,
                        ClusterConnectionEvent {
                            ts_unix_ms: now_unix_ms(),
                            pane_id: Some(pane_id.to_string()),
                            cluster: binding.cluster.clone(),
                            target: Some(binding.target.clone()),
                            source: Some(binding.source.clone()),
                            state: ClusterConnectionState::Retrying,
                            message: format!(
                                "retrying post-launch health probe (remaining retries: {retries_remaining})"
                            ),
                        },
                    )?;
                    continue;
                }

                return match decide_failure_policy_action(on_failure, &binding.target, &reason) {
                    RetryPromptDecision::Retry => {
                        retries_remaining = 0;
                        continue;
                    }
                    RetryPromptDecision::Continue => Ok(Some(reason)),
                    RetryPromptDecision::Abort => Err(reason),
                };
            }
        }
    }
}

pub fn decide_failure_policy_action(
    policy: RetryFailurePolicy,
    target: &str,
    error: &str,
) -> RetryPromptDecision {
    match policy {
        RetryFailurePolicy::Abort => RetryPromptDecision::Abort,
        RetryFailurePolicy::Continue => RetryPromptDecision::Continue,
        RetryFailurePolicy::Prompt => {
            prompt_retry_decision(target, error).unwrap_or(RetryPromptDecision::Abort)
        }
    }
}

pub fn prompt_retry_decision(target: &str, error: &str) -> Option<RetryPromptDecision> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let request = prompt::PromptRequest::single_select(
        format!("Retry host '{target}'?"),
        vec![
            prompt::PromptOption::new("retry", "Retry health probe"),
            prompt::PromptOption::new("continue", "Continue launch anyway"),
            prompt::PromptOption::new("abort", "Abort"),
        ],
    )
    .message(format!("{error}\nChoose retry behavior."))
    .submit_label("Apply")
    .cancel_label("Abort");

    let response =
        tokio::task::block_in_place(|| handle.block_on(prompt::request(request))).ok()?;
    match response {
        prompt::PromptResponse::Submitted(prompt::PromptValue::Single(choice)) => {
            match choice.as_str() {
                "retry" => Some(RetryPromptDecision::Retry),
                "continue" => Some(RetryPromptDecision::Continue),
                _ => Some(RetryPromptDecision::Abort),
            }
        }
        prompt::PromptResponse::Submitted(_) => Some(RetryPromptDecision::Abort),
        prompt::PromptResponse::Cancelled | prompt::PromptResponse::RejectedBusy => {
            Some(RetryPromptDecision::Abort)
        }
    }
}

pub fn execute_cluster_pane_move(
    caller: &impl ClusterRuntimeOps,
    args: ClusterPaneMoveArgs,
) -> Result<ClusterCommandPaneMutationResponse, String> {
    let list = caller
        .pane_list(&PaneListRequest { session: None })
        .map_err(|error| format!("failed listing panes: {error}"))?;

    let pane = resolve_retry_pane(&list.panes, &args.pane)?;
    let previous_binding = resolve_cluster_binding_for_pane(caller, pane)?;
    run_health_probe(caller, &args.host, HealthProbe::Test)
        .map_err(|error| format!("target '{}' is not ready: {error}", args.host))?;

    let pane_name = retarget_pane_name_with_cluster(
        pane.name.as_deref(),
        previous_binding.cluster.as_deref(),
        &args.host,
    );
    let launch = caller
        .pane_launch(&PaneLaunchRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
            direction: PaneSplitDirection::Vertical,
            name: pane_name,
            command: PaneLaunchCommand {
                program: "bmux".to_string(),
                args: vec![
                    "connect".to_string(),
                    args.host.clone(),
                    "--reconnect-forever".to_string(),
                ],
                cwd: None,
                env: BTreeMap::from([("BMUX_CLUSTER_TARGET".to_string(), args.host.clone())]),
            },
        })
        .map_err(|error| format!("failed moving pane to '{}': {error}", args.host))?;

    let new_binding = ClusterPaneBinding {
        target: args.host.clone(),
        cluster: previous_binding.cluster,
        source: "move".to_string(),
        state: ClusterConnectionState::Connecting,
        retry_count: previous_binding.retry_count,
        last_error: None,
        updated_at_unix_ms: now_unix_ms(),
    };
    set_cluster_pane_binding(caller, &launch.id.to_string(), Some(&new_binding))?;
    append_cluster_connection_event(
        caller,
        ClusterConnectionEvent {
            ts_unix_ms: now_unix_ms(),
            pane_id: Some(launch.id.to_string()),
            cluster: new_binding.cluster.clone(),
            target: Some(args.host.clone()),
            source: Some("move".to_string()),
            state: ClusterConnectionState::Connecting,
            message: "move launched replacement pane".to_string(),
        },
    )?;
    let mut new_binding = new_binding;
    let _ = verify_launched_binding(
        caller,
        &launch.id.to_string(),
        &mut new_binding,
        RetryFailurePolicy::Continue,
        0,
    )?;

    caller
        .pane_close(&PaneCloseRequest {
            session: None,
            target: Some(PaneSelector::ById(pane.id)),
        })
        .map_err(|error| format!("failed closing old pane {}: {error}", pane.id))?;
    set_cluster_pane_binding(caller, &pane.id.to_string(), None)?;

    Ok(ClusterCommandPaneMutationResponse {
        target: args.host,
        old_pane_id: Some(pane.id.to_string()),
        old_name: pane.name.clone(),
        new_pane_id: launch.id.to_string(),
        session_id: launch.session_id.to_string(),
    })
}

pub fn parse_pane_retry_ref(raw: String) -> PaneRetryRef {
    if raw.eq_ignore_ascii_case("active") {
        PaneRetryRef::Active
    } else if let Ok(index) = raw.parse::<u32>() {
        PaneRetryRef::Index(index)
    } else {
        PaneRetryRef::Name(raw)
    }
}

pub fn resolve_retry_pane<'a>(
    panes: &'a [PaneSummary],
    pane_ref: &PaneRetryRef,
) -> Result<&'a PaneSummary, String> {
    match pane_ref {
        PaneRetryRef::Active => panes
            .iter()
            .find(|pane| pane.focused)
            .ok_or_else(|| "no active pane found".to_string()),
        PaneRetryRef::Index(index) => panes
            .iter()
            .find(|pane| pane.index == *index)
            .ok_or_else(|| format!("pane index '{index}' not found")),
        PaneRetryRef::Name(name) => panes
            .iter()
            .find(|pane| pane.name.as_deref() == Some(name.as_str()))
            .ok_or_else(|| format!("pane name '{name}' not found")),
    }
}

#[cfg(test)]
pub fn parse_cluster_target_from_pane_name(name: Option<&str>) -> Option<String> {
    let value = name?.trim();
    if value.is_empty() {
        return None;
    }
    let (_prefix, target) = value.split_once(':')?;
    let target = target.trim();
    if target.is_empty() {
        None
    } else {
        Some(target.to_string())
    }
}

pub fn parse_cluster_and_target_from_pane_name(
    name: Option<&str>,
) -> Option<(Option<String>, String)> {
    let value = name?.trim();
    if value.is_empty() {
        return None;
    }
    let (prefix, target) = value.split_once(':')?;
    let prefix = prefix.trim();
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let cluster = if prefix.eq_ignore_ascii_case("host") || prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    };
    Some((cluster, target.to_string()))
}

pub fn format_pane_name(cluster: Option<&str>, target: &str) -> String {
    if let Some(cluster) = cluster
        && !cluster.trim().is_empty()
    {
        return format!("{}:{target}", cluster.trim());
    }
    format!("host:{target}")
}

pub fn retarget_pane_name_with_cluster(
    name: Option<&str>,
    cluster: Option<&str>,
    target: &str,
) -> Option<String> {
    if let Some(cluster) = cluster
        && !cluster.trim().is_empty()
    {
        return Some(format_pane_name(Some(cluster), target));
    }
    retarget_pane_name(name, target)
}

pub fn resolve_cluster_binding_for_pane(
    caller: &impl ClusterRuntimeOps,
    pane: &PaneSummary,
) -> Result<ClusterPaneBinding, String> {
    let pane_id = pane.id.to_string();
    match get_cluster_pane_binding(caller, &pane_id) {
        Ok(Some(binding)) if !binding.target.trim().is_empty() => return Ok(binding),
        Ok(_) | Err(_) => {}
    }
    let (cluster, target) = parse_cluster_and_target_from_pane_name(pane.name.as_deref())
        .ok_or_else(|| {
            format!(
                "cannot infer cluster target from pane name {:?}; expected '<cluster>:<target>' or 'host:<target>'",
                pane.name
            )
        })?;
    Ok(ClusterPaneBinding {
        target,
        cluster,
        source: "name-fallback".to_string(),
        state: ClusterConnectionState::Degraded,
        retry_count: 0,
        last_error: Some("metadata missing; inferred from pane name".to_string()),
        updated_at_unix_ms: now_unix_ms(),
    })
}

pub fn retarget_pane_name(name: Option<&str>, target: &str) -> Option<String> {
    let current = name?.trim();
    if current.is_empty() {
        return Some(format!("host:{target}"));
    }
    if let Some((prefix, _)) = current.split_once(':') {
        return Some(format!("{}:{target}", prefix.trim()));
    }
    Some(format!("host:{target}"))
}
