#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostRuntimeApi, HostScope, NativeCommandContext, NativeDescriptor,
    NativeServiceContext, PaneListRequest, PluginCommand, PluginCommandArgument,
    PluginCommandArgumentKind, PluginService, RustPlugin, ServiceKind, ServiceResponse,
    SessionCreateRequest, SessionKillRequest, SessionSelector, decode_service_message,
    encode_service_message,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Default)]
struct WindowsPlugin;

impl RustPlugin for WindowsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.windows", "bmux Windows")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux windows command plugin")
            .require_capability("bmux.commands")
            .expect("capability should parse")
            .provide_capability("bmux.windows.read")
            .expect("capability should parse")
            .provide_capability("bmux.windows.write")
            .expect("capability should parse")
            .provide_feature("bmux.windows")
            .expect("feature should parse")
            .service(PluginService {
                capability: HostScope::new("bmux.windows.read").expect("host scope should parse"),
                kind: ServiceKind::Query,
                interface_id: "window-query/v1".to_string(),
            })
            .service(PluginService {
                capability: HostScope::new("bmux.windows.write").expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "window-command/v1".to_string(),
            })
            .command(plugin_command(
                "new-window",
                "Create a workspace window",
                vec![vec!["window", "new"]],
            ))
            .command(plugin_command(
                "list-windows",
                "List workspace windows",
                vec![vec!["window", "list"]],
            ))
            .command(plugin_command(
                "kill-window",
                "Kill a workspace window",
                vec![vec!["window", "kill"]],
            ))
            .command(plugin_command(
                "kill-all-windows",
                "Kill all workspace windows",
                vec![vec!["window", "kill-all"]],
            ))
            .command(plugin_command(
                "switch-window",
                "Switch active workspace window",
                vec![vec!["window", "switch"]],
            ))
            .build()
            .expect("descriptor should validate")
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match handle_command(&context) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        }
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("window-query/v1", "list") => {
                let request =
                    match decode_service_message::<ListWindowsRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let windows = match list_windows(&context, request.session.as_deref()) {
                    Ok(windows) => windows,
                    Err(error) => {
                        return ServiceResponse::error("list_failed", error.to_string());
                    }
                };
                let payload = match encode_service_message(&ListWindowsResponse { windows }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "new") => {
                let request =
                    match decode_service_message::<NewWindowRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let ack = match create_window(&context, request.name) {
                    Ok(ack) => ack,
                    Err(error) => return ServiceResponse::error("new_failed", error),
                };
                let payload = match encode_service_message(&ack) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "kill") => {
                let request =
                    match decode_service_message::<KillWindowRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let selector = match parse_selector(&request.target) {
                    Ok(selector) => selector,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let ack = match kill_window(&context, selector, request.force_local) {
                    Ok(ack) => ack,
                    Err(error) => return ServiceResponse::error("kill_failed", error),
                };
                let payload = match encode_service_message(&ack) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "kill_all") => {
                let request =
                    match decode_service_message::<KillAllWindowsRequest>(&context.request.payload)
                    {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let ack = match kill_all_windows(&context, request.force_local) {
                    Ok(ack) => ack,
                    Err(error) => return ServiceResponse::error("kill_failed", error),
                };
                let payload = match encode_service_message(&ack) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "switch") => {
                let request =
                    match decode_service_message::<SwitchWindowRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let selector = match parse_selector(&request.target) {
                    Ok(selector) => selector,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let ack = match switch_window(&context, selector) {
                    Ok(ack) => ack,
                    Err(error) => return ServiceResponse::error("switch_failed", error),
                };
                let payload = match encode_service_message(&ack) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            _ => ServiceResponse::error(
                "unsupported_service_operation",
                format!(
                    "unsupported windows service invocation '{}:{}'",
                    context.request.service.interface_id, context.request.operation,
                ),
            ),
        }
    }
}

fn handle_command(context: &NativeCommandContext) -> Result<(), String> {
    match context.command.as_str() {
        "new-window" => {
            let name = option_value(&context.arguments, "name");
            let response = context
                .session_create(&SessionCreateRequest { name })
                .map_err(|error| error.to_string())?;
            println!("created window session: {}", response.id);
            Ok(())
        }
        "list-windows" => {
            let session_filter = option_value(&context.arguments, "session");
            let as_json = has_flag(&context.arguments, "json");
            let windows = list_windows(context, session_filter.as_deref())?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListWindowsResponse { windows })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if windows.is_empty() {
                println!("no windows");
            } else {
                for window in windows {
                    println!(
                        "{}\t{}\t{}",
                        window.id,
                        window.name,
                        if window.active { "active" } else { "inactive" }
                    );
                }
            }
            Ok(())
        }
        "kill-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let force_local = has_flag(&context.arguments, "force-local");
            let response = context
                .session_kill(&SessionKillRequest {
                    selector,
                    force_local,
                })
                .map_err(|error| error.to_string())?;
            println!("killed window session: {}", response.id);
            Ok(())
        }
        "kill-all-windows" => {
            let force_local = has_flag(&context.arguments, "force-local");
            let sessions = context
                .session_list()
                .map_err(|error| error.to_string())?
                .sessions;
            if sessions.is_empty() {
                println!("no windows");
                return Ok(());
            }
            for session in sessions {
                let response = context
                    .session_kill(&SessionKillRequest {
                        selector: SessionSelector::ById(session.id),
                        force_local,
                    })
                    .map_err(|error| error.to_string())?;
                println!("killed window session: {}", response.id);
            }
            Ok(())
        }
        "switch-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let session_id = resolve_session_id(context, selector)?;
            let pane_response = context
                .pane_list(&PaneListRequest {
                    session: Some(SessionSelector::ById(session_id)),
                })
                .map_err(|error| error.to_string())?;
            if pane_response.panes.is_empty() {
                return Err("target window has no panes".to_string());
            }
            println!(
                "switch-window selected session {} (attach command will enter it)",
                session_id
            );
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn list_windows(
    caller: &impl HostRuntimeApi,
    session_filter: Option<&str>,
) -> Result<Vec<WindowEntry>, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let selected = if let Some(filter) = session_filter {
        let selector = parse_selector(filter)?;
        sessions
            .into_iter()
            .filter(|session| match &selector {
                SessionSelector::ById(id) => &session.id == id,
                SessionSelector::ByName(name) => session.name.as_deref() == Some(name.as_str()),
            })
            .collect::<Vec<_>>()
    } else {
        sessions
    };

    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(index, session)| WindowEntry {
            id: session.id.to_string(),
            name: session
                .name
                .unwrap_or_else(|| format!("session-{}", index.saturating_add(1))),
            active: index == 0,
        })
        .collect())
}

fn create_window(
    caller: &impl HostRuntimeApi,
    name: Option<String>,
) -> Result<WindowCommandAck, String> {
    let response = caller
        .session_create(&SessionCreateRequest { name })
        .map_err(|error| error.to_string())?;
    Ok(WindowCommandAck {
        ok: true,
        id: Some(response.id.to_string()),
    })
}

fn kill_window(
    caller: &impl HostRuntimeApi,
    selector: SessionSelector,
    force_local: bool,
) -> Result<WindowCommandAck, String> {
    let response = caller
        .session_kill(&SessionKillRequest {
            selector,
            force_local,
        })
        .map_err(|error| error.to_string())?;
    Ok(WindowCommandAck {
        ok: true,
        id: Some(response.id.to_string()),
    })
}

fn kill_all_windows(
    caller: &impl HostRuntimeApi,
    force_local: bool,
) -> Result<WindowCommandAck, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    for session in sessions {
        caller
            .session_kill(&SessionKillRequest {
                selector: SessionSelector::ById(session.id),
                force_local,
            })
            .map_err(|error| error.to_string())?;
    }
    Ok(WindowCommandAck { ok: true, id: None })
}

fn switch_window(
    caller: &impl HostRuntimeApi,
    selector: SessionSelector,
) -> Result<WindowCommandAck, String> {
    let session_id = resolve_session_id(caller, selector)?;
    let pane_response = caller
        .pane_list(&PaneListRequest {
            session: Some(SessionSelector::ById(session_id)),
        })
        .map_err(|error| error.to_string())?;
    if pane_response.panes.is_empty() {
        return Err("target window has no panes".to_string());
    }
    Ok(WindowCommandAck {
        ok: true,
        id: Some(session_id.to_string()),
    })
}

fn resolve_session_id(
    caller: &impl HostRuntimeApi,
    selector: SessionSelector,
) -> Result<Uuid, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let session = sessions.into_iter().find(|session| match &selector {
        SessionSelector::ById(id) => session.id == *id,
        SessionSelector::ByName(name) => session.name.as_deref() == Some(name.as_str()),
    });
    session
        .map(|session| session.id)
        .ok_or_else(|| "target session not found".to_string())
}

fn parse_selector(value: &str) -> Result<SessionSelector, String> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(SessionSelector::ById(id));
    }
    if value.trim().is_empty() {
        return Err("target must not be empty".to_string());
    }
    Ok(SessionSelector::ByName(value.to_string()))
}

fn option_value(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    arguments
        .windows(2)
        .find_map(|chunk| (chunk[0] == long_flag).then(|| chunk[1].clone()))
}

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

fn positional_value(arguments: &[String]) -> Option<String> {
    arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .cloned()
}

fn plugin_command(name: &str, summary: &str, aliases: Vec<Vec<&str>>) -> PluginCommand {
    let mut command = PluginCommand::new(name, summary)
        .path([name])
        .execution(CommandExecutionKind::ProviderExec)
        .expose_in_cli(true);
    for alias in aliases {
        command = command.alias(alias);
    }
    for argument in command_arguments(name) {
        command = command.argument(argument);
    }
    command
}

fn command_arguments(name: &str) -> Vec<PluginCommandArgument> {
    match name {
        "new-window" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::option("name", PluginCommandArgumentKind::String).short('n'),
        ],
        "list-windows" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("json").short('j'),
        ],
        "kill-window" => vec![
            PluginCommandArgument::positional("target", PluginCommandArgumentKind::String)
                .required(true),
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("force-local"),
        ],
        "kill-all-windows" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("force-local"),
        ],
        "switch-window" => vec![
            PluginCommandArgument::positional("target", PluginCommandArgumentKind::String)
                .required(true),
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
        ],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsRequest {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillWindowRequest {
    target: String,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllWindowsRequest {
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchWindowRequest {
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowEntry {
    id: String,
    name: String,
    active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsResponse {
    windows: Vec<WindowEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowCommandAck {
    ok: bool,
    #[serde(default)]
    id: Option<String>,
}

bmux_plugin::export_plugin!(WindowsPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{
        PaneListResponse, PaneSummary, ServiceCaller, SessionListResponse, SessionSummary,
    };
    use std::sync::Mutex;

    struct MockHost {
        sessions: Vec<SessionSummary>,
        pane_count_by_session: std::collections::BTreeMap<Uuid, usize>,
        fail_create: bool,
        fail_kill: bool,
        fail_pane_list: bool,
        creates: Mutex<Vec<Option<String>>>,
        kills: Mutex<Vec<SessionKillRequest>>,
    }

    impl MockHost {
        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            let pane_count_by_session = sessions
                .iter()
                .map(|session| (session.id, 1usize))
                .collect::<std::collections::BTreeMap<_, _>>();
            Self {
                sessions,
                pane_count_by_session,
                fail_create: false,
                fail_kill: false,
                fail_pane_list: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
            }
        }

        fn with_empty_target_session(target_id: Uuid) -> Self {
            let sessions = vec![SessionSummary {
                id: target_id,
                name: Some("target".to_string()),
                client_count: 1,
            }];
            let mut pane_count_by_session = std::collections::BTreeMap::new();
            pane_count_by_session.insert(target_id, 0);
            Self {
                sessions,
                pane_count_by_session,
                fail_create: false,
                fail_kill: false,
                fail_pane_list: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
            }
        }

        fn with_failures(fail_create: bool, fail_kill: bool, fail_pane_list: bool) -> Self {
            let sessions = sample_sessions();
            let pane_count_by_session = sessions
                .iter()
                .map(|session| (session.id, 1usize))
                .collect::<std::collections::BTreeMap<_, _>>();
            Self {
                sessions,
                pane_count_by_session,
                fail_create,
                fail_kill,
                fail_pane_list,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
            }
        }
    }

    impl ServiceCaller for MockHost {
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin::Result<Vec<u8>> {
            match (interface_id, operation) {
                ("session-query/v1", "list") => encode_service_message(&SessionListResponse {
                    sessions: self.sessions.clone(),
                })
                .map_err(Into::into),
                ("session-command/v1", "new") => {
                    if self.fail_create {
                        return Err(bmux_plugin::PluginError::ServiceProtocol {
                            details: "mock create failure".to_string(),
                        });
                    }
                    let request: SessionCreateRequest = decode_service_message(&payload)?;
                    self.creates
                        .lock()
                        .expect("create log lock should succeed")
                        .push(request.name.clone());
                    encode_service_message(&bmux_plugin::SessionCreateResponse {
                        id: Uuid::new_v4(),
                        name: request.name,
                    })
                    .map_err(Into::into)
                }
                ("session-command/v1", "kill") => {
                    if self.fail_kill {
                        return Err(bmux_plugin::PluginError::ServiceProtocol {
                            details: "mock kill failure".to_string(),
                        });
                    }
                    let request: SessionKillRequest = decode_service_message(&payload)?;
                    self.kills
                        .lock()
                        .expect("kill log lock should succeed")
                        .push(request.clone());
                    encode_service_message(&bmux_plugin::SessionKillResponse {
                        id: match request.selector {
                            SessionSelector::ById(id) => id,
                            SessionSelector::ByName(_) => Uuid::new_v4(),
                        },
                    })
                    .map_err(Into::into)
                }
                ("pane-query/v1", "list") => {
                    if self.fail_pane_list {
                        return Err(bmux_plugin::PluginError::ServiceProtocol {
                            details: "mock pane list failure".to_string(),
                        });
                    }
                    let request: PaneListRequest = decode_service_message(&payload)?;
                    let pane_count = request
                        .session
                        .and_then(|selector| match selector {
                            SessionSelector::ById(id) => {
                                self.pane_count_by_session.get(&id).copied()
                            }
                            SessionSelector::ByName(name) => self
                                .sessions
                                .iter()
                                .find(|session| session.name.as_deref() == Some(name.as_str()))
                                .and_then(|session| {
                                    self.pane_count_by_session.get(&session.id).copied()
                                }),
                        })
                        .unwrap_or(0);
                    let panes = (0..pane_count)
                        .map(|index| PaneSummary {
                            id: Uuid::new_v4(),
                            index: (index + 1) as u32,
                            name: Some(format!("pane-{}", index + 1)),
                            focused: index == 0,
                        })
                        .collect::<Vec<_>>();
                    encode_service_message(&PaneListResponse { panes }).map_err(Into::into)
                }
                _ => Err(bmux_plugin::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        }
    }

    fn sample_sessions() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                client_count: 1,
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                client_count: 2,
            },
        ]
    }

    #[test]
    fn list_windows_projects_sessions_and_marks_first_active() {
        let host = MockHost::with_sessions(sample_sessions());
        let windows = list_windows(&host, None).expect("list should succeed");

        assert_eq!(windows.len(), 2);
        assert!(windows[0].active);
        assert!(!windows[1].active);
        assert_eq!(windows[0].name, "alpha");
        assert_eq!(windows[1].name, "beta");
    }

    #[test]
    fn list_windows_filters_by_session_selector() {
        let sessions = sample_sessions();
        let beta_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);

        let by_name = list_windows(&host, Some("beta")).expect("list by name should succeed");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "beta");

        let by_id =
            list_windows(&host, Some(&beta_id.to_string())).expect("list by id should succeed");
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].id, beta_id.to_string());
    }

    #[test]
    fn resolve_session_id_finds_name_and_id() {
        let sessions = sample_sessions();
        let alpha_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);

        let resolved_name = resolve_session_id(&host, SessionSelector::ByName("alpha".to_string()))
            .expect("resolve by name should succeed");
        assert_eq!(resolved_name, alpha_id);

        let resolved_id = resolve_session_id(&host, SessionSelector::ById(alpha_id))
            .expect("resolve by id should succeed");
        assert_eq!(resolved_id, alpha_id);
    }

    #[test]
    fn parse_selector_rejects_blank_values() {
        let error = parse_selector("   ").expect_err("blank selector should fail");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn create_window_calls_session_create() {
        let host = MockHost::with_sessions(sample_sessions());
        let ack = create_window(&host, Some("dev".to_string())).expect("create should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some());
        let creates = host.creates.lock().expect("create log lock should succeed");
        assert_eq!(creates.as_slice(), &[Some("dev".to_string())]);
    }

    #[test]
    fn kill_all_windows_calls_kill_for_each_session() {
        let host = MockHost::with_sessions(sample_sessions());
        let ack = kill_all_windows(&host, true).expect("kill all should succeed");
        assert!(ack.ok);
        let kills = host.kills.lock().expect("kill log lock should succeed");
        assert_eq!(kills.len(), 2);
        assert!(kills.iter().all(|request| request.force_local));
    }

    #[test]
    fn kill_window_passes_selector_and_force_local() {
        let host = MockHost::with_sessions(sample_sessions());
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;

        let ack =
            kill_window(&host, SessionSelector::ById(target), true).expect("kill should succeed");
        assert!(ack.ok);
        let target_text = target.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let kills = host.kills.lock().expect("kill log lock should succeed");
        assert_eq!(kills.len(), 1);
        assert!(matches!(kills[0].selector, SessionSelector::ById(id) if id == target));
        assert!(kills[0].force_local);
    }

    #[test]
    fn switch_window_requires_target_session_to_have_panes() {
        let target_id = Uuid::new_v4();
        let host = MockHost::with_empty_target_session(target_id);
        let error = switch_window(&host, SessionSelector::ById(target_id))
            .expect_err("switch should fail when target has no panes");
        assert!(error.contains("no panes"));
    }

    #[test]
    fn switch_window_returns_selected_session_id() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);

        let ack =
            switch_window(&host, SessionSelector::ById(target_id)).expect("switch should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn create_window_propagates_host_error() {
        let host = MockHost::with_failures(true, false, false);
        let error = create_window(&host, Some("dev".to_string()))
            .expect_err("create should surface host failure");
        assert!(error.contains("mock create failure"));
    }

    #[test]
    fn kill_window_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_window(&host, SessionSelector::ByName("alpha".to_string()), false)
            .expect_err("kill should surface host failure");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn kill_all_windows_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_all_windows(&host, true).expect_err("kill all should fail on host error");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn switch_window_propagates_pane_list_error() {
        let host = MockHost::with_failures(false, false, true);
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;
        let error = switch_window(&host, SessionSelector::ById(target))
            .expect_err("switch should fail when pane list fails");
        assert!(error.contains("mock pane list failure"));
    }
}
