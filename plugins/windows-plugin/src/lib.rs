#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{SessionSelector, WindowSelector, WindowSummary};
use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, PluginCommand,
    PluginCommandArgument, PluginCommandArgumentKind, PluginFeature, PluginService, RustPlugin,
    ServiceKind,
};
use std::collections::BTreeSet;

#[derive(Default)]
struct WindowsPlugin;

impl RustPlugin for WindowsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor {
            id: "bmux.windows".to_string(),
            display_name: "bmux Windows".to_string(),
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_api: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            native_abi: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            description: Some("Shipped bmux windows command plugin".to_string()),
            homepage: None,
            required_capabilities: BTreeSet::from([
                HostScope::new("bmux.commands").expect("host scope should parse"),
                HostScope::new("bmux.sessions.read").expect("host scope should parse"),
            ]),
            provided_capabilities: BTreeSet::from([
                HostScope::new("bmux.windows.read").expect("host scope should parse"),
                HostScope::new("bmux.windows.write").expect("host scope should parse"),
            ]),
            provided_features: BTreeSet::from([
                PluginFeature::new("bmux.windows").expect("plugin feature should parse")
            ]),
            services: vec![
                PluginService {
                    capability: HostScope::new("bmux.windows.read")
                        .expect("host scope should parse"),
                    kind: ServiceKind::Query,
                    interface_id: "window-query/v1".to_string(),
                },
                PluginService {
                    capability: HostScope::new("bmux.windows.write")
                        .expect("host scope should parse"),
                    kind: ServiceKind::Command,
                    interface_id: "window-command/v1".to_string(),
                },
            ],
            commands: vec![
                plugin_command(
                    "new-window",
                    "Create a new window in a session",
                    vec![vec!["window".to_string(), "new".to_string()]],
                ),
                plugin_command(
                    "list-windows",
                    "List windows for a session",
                    vec![vec!["window".to_string(), "list".to_string()]],
                ),
                plugin_command(
                    "kill-window",
                    "Kill a window by name, UUID, or active",
                    vec![vec!["window".to_string(), "kill".to_string()]],
                ),
                plugin_command(
                    "kill-all-windows",
                    "Kill all windows in a session",
                    vec![vec!["window".to_string(), "kill-all".to_string()]],
                ),
                plugin_command(
                    "switch-window",
                    "Switch active window by name, UUID, or active",
                    vec![vec!["window".to_string(), "switch".to_string()]],
                ),
            ],
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            },
        }
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
}

bmux_plugin::export_plugin!(WindowsPlugin);

fn plugin_command(name: &str, summary: &str, aliases: Vec<Vec<String>>) -> PluginCommand {
    let mut command = PluginCommand::new(name, summary)
        .path([name])
        .execution(CommandExecutionKind::HostCallback)
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
    let paths = command_paths(context);
    match block_on_plugin_future(async move { async_new_window(&paths, args).await }) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_new_window(paths: &ConfigPaths, args: WindowArgs) -> i32 {
    let selector = args.session.as_deref().map(parse_session_selector);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let window_id = match client.new_window(selector.clone(), args.name).await {
        Ok(window_id) => window_id,
        Err(error) => {
            eprintln!("failed creating window: {error}");
            return 1;
        }
    };
    match resolve_window_summary_by_id(&mut client, selector, window_id).await {
        Ok(Some(window)) => {
            match cli_window_context_label(&mut client, &window).await {
                Ok(label) => println!("created window: {label}"),
                Err(error) => {
                    eprintln!("failed resolving created window label: {error}");
                    return 1;
                }
            }
            0
        }
        Ok(None) => {
            eprintln!("created window missing from list response");
            1
        }
        Err(error) => {
            eprintln!("failed resolving created window: {error}");
            1
        }
    }
}

fn run_list_windows(context: &NativeCommandContext) -> i32 {
    let args = match parse_window_args(&context.arguments) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let paths = command_paths(context);
    match block_on_plugin_future(async move { async_list_windows(&paths, args).await }) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_list_windows(paths: &ConfigPaths, args: WindowArgs) -> i32 {
    let selector = args.session.as_deref().map(parse_session_selector);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let windows = match client.list_windows(selector).await {
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

    let sessions = match client.list_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            eprintln!("failed resolving session labels: {error}");
            return 1;
        }
    };

    let mut windows = windows;
    sort_windows(&mut windows);
    if args.session.is_none() {
        let session_label = sessions
            .iter()
            .find(|entry| entry.id == windows[0].session_id)
            .map(session_summary_label)
            .unwrap_or_else(|| format!("session-{}", short_uuid(windows[0].session_id)));
        println!("session context: {session_label}");
    }
    println!("ID                                   SESSION          WINDOW ACTIVE");
    for window in windows {
        let session_label = sessions
            .iter()
            .find(|session| session.id == window.session_id)
            .map(session_summary_label)
            .unwrap_or_else(|| format!("session-{}", short_uuid(window.session_id)));
        println!(
            "{:<36} {:<16} {:<12} {}",
            window.id,
            session_label,
            window_summary_label(&window),
            if window.active { "yes" } else { "no" }
        );
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
    let paths = command_paths(context);
    match block_on_plugin_future(async move {
        async_kill_window(&paths, args.session, &target, args.force_local).await
    }) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_kill_window(
    paths: &ConfigPaths,
    session: Option<String>,
    target: &str,
    force_local: bool,
) -> i32 {
    let selector = session.as_deref().map(parse_session_selector);
    let window_selector = parse_window_selector(target);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match client
        .kill_window_with_options(selector, window_selector, force_local)
        .await
    {
        Ok(window_id) => {
            println!("killed window: {window_id}");
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
    let paths = command_paths(context);
    match block_on_plugin_future(async move {
        async_kill_all_windows(&paths, args.session, args.force_local).await
    }) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_kill_all_windows(
    paths: &ConfigPaths,
    session: Option<String>,
    force_local: bool,
) -> i32 {
    let selector = session.as_deref().map(parse_session_selector);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let windows = match client.list_windows(selector.clone()).await {
        Ok(windows) => windows,
        Err(error) => {
            eprintln!("failed listing windows: {error}");
            return 1;
        }
    };
    if windows.is_empty() {
        println!("no windows");
        return 0;
    }
    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
    for window in windows {
        match client
            .kill_window_with_options(
                selector.clone(),
                WindowSelector::ById(window.id),
                force_local,
            )
            .await
        {
            Ok(window_id) => {
                println!("killed window: {window_id}");
                killed_count = killed_count.saturating_add(1);
            }
            Err(error) => {
                failed_count = failed_count.saturating_add(1);
                eprintln!("failed killing window {}: {error}", window.id);
            }
        }
    }
    println!("kill-all-windows complete: killed {killed_count}, failed {failed_count}");
    if failed_count == 0 { 0 } else { 1 }
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
    let paths = command_paths(context);
    match block_on_plugin_future(
        async move { async_switch_window(&paths, args.session, &target).await },
    ) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_switch_window(paths: &ConfigPaths, session: Option<String>, target: &str) -> i32 {
    let selector = session.as_deref().map(parse_session_selector);
    let window_selector = parse_window_selector(target);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    let window_id = match client
        .switch_window(selector.clone(), window_selector)
        .await
    {
        Ok(window_id) => window_id,
        Err(error) => {
            eprintln!("failed switching window: {error}");
            return 1;
        }
    };
    match resolve_window_summary_by_id(&mut client, selector, window_id).await {
        Ok(Some(window)) => match cli_window_context_label(&mut client, &window).await {
            Ok(label) => {
                println!("active window: {label}");
                0
            }
            Err(error) => {
                eprintln!("failed resolving switched window label: {error}");
                1
            }
        },
        Ok(None) => {
            eprintln!("active window missing from list response");
            1
        }
        Err(error) => {
            eprintln!("failed resolving switched window: {error}");
            1
        }
    }
}

fn command_paths(context: &NativeCommandContext) -> ConfigPaths {
    ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    )
}

async fn connect_client(paths: &ConfigPaths) -> Result<BmuxClient, i32> {
    BmuxClient::connect_with_paths(paths, "bmux-windows-plugin")
        .await
        .map_err(|error| {
            eprintln!("failed connecting to bmux host: {error}");
            1
        })
}

fn block_on_plugin_future<F>(future: F) -> Result<i32, ()>
where
    F: std::future::Future<Output = i32>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => Ok(runtime.block_on(future)),
            Err(_) => Err(()),
        },
    }
}

fn parse_session_selector(target: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(target) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(target.to_string()),
    }
}

fn parse_window_selector(target: &str) -> WindowSelector {
    if target.eq_ignore_ascii_case("active") {
        return WindowSelector::Active;
    }
    if let Ok(number) = target.parse::<u32>()
        && number > 0
    {
        return WindowSelector::ByNumber(number);
    }
    match uuid::Uuid::parse_str(target) {
        Ok(id) => WindowSelector::ById(id),
        Err(_) => WindowSelector::ByName(target.to_string()),
    }
}

async fn resolve_window_summary_by_id(
    client: &mut BmuxClient,
    session: Option<SessionSelector>,
    window_id: uuid::Uuid,
) -> bmux_client::Result<Option<WindowSummary>> {
    Ok(client
        .list_windows(session)
        .await?
        .into_iter()
        .find(|window| window.id == window_id))
}

async fn cli_window_context_label(
    client: &mut BmuxClient,
    window: &WindowSummary,
) -> bmux_client::Result<String> {
    let session_label = client
        .list_sessions()
        .await?
        .into_iter()
        .find(|entry| entry.id == window.session_id)
        .map(|entry| session_summary_label(&entry))
        .unwrap_or_else(|| format!("session-{}", short_uuid(window.session_id)));
    Ok(format!(
        "{} in {session_label}",
        window_summary_label(window)
    ))
}

fn short_uuid(id: uuid::Uuid) -> String {
    id.as_simple().to_string()[..8].to_string()
}

fn session_summary_label(session: &bmux_ipc::SessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
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
        assert!(
            reparsed
                .provided_capabilities
                .iter()
                .any(|capability| capability.as_str() == "bmux.windows.read")
        );
    }
}
