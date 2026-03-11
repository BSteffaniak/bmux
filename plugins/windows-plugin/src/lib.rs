#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_cli_output::{Table, TableColumn, write_table};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{SessionSelector, WindowSelector, WindowSummary};
use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, NativeServiceContext,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginService, RustPlugin,
    ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
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
            .require_capability("bmux.sessions.read")
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
                "Create a new window in a session",
                vec![vec!["window".to_string(), "new".to_string()]],
            ))
            .command(plugin_command(
                "list-windows",
                "List windows for a session",
                vec![vec!["window".to_string(), "list".to_string()]],
            ))
            .command(plugin_command(
                "kill-window",
                "Kill a window by name, UUID, or active",
                vec![vec!["window".to_string(), "kill".to_string()]],
            ))
            .command(plugin_command(
                "kill-all-windows",
                "Kill all windows in a session",
                vec![vec!["window".to_string(), "kill-all".to_string()]],
            ))
            .command(plugin_command(
                "switch-window",
                "Switch active window by name, UUID, or active",
                vec![vec!["window".to_string(), "switch".to_string()]],
            ))
            .lifecycle(bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            })
            .build()
            .expect("descriptor should validate")
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match context.command.as_str() {
            "new-window" => run_new_window(&context),
            "list-windows" => run_list_windows(&context),
            "kill-window" => run_kill_window(&context),
            "kill-all-windows" => run_kill_all_windows(&context),
            "switch-window" => run_switch_window(&context),
            _ => 64,
        }
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("window-query/v1", "list") => run_window_query_service(&context),
            ("window-command/v1", "new")
            | ("window-command/v1", "switch")
            | ("window-command/v1", "kill")
            | ("window-command/v1", "kill_all") => run_window_command_service(&context),
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

bmux_plugin::export_plugin!(WindowsPlugin);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsRequest {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsResponse {
    windows: Vec<WindowSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    session: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowResponse {
    window: WindowSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchWindowRequest {
    session: Option<String>,
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchWindowResponse {
    window: WindowSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillWindowRequest {
    session: Option<String>,
    target: String,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillWindowResponse {
    window_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllWindowsRequest {
    session: Option<String>,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllWindowsResponse {
    killed_count: usize,
    failed_count: usize,
}

fn plugin_command(name: &str, summary: &str, aliases: Vec<Vec<String>>) -> PluginCommand {
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

#[derive(Default)]
struct WindowArgs {
    session: Option<String>,
    name: Option<String>,
    target: Option<String>,
    force_local: bool,
    json: bool,
}

fn parse_window_args(arguments: &[String]) -> Result<WindowArgs, i32> {
    let mut parsed = WindowArgs::default();
    let mut iter = arguments.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => parsed.session = iter.next().cloned(),
            "--name" => parsed.name = iter.next().cloned(),
            "--target" => parsed.target = iter.next().cloned(),
            "--force-local" => parsed.force_local = true,
            "--json" => parsed.json = true,
            other if other.starts_with('-') => return Err(64),
            other => {
                if parsed.target.is_none() {
                    parsed.target = Some(other.to_string());
                } else {
                    return Err(64);
                }
            }
        }
    }
    Ok(parsed)
}

fn run_new_window(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };

    let response = match with_command_client(
        context,
        "bmux-windows-plugin-command",
        |mut client| async move {
            let selector = args.session.as_deref().map(parse_session_selector);
            let window_id = client.new_window(selector.clone(), args.name).await?;
            let window = resolve_window_summary_by_id(&mut client, selector, window_id)
                .await?
                .ok_or(bmux_client::ClientError::UnexpectedResponse(
                    "created window missing from list response",
                ))?;
            Ok::<NewWindowResponse, bmux_client::ClientError>(NewWindowResponse { window })
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("failed creating window: {error}");
            return 1;
        }
    };

    println!("created window: {}", window_summary_label(&response.window));
    0
}

fn run_list_windows(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };

    let windows = match with_command_client(
        context,
        "bmux-windows-plugin-command",
        |mut client| async move {
            client
                .list_windows(args.session.as_deref().map(parse_session_selector))
                .await
        },
    ) {
        Ok(windows) => windows,
        Err(error) => {
            eprintln!("failed listing windows: {error}");
            return 1;
        }
    };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&windows).expect("windows json should encode")
        );
        return 0;
    }

    if windows.is_empty() {
        println!("no windows");
        return 0;
    }

    let mut windows = windows;
    sort_windows(&mut windows);
    let mut table = Table::new(vec![
        TableColumn::new("ID").min_width(36),
        TableColumn::new("SESSION").min_width(16),
        TableColumn::new("WINDOW").min_width(12),
        TableColumn::new("ACTIVE"),
    ]);
    for window in windows {
        table.push_row(vec![
            window.id.to_string(),
            format!("session-{}", short_uuid(window.session_id)),
            window_summary_label(&window),
            if window.active {
                "yes".to_string()
            } else {
                "no".to_string()
            },
        ]);
    }
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = write_table(&mut stdout, &table) {
        eprintln!("failed rendering windows table: {error}");
        return 1;
    }
    0
}

fn run_kill_window(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let Some(target) = args.target else {
        eprintln!("kill-window requires <target>");
        return 64;
    };

    match with_command_client(
        context,
        "bmux-windows-plugin-command",
        |mut client| async move {
            let window_id = client
                .kill_window_with_options(
                    args.session.as_deref().map(parse_session_selector),
                    parse_window_selector(&target),
                    args.force_local,
                )
                .await?;
            Ok::<KillWindowResponse, bmux_client::ClientError>(KillWindowResponse { window_id })
        },
    ) {
        Ok(response) => {
            println!("killed window: {}", response.window_id);
            0
        }
        Err(error) => {
            eprintln!("failed killing window: {error}");
            1
        }
    }
}

fn run_kill_all_windows(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };

    match with_command_client(
        context,
        "bmux-windows-plugin-command",
        |mut client| async move {
            let selector = args.session.as_deref().map(parse_session_selector);
            let windows = client.list_windows(selector.clone()).await?;
            let mut killed_count = 0usize;
            let mut failed_count = 0usize;
            for window in windows {
                match client
                    .kill_window_with_options(
                        selector.clone(),
                        WindowSelector::ById(window.id),
                        args.force_local,
                    )
                    .await
                {
                    Ok(_) => killed_count = killed_count.saturating_add(1),
                    Err(_) => failed_count = failed_count.saturating_add(1),
                }
            }
            Ok::<KillAllWindowsResponse, bmux_client::ClientError>(KillAllWindowsResponse {
                killed_count,
                failed_count,
            })
        },
    ) {
        Ok(response) => {
            println!(
                "kill-all-windows complete: killed {}, failed {}",
                response.killed_count, response.failed_count
            );
            if response.failed_count == 0 { 0 } else { 1 }
        }
        Err(error) => {
            eprintln!("failed killing windows: {error}");
            1
        }
    }
}

fn run_switch_window(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let Some(target) = args.target else {
        eprintln!("switch-window requires <target>");
        return 64;
    };

    match with_command_client(
        context,
        "bmux-windows-plugin-command",
        |mut client| async move {
            let selector = args.session.as_deref().map(parse_session_selector);
            let window_id = client
                .switch_window(selector.clone(), parse_window_selector(&target))
                .await?;
            let window = resolve_window_summary_by_id(&mut client, selector, window_id)
                .await?
                .ok_or(bmux_client::ClientError::UnexpectedResponse(
                    "active window missing from list response",
                ))?;
            Ok::<SwitchWindowResponse, bmux_client::ClientError>(SwitchWindowResponse { window })
        },
    ) {
        Ok(response) => {
            println!("active window: {}", window_summary_label(&response.window));
            0
        }
        Err(error) => {
            eprintln!("failed switching window: {error}");
            1
        }
    }
}

fn run_window_query_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = match decode_service_message::<ListWindowsRequest>(&context.request.payload) {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };

    with_client(
        context,
        "bmux-windows-plugin-service",
        |mut client| async move {
            let windows = client
                .list_windows(request.session.as_deref().map(parse_session_selector))
                .await
                .map_err(client_error)?;
            Ok(encode_service_message(&ListWindowsResponse { windows })?)
        },
    )
}

fn run_window_command_service(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        "new" => {
            let request = match decode_service_message::<NewWindowRequest>(&context.request.payload)
            {
                Ok(request) => request,
                Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
            };

            with_client(
                context,
                "bmux-windows-plugin-service",
                |mut client| async move {
                    let window_id = client
                        .new_window(
                            request.session.as_deref().map(parse_session_selector),
                            request.name,
                        )
                        .await
                        .map_err(client_error)?;
                    let window = resolve_window_summary_by_id(
                        &mut client,
                        request.session.as_deref().map(parse_session_selector),
                        window_id,
                    )
                    .await
                    .map_err(client_error)?
                    .ok_or_else(|| {
                        bmux_plugin::PluginError::ServiceProtocol {
                            details: "created window missing from list response".to_string(),
                        }
                    })?;
                    Ok(encode_service_message(&NewWindowResponse { window })?)
                },
            )
        }
        "switch" => {
            let request =
                match decode_service_message::<SwitchWindowRequest>(&context.request.payload) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };

            with_client(
                context,
                "bmux-windows-plugin-service",
                |mut client| async move {
                    let window_id = client
                        .switch_window(
                            request.session.as_deref().map(parse_session_selector),
                            parse_window_selector(&request.target),
                        )
                        .await
                        .map_err(client_error)?;
                    let window = resolve_window_summary_by_id(
                        &mut client,
                        request.session.as_deref().map(parse_session_selector),
                        window_id,
                    )
                    .await
                    .map_err(client_error)?
                    .ok_or_else(|| {
                        bmux_plugin::PluginError::ServiceProtocol {
                            details: "active window missing from list response".to_string(),
                        }
                    })?;
                    Ok(encode_service_message(&SwitchWindowResponse { window })?)
                },
            )
        }
        "kill" => {
            let request =
                match decode_service_message::<KillWindowRequest>(&context.request.payload) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };

            with_client(
                context,
                "bmux-windows-plugin-service",
                |mut client| async move {
                    let window_id = client
                        .kill_window_with_options(
                            request.session.as_deref().map(parse_session_selector),
                            parse_window_selector(&request.target),
                            request.force_local,
                        )
                        .await
                        .map_err(client_error)?;
                    Ok(encode_service_message(&KillWindowResponse { window_id })?)
                },
            )
        }
        "kill_all" => {
            let request =
                match decode_service_message::<KillAllWindowsRequest>(&context.request.payload) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };

            with_client(
                context,
                "bmux-windows-plugin-service",
                |mut client| async move {
                    let selector = request.session.as_deref().map(parse_session_selector);
                    let windows = client
                        .list_windows(selector.clone())
                        .await
                        .map_err(client_error)?;
                    let mut killed_count = 0usize;
                    let mut failed_count = 0usize;
                    for window in windows {
                        match client
                            .kill_window_with_options(
                                selector.clone(),
                                WindowSelector::ById(window.id),
                                request.force_local,
                            )
                            .await
                        {
                            Ok(_) => killed_count = killed_count.saturating_add(1),
                            Err(_) => failed_count = failed_count.saturating_add(1),
                        }
                    }
                    Ok(encode_service_message(&KillAllWindowsResponse {
                        killed_count,
                        failed_count,
                    })?)
                },
            )
        }
        _ => ServiceResponse::error(
            "unsupported_service_operation",
            format!(
                "unsupported windows command operation '{}'",
                context.request.operation
            ),
        ),
    }
}

fn with_client<F, Fut>(
    context: &NativeServiceContext,
    principal: &str,
    operation: F,
) -> ServiceResponse
where
    F: FnOnce(BmuxClient) -> Fut,
    Fut: std::future::Future<Output = bmux_plugin::Result<Vec<u8>>>,
{
    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async move {
                match BmuxClient::connect_with_paths(&paths, principal).await {
                    Ok(client) => match operation(client).await {
                        Ok(payload) => ServiceResponse::ok(payload),
                        Err(error) => ServiceResponse::error("service_failed", error.to_string()),
                    },
                    Err(error) => ServiceResponse::error("connect_failed", error.to_string()),
                }
            })
        }),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(async move {
                match BmuxClient::connect_with_paths(&paths, principal).await {
                    Ok(client) => match operation(client).await {
                        Ok(payload) => ServiceResponse::ok(payload),
                        Err(error) => ServiceResponse::error("service_failed", error.to_string()),
                    },
                    Err(error) => ServiceResponse::error("connect_failed", error.to_string()),
                }
            }),
            Err(error) => ServiceResponse::error("runtime_error", error.to_string()),
        },
    }
}

fn with_command_client<T, Fut>(
    context: &NativeCommandContext,
    principal: &str,
    operation: impl FnOnce(BmuxClient) -> Fut,
) -> Result<T, String>
where
    Fut: std::future::Future<Output = std::result::Result<T, bmux_client::ClientError>>,
{
    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async {
                let client = BmuxClient::connect_with_paths(&paths, principal)
                    .await
                    .map_err(|error| error.to_string())?;
                operation(client).await.map_err(|error| error.to_string())
            })
        }),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                let client = BmuxClient::connect_with_paths(&paths, principal)
                    .await
                    .map_err(|error| error.to_string())?;
                operation(client).await.map_err(|error| error.to_string())
            })
        }
    }
}

fn parse_session_selector(value: &str) -> SessionSelector {
    match Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
    }
}

fn parse_window_selector(value: &str) -> WindowSelector {
    if value.eq_ignore_ascii_case("active") {
        return WindowSelector::Active;
    }
    if let Ok(number) = value.parse::<u32>()
        && number > 0
    {
        return WindowSelector::ByNumber(number);
    }
    match Uuid::parse_str(value) {
        Ok(id) => WindowSelector::ById(id),
        Err(_) => WindowSelector::ByName(value.to_string()),
    }
}

fn client_error(error: bmux_client::ClientError) -> bmux_plugin::PluginError {
    bmux_plugin::PluginError::ServiceProtocol {
        details: error.to_string(),
    }
}

async fn resolve_window_summary_by_id(
    client: &mut BmuxClient,
    session: Option<SessionSelector>,
    window_id: Uuid,
) -> Result<Option<WindowSummary>, bmux_client::ClientError> {
    Ok(client
        .list_windows(session)
        .await?
        .into_iter()
        .find(|window| window.id == window_id))
}

fn short_uuid(id: Uuid) -> String {
    id.as_simple().to_string()[..8].to_string()
}

fn window_summary_label(window: &WindowSummary) -> String {
    match &window.name {
        Some(name) => format!("{}:{name}", window.number),
        None => window.number.to_string(),
    }
}

fn sort_windows(windows: &mut [WindowSummary]) {
    windows.sort_by(|left, right| {
        left.number
            .cmp(&right.number)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use super::WindowsPlugin;
    use bmux_plugin::RustPlugin;

    #[test]
    fn descriptor_includes_windows_feature() {
        let descriptor = WindowsPlugin.descriptor();
        let serialized = descriptor
            .to_toml_string()
            .expect("descriptor should serialize");
        let reparsed = bmux_plugin::NativeDescriptor::from_toml_str(&serialized)
            .expect("descriptor should parse");
        assert_eq!(reparsed.id, "bmux.windows");
        assert_eq!(reparsed.commands.len(), 5);
    }
}
