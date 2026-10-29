//! Optional server-owned live publisher. CLI callers only invoke typed services.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
pub mod authorization;
#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;

mod committed_output;
pub use committed_output::authorize_committed_output;
mod controller;
mod deploy;
mod diagnostics;
mod indicator;
mod mirror;
mod platform;
mod presentation;
mod profiles;
pub use controller::DeliveryCounters;
pub use presentation::BroadcastPresentation;

/// In-process diagnostics only: no terminal writes and no controller startup.
pub fn delivery_counters() -> Option<DeliveryCounters> {
    let registry = CONTROLLER.get()?.lock().ok()?;
    registry.controller.as_ref()?.delivery_counters().ok()
}
mod guided;
mod identity;
mod inline_ui;
mod relay;
mod setup_ui;
mod start_identity;
mod startup_ui;
pub mod terminal;
use bmux_broadcast_plugin_api::{
    broadcast_control_v1 as control, broadcast_output_v1 as output, broadcast_turnkey_v1 as turnkey,
};
use bmux_plugin_sdk::{
    NativeCommandContext, NativeServiceContext, PluginCommandError, RustPlugin, ServiceResponse,
};
use std::sync::{Arc, Mutex, OnceLock};

struct Registry {
    controller: Option<Arc<controller::Controller>>,
    enabled: bool,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            controller: None,
            enabled: true,
        }
    }
}
static CONTROLLER: OnceLock<Mutex<Registry>> = OnceLock::new();
fn controller() -> Result<Arc<controller::Controller>, ServiceResponse> {
    let mut current = CONTROLLER
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| error("controller_unavailable"))?;
    if !current.enabled {
        return Err(error("controller_stopped"));
    }
    if let Some(controller) = current.controller.as_ref() {
        return Ok(controller.clone());
    }
    let next = Arc::new(controller::Controller::new().map_err(error)?);
    current.controller = Some(next.clone());
    Ok(next)
}
fn error(code: &'static str) -> ServiceResponse {
    ServiceResponse::error(code, "broadcast operation rejected")
}
fn authenticated(context: &NativeServiceContext) -> Result<uuid::Uuid, ServiceResponse> {
    authorization::check(
        context
            .caller_client_id
            .ok_or_else(|| error("authentication_required"))?,
    )
    .map_err(error)
}
#[derive(Default)]
pub struct BroadcastPlugin;
impl RustPlugin for BroadcastPlugin {
    type Contract = bmux_broadcast_plugin_api::Contract;
    fn activate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        CONTROLLER
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| PluginCommandError::failed("broadcast activation unavailable"))?
            .enabled = true;
        indicator::update("stopped", "offline");
        Ok(bmux_plugin_sdk::EXIT_OK)
    }
    fn deactivate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        authorization::revoke();
        if let Some(current) = CONTROLLER.get() {
            let controller = {
                let mut registry = current
                    .lock()
                    .map_err(|_| PluginCommandError::failed("broadcast shutdown unavailable"))?;
                registry.enabled = false;
                registry.controller.take()
            };
            if let Some(controller) = controller {
                controller.shutdown();
            }
        }
        indicator::update("stopped", "offline");
        indicator::remove();
        Ok(bmux_plugin_sdk::EXIT_OK)
    }
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "broadcast-diagnostics-v1", "status" => |_r: (), ctx| { authenticated(ctx)?; controller()?.diagnostics().map_err(error) },
            "broadcast-diagnostics-v1", "events" => |r: bmux_broadcast_plugin_api::broadcast_diagnostics_v1::client::EventsRequest, ctx| { authenticated(ctx)?; controller()?.events(r.last).map_err(error) },
            "broadcast-presentation-v1", "status" => |_r: (), _ctx| { Ok::<_, ServiceResponse>(indicator::status()) },
            "broadcast-turnkey-v1", "register" => |r: turnkey::client::RegisterRequest, ctx| { let owner = authenticated(ctx)?; controller()?.register_label(owner, &r.source_id, &r.label).map_err(error) },
            "broadcast-turnkey-v1", "sources" => |_r: (), ctx| { authenticated(ctx)?; controller()?.sources().map_err(error) },
            "broadcast-turnkey-v1", "status" => |_r: (), ctx| { authenticated(ctx)?; controller()?.turnkey_status().map_err(error) },
            "broadcast-turnkey-v1", "start-profile" => |r: turnkey::client::StartProfileRequest, ctx| {
                let owner = authenticated(ctx)?;
                let profile = profiles::selected(std::path::Path::new(&ctx.connection.config_dir), r.profile.as_deref()).map_err(error)?;
                let relay = relay::Relay::new(&profile.endpoint, &profile.channel, &profile.credential_file).map_err(error)?;
                let url = guided::viewer_url(&profile.endpoint, &profile.channel).map_err(error)?;
                controller()?.start_owned(owner, &r.source_id, r.profile, relay, url, std::path::Path::new(&ctx.connection.state_dir)).map_err(error)
            },
            "broadcast-turnkey-v1", "start-profile-publication" => |r: turnkey::client::StartProfilePublicationRequest, ctx| {
                authenticated(ctx)?;
                let profile = profiles::selected(std::path::Path::new(&ctx.connection.config_dir), r.profile.as_deref()).map_err(error)?;
                let relay = relay::Relay::new(&profile.endpoint, &profile.channel, &profile.credential_file).map_err(error)?;
                let url = guided::viewer_url(&profile.endpoint, &profile.channel).map_err(error)?;
                controller()?.start_profile_publication(&r.source_id, relay, url, std::path::Path::new(&ctx.connection.state_dir)).map_err(error)
            },
            "broadcast-turnkey-v1", "stop-owned" => |r: turnkey::client::StopOwnedRequest, ctx| { let owner = authenticated(ctx)?; controller()?.stop_owned(owner, &r.token).map_err(error) },
            "broadcast-control-v1", "prepare-local" => |_r: (), ctx| {
                if ctx.caller_client_id.is_none() { return Err(error("authentication_required")); }
                if !CONTROLLER.get_or_init(Mutex::default).lock().map_err(|_| error("controller_unavailable"))?.enabled { return Err(error("controller_stopped")); }
                authorization::initialize(std::path::Path::new(&ctx.connection.runtime_dir)).map_err(error)?;
                Ok::<_, ServiceResponse>(true)
            },
            "broadcast-control-v1", "authorize" => |r: control::client::AuthorizeRequest, ctx| {
                let caller = ctx.caller_client_id.ok_or_else(|| error("authentication_required"))?;
                if !CONTROLLER.get_or_init(Mutex::default).lock().map_err(|_| error("controller_unavailable"))?.enabled { return Err(error("controller_stopped")); }
                authorization::authorize(caller, &r.local_proof).map_err(error)
            },
            "broadcast-control-v1", "start" => |r: control::client::StartRequest, ctx| {
                let owner = authenticated(ctx)?;
                let relay = relay::Relay::new(&r.endpoint, &r.channel, std::path::Path::new(&r.credential_file)).map_err(error)?;
                controller()?.control_owned(owner, controller::Control::Start(Box::new(relay), r.source_id)).map_err(error)
            },
            "broadcast-control-v1", "pause" => |_r: (), ctx| { let owner = authenticated(ctx)?; controller()?.control_owned(owner, controller::Control::Pause).map_err(error) },
            "broadcast-control-v1", "resume" => |_r: (), ctx| { let owner = authenticated(ctx)?; controller()?.control_owned(owner, controller::Control::Resume).map_err(error) },
            "broadcast-control-v1", "stop" => |_r: (), ctx| { let owner = authenticated(ctx)?; controller()?.control_owned(owner, controller::Control::Stop).map_err(error) },
            "broadcast-control-v1", "start-publication" => |r: control::client::StartPublicationRequest, ctx| {
                authenticated(ctx)?;
                let relay = relay::Relay::new(&r.endpoint, &r.channel, std::path::Path::new(&r.credential_file)).map_err(error)?;
                controller()?.control_publication(controller::Control::Start(Box::new(relay), r.source_id)).map_err(error)
            },
            "broadcast-control-v1", "pause-publication" => |_r: (), ctx| { authenticated(ctx)?; controller()?.control_publication(controller::Control::Pause).map_err(error) },
            "broadcast-control-v1", "resume-publication" => |_r: (), ctx| { authenticated(ctx)?; controller()?.control_publication(controller::Control::Resume).map_err(error) },
            "broadcast-control-v1", "stop-publication" => |_r: (), ctx| { authenticated(ctx)?; controller()?.control_publication(controller::Control::Stop).map_err(error) },
            "broadcast-control-v1", "source-publication" => |r: control::client::SourcePublicationRequest, ctx| { authenticated(ctx)?; controller()?.control_publication(controller::Control::Source(r.source_id)).map_err(error) },
            "broadcast-control-v1", "status" => |_r: (), ctx| { authenticated(ctx)?; controller()?.status().map_err(error) },
            "broadcast-control-v1", "terminal" => |r: control::client::TerminalRequest, ctx| {
                let owner = authenticated(ctx)?;
                let target = uuid::Uuid::parse_str(&r.session_id).map_err(|_| error("invalid_target"))?;
                let cols = u16::try_from(r.cols).map_err(|_| error("viewport_bound"))?;
                let rows = u16::try_from(r.rows).map_err(|_| error("viewport_bound"))?;
                let selection = terminal::RuntimeSelection { executable: r.executable.into(), sha256: r.sha256, version: r.runtime_version, paths: bmux_config::ConfigPaths::new(ctx.connection.config_dir.clone().into(), ctx.connection.runtime_dir.clone().into(), ctx.connection.data_dir.clone().into(), ctx.connection.state_dir.clone().into()) };
                let controller = controller()?;
                std::thread::scope(|scope| scope.spawn(move || controller.terminal(owner, r.source_id, selection, target, cols, rows)).join().map_err(|_| "executor_unavailable")?).map_err(error)
            },
            "broadcast-control-v1", "source" => |r: control::client::SourceRequest, ctx| { let owner = authenticated(ctx)?; controller()?.control_owned(owner, controller::Control::Source(r.source_id)).map_err(error) },
            "broadcast-output-v1", "register" => |r: output::client::RegisterRequest, ctx| { let owner = authenticated(ctx)?; controller()?.source(owner, &r.source_id, true).map_err(error) },
            "broadcast-output-v1", "poll" => |r: output::client::PollRequest, ctx| { let owner = authenticated(ctx)?; controller()?.source(owner, &r.source_id, false).map_err(error) },
            "broadcast-output-v1", "publish" => |r: output::client::PublishRequest, ctx| { let owner = authenticated(ctx)?; controller()?.publish(owner, &r.source_id, r.refresh, r.frame).map_err(error) },
            "broadcast-output-v1", "unregister" => |r: output::client::UnregisterRequest, ctx| { let owner = authenticated(ctx)?; controller()?.unregister(owner, &r.source_id).map_err(error) },
        })
    }
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        if context.invocation_source
            == bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding
        {
            let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                PluginCommandError::failed("broadcast prompts require attach runtime")
            })?;
            handle.spawn_blocking(move || {
                if let Err(error) = run_command(context) {
                    let text = format!("Broadcast action failed: {error}");
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        runtime.spawn(async move {
                            let _ = bmux_plugin::prompt::request(
                                bmux_plugin::prompt::PromptRequest::confirm(text)
                                    .confirm_labels("OK", "Close"),
                            )
                            .await;
                        });
                    }
                }
            });
            return Ok(bmux_plugin_sdk::EXIT_OK);
        }
        std::thread::scope(|scope| {
            scope
                .spawn(move || run_command(context))
                .join()
                .map_err(|_| PluginCommandError::failed("broadcast caller failed"))?
        })
    }
}
fn run_command(mut context: NativeCommandContext) -> Result<i32, PluginCommandError> {
    if let Some(action) = context.command.strip_prefix("broadcast-") {
        context.arguments.insert(0, action.to_owned());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| PluginCommandError::failed("broadcast caller unavailable"))?;
    runtime.block_on(async {
        if context.arguments.first().is_some_and(|arg| arg == "deploy") { return deploy::run(&context, &context.arguments[1..]).await; }
        if guided::local(&context).await? { return Ok(bmux_plugin_sdk::EXIT_OK); }
        let paths = bmux_config::ConfigPaths::new(context.connection.config_dir.clone().into(), context.connection.runtime_dir.clone().into(), context.connection.data_dir.clone().into(), context.connection.state_dir.clone().into());
        let mut client = bmux_client::BmuxClient::connect_with_paths_and_timeout(&paths, "broadcast-control", std::time::Duration::from_secs(30)).await.map_err(|_| PluginCommandError::failed("broadcast service unavailable"))?;
        control::client::prepare_local(&mut client).await.map_err(|_| PluginCommandError::failed("broadcast local authorization unavailable"))?;
        let proof = authorization::local_proof(&paths.runtime_dir).map_err(|_| PluginCommandError::failed("broadcast local authorization unavailable"))?;
        control::client::authorize(&mut client, proof).await.map_err(|_| PluginCommandError::failed("broadcast local authorization rejected"))?;
        if guided::connected(&context, &mut client).await? { return Ok(bmux_plugin_sdk::EXIT_OK); }
        let args = &context.arguments;
        let result = match args.first().map(String::as_str) {
            Some("start") if args.len() == 5 => control::client::start_publication(&mut client, args[1].clone(), args[2].clone(), args[3].clone(), args[4].clone()).await,
            Some("pause") if args.len() == 1 => control::client::pause_publication(&mut client).await,
            Some("resume") if args.len() == 1 => control::client::resume_publication(&mut client).await,
            Some("stop") if args.len() == 1 => control::client::stop_publication(&mut client).await,
            Some("status") if args.len() == 1 => control::client::status(&mut client).await,
            Some("terminal") if args.len() == 8 => {
                let cols = args[6].parse().map_err(|_| PluginCommandError::failed("invalid columns"))?;
                let rows = args[7].parse().map_err(|_| PluginCommandError::failed("invalid rows"))?;
                control::client::terminal(&mut client, args[1].clone(), args[2].clone(), args[3].clone(), args[4].clone(), args[5].clone(), cols, rows).await
            },
            Some("source") if args.len() == 2 => control::client::source_publication(&mut client, args[1].clone()).await,
            _ => return Err(PluginCommandError::failed("usage: broadcast start ENDPOINT CHANNEL CREDENTIAL_FILE SOURCE_UUID | pause | resume | stop | status | source SOURCE_UUID | terminal SOURCE_UUID SESSION_UUID ABSOLUTE_EXECUTABLE SHA256 VERSION COLS ROWS")),
        };
        let status = result.map_err(|_| PluginCommandError::failed("broadcast operation rejected"))?;
        if context.invocation_source.is_cli() { println!("{}", serde_json::to_string(&status).map_err(|_| PluginCommandError::failed("broadcast status unavailable"))?); }
        Ok(bmux_plugin_sdk::EXIT_OK)
    })
}
bmux_plugin_sdk::export_plugin!(BroadcastPlugin, include_str!("../plugin.toml"));
