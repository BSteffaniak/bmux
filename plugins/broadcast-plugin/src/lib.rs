//! Optional server-owned live publisher. CLI callers only invoke typed services.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_broadcast_plugin_api::export_schema,
            crate_path: ::bmux_broadcast_plugin_api,
        },
        settings: "plugin.toml",
    }
}

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
pub use controller::{DeliveryCounters, IndicatorPort};
pub use presentation::BroadcastPresentation;

/// In-process diagnostics only: no terminal writes and no controller startup.
pub fn delivery_counters() -> Option<DeliveryCounters> {
    let registry = LEGACY.get()?.registry.lock().ok()?;
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
static LEGACY: OnceLock<BroadcastPlugin> = OnceLock::new();
fn legacy() -> &'static BroadcastPlugin {
    LEGACY.get_or_init(|| BroadcastPlugin::with_indicator(Arc::new(controller::LegacyIndicator)))
}
fn error(code: &'static str) -> ServiceResponse {
    ServiceResponse::error(code, "broadcast operation rejected")
}
#[derive(Clone)]
pub struct BroadcastPlugin {
    registry: Arc<Mutex<Registry>>,
    authorization: Arc<authorization::Authorization>,
    indicator: Arc<dyn IndicatorPort>,
}
impl Default for BroadcastPlugin {
    fn default() -> Self {
        legacy().clone()
    }
}
impl BroadcastPlugin {
    /// Diagnostics for this exact loaded instance; shared hosts must use this
    /// rather than the legacy process facade.
    pub fn delivery_counters(&self) -> Option<DeliveryCounters> {
        self.registry
            .lock()
            .ok()?
            .controller
            .as_ref()?
            .delivery_counters()
            .ok()
    }

    pub fn with_indicator(indicator: Arc<dyn IndicatorPort>) -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            authorization: Arc::new(authorization::Authorization::default()),
            indicator,
        }
    }
    fn controller(&self) -> Result<Arc<controller::Controller>, ServiceResponse> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| error("controller_unavailable"))?;
        if !registry.enabled {
            return Err(error("controller_stopped"));
        }
        if let Some(controller) = registry.controller.as_ref() {
            return Ok(Arc::clone(controller));
        }
        let next = Arc::new(
            controller::Controller::with_indicator(Arc::clone(&self.indicator)).map_err(error)?,
        );
        registry.controller = Some(Arc::clone(&next));
        Ok(next)
    }
    fn authenticated(&self, context: &NativeServiceContext) -> Result<uuid::Uuid, ServiceResponse> {
        self.authorization
            .check(
                context
                    .caller_client_id
                    .ok_or_else(|| error("authentication_required"))?,
            )
            .map_err(error)
    }
    fn activate_owner(&self) -> Result<(), PluginCommandError> {
        self.registry
            .lock()
            .map_err(|_| PluginCommandError::failed("broadcast activation unavailable"))?
            .enabled = true;
        self.indicator.update("stopped", "offline");
        Ok(())
    }
    fn deactivate_owner(&self) -> Result<(), PluginCommandError> {
        let controller = {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| PluginCommandError::failed("broadcast shutdown unavailable"))?;
            registry.enabled = false;
            registry.controller.clone()
        };
        // Retain the controller and relay on unacknowledged stop.
        if let Some(controller) = controller {
            controller
                .shutdown_checked()
                .map_err(PluginCommandError::failed)?;
        }
        self.authorization
            .revoke()
            .map_err(PluginCommandError::failed)?;
        self.registry
            .lock()
            .map_err(|_| PluginCommandError::failed("broadcast shutdown unavailable"))?
            .controller = None;
        self.indicator.update("stopped", "offline");
        self.indicator.remove();
        Ok(())
    }
}
impl RustPlugin for BroadcastPlugin {
    type Contract = bmux_broadcast_plugin_api::Contract;
    fn activate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        self.activate_owner()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }
    fn deactivate(
        &mut self,
        _context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        self.deactivate_owner()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        self.service(context)
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
impl BroadcastPlugin {
    fn service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "broadcast-diagnostics-v1", "status" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.diagnostics().map_err(error) },
            "broadcast-diagnostics-v1", "events" => |r: bmux_broadcast_plugin_api::broadcast_diagnostics_v1::client::EventsRequest, ctx| { self.authenticated(ctx)?; self.controller()?.events(r.last).map_err(error) },
            "broadcast-presentation-v1", "status" => |_r: (), _ctx| { Ok::<_, ServiceResponse>(self.indicator.status()) },
            "broadcast-turnkey-v1", "register" => |r: turnkey::client::RegisterRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.register_label(owner, &r.source_id, &r.label).map_err(error) },
            "broadcast-turnkey-v1", "sources" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.sources().map_err(error) },
            "broadcast-turnkey-v1", "status" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.turnkey_status().map_err(error) },
            "broadcast-turnkey-v1", "start-profile" => |r: turnkey::client::StartProfileRequest, ctx| {
                let owner = self.authenticated(ctx)?;
                let profile = profiles::selected(std::path::Path::new(&ctx.connection.config_dir), r.profile.as_deref()).map_err(error)?;
                let relay = relay::Relay::new(&profile.endpoint, &profile.channel, &profile.credential_file).map_err(error)?;
                let url = guided::viewer_url(&profile.endpoint, &profile.channel).map_err(error)?;
                self.controller()?.start_owned(owner, &r.source_id, r.profile, relay, url, std::path::Path::new(&ctx.connection.state_dir)).map_err(error)
            },
            "broadcast-turnkey-v1", "start-profile-publication" => |r: turnkey::client::StartProfilePublicationRequest, ctx| {
                self.authenticated(ctx)?;
                let profile = profiles::selected(std::path::Path::new(&ctx.connection.config_dir), r.profile.as_deref()).map_err(error)?;
                let relay = relay::Relay::new(&profile.endpoint, &profile.channel, &profile.credential_file).map_err(error)?;
                let url = guided::viewer_url(&profile.endpoint, &profile.channel).map_err(error)?;
                self.controller()?.start_profile_publication(&r.source_id, relay, url, std::path::Path::new(&ctx.connection.state_dir)).map_err(error)
            },
            "broadcast-turnkey-v1", "stop-owned" => |r: turnkey::client::StopOwnedRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.stop_owned(owner, &r.token).map_err(error) },
            "broadcast-control-v1", "prepare-local" => |_r: (), ctx| {
                if ctx.caller_client_id.is_none() { return Err(error("authentication_required")); }
                if !self.registry.lock().map_err(|_| error("controller_unavailable"))?.enabled { return Err(error("controller_stopped")); }
                self.authorization.initialize(std::path::Path::new(&ctx.connection.runtime_dir)).map_err(error)?;
                Ok::<_, ServiceResponse>(true)
            },
            "broadcast-control-v1", "authorize" => |r: control::client::AuthorizeRequest, ctx| {
                let caller = ctx.caller_client_id.ok_or_else(|| error("authentication_required"))?;
                if !self.registry.lock().map_err(|_| error("controller_unavailable"))?.enabled { return Err(error("controller_stopped")); }
                self.authorization.authorize(caller, &r.local_proof).map_err(error)
            },
            "broadcast-control-v1", "start" => |r: control::client::StartRequest, ctx| {
                let owner = self.authenticated(ctx)?;
                let relay = relay::Relay::new(&r.endpoint, &r.channel, std::path::Path::new(&r.credential_file)).map_err(error)?;
                self.controller()?.control_owned(owner, controller::Control::Start(Box::new(relay), r.source_id)).map_err(error)
            },
            "broadcast-control-v1", "pause" => |_r: (), ctx| { let owner = self.authenticated(ctx)?; self.controller()?.control_owned(owner, controller::Control::Pause).map_err(error) },
            "broadcast-control-v1", "resume" => |_r: (), ctx| { let owner = self.authenticated(ctx)?; self.controller()?.control_owned(owner, controller::Control::Resume).map_err(error) },
            "broadcast-control-v1", "stop" => |_r: (), ctx| { let owner = self.authenticated(ctx)?; self.controller()?.control_owned(owner, controller::Control::Stop).map_err(error) },
            "broadcast-control-v1", "start-publication" => |r: control::client::StartPublicationRequest, ctx| {
                self.authenticated(ctx)?;
                let relay = relay::Relay::new(&r.endpoint, &r.channel, std::path::Path::new(&r.credential_file)).map_err(error)?;
                self.controller()?.control_publication(controller::Control::Start(Box::new(relay), r.source_id)).map_err(error)
            },
            "broadcast-control-v1", "pause-publication" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.control_publication(controller::Control::Pause).map_err(error) },
            "broadcast-control-v1", "resume-publication" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.control_publication(controller::Control::Resume).map_err(error) },
            "broadcast-control-v1", "stop-publication" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.control_publication(controller::Control::Stop).map_err(error) },
            "broadcast-control-v1", "source-publication" => |r: control::client::SourcePublicationRequest, ctx| { self.authenticated(ctx)?; self.controller()?.control_publication(controller::Control::Source(r.source_id)).map_err(error) },
            "broadcast-control-v1", "status" => |_r: (), ctx| { self.authenticated(ctx)?; self.controller()?.status().map_err(error) },
            "broadcast-control-v1", "terminal" => |r: control::client::TerminalRequest, ctx| {
                let owner = self.authenticated(ctx)?;
                let target = uuid::Uuid::parse_str(&r.session_id).map_err(|_| error("invalid_target"))?;
                let cols = u16::try_from(r.cols).map_err(|_| error("viewport_bound"))?;
                let rows = u16::try_from(r.rows).map_err(|_| error("viewport_bound"))?;
                let selection = terminal::RuntimeSelection { executable: r.executable.into(), sha256: r.sha256, version: r.runtime_version, paths: bmux_config::ConfigPaths::new(ctx.connection.config_dir.clone().into(), ctx.connection.runtime_dir.clone().into(), ctx.connection.data_dir.clone().into(), ctx.connection.state_dir.clone().into()) };
                let controller = self.controller()?;
                std::thread::scope(|scope| scope.spawn(move || controller.terminal(owner, r.source_id, selection, target, cols, rows)).join().map_err(|_| "executor_unavailable")?).map_err(error)
            },
            "broadcast-control-v1", "source" => |r: control::client::SourceRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.control_owned(owner, controller::Control::Source(r.source_id)).map_err(error) },
            "broadcast-output-v1", "register" => |r: output::client::RegisterRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.source(owner, &r.source_id, true).map_err(error) },
            "broadcast-output-v1", "poll" => |r: output::client::PollRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.source(owner, &r.source_id, false).map_err(error) },
            "broadcast-output-v1", "publish" => |r: output::client::PublishRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.publish(owner, &r.source_id, r.refresh, r.frame).map_err(error) },
            "broadcast-output-v1", "unregister" => |r: output::client::UnregisterRequest, ctx| { let owner = self.authenticated(ctx)?; self.controller()?.unregister(owner, &r.source_id).map_err(error) },
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
impl bmux_plugin_sdk::definition::PluginInstance for BroadcastPlugin {
    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        <bmux_broadcast_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface)
    }

    fn activate(
        &self,
        _context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        Box::pin(async move { self.activate_owner() })
    }
    fn deactivate(
        &self,
        _context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        let owner = self.clone();
        Box::pin(async move {
            tokio::runtime::Handle::try_current().map_err(|_| {
                PluginCommandError::unavailable("broadcast cleanup requires host async runtime")
            })?;
            tokio::task::spawn_blocking(move || owner.deactivate_owner())
                .await
                .map_err(|_| PluginCommandError::failed("broadcast cleanup task panicked"))?
        })
    }
    fn invoke(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeServiceContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<ServiceResponse>,
    > {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "broadcast invocation cancelled",
                ));
            }
            Ok(self.service(context))
        })
    }
    fn command(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeCommandContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::CommandOutcome>
    {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "broadcast command cancelled",
                ))
                .into();
            }
            // Same command workflow; no new domain transport client or ABI.
            std::thread::scope(|scope| {
                scope
                    .spawn(move || run_command(context))
                    .join()
                    .unwrap_or_else(|_| Err(PluginCommandError::failed("broadcast caller failed")))
            })
            .into()
        })
    }
}
// The one definition both bundling and generated dynamic packaging use:
// fresh default author state per created instance.
bmux_plugin_sdk::define_plugin!(
    rust_plugin = BroadcastPlugin,
    manifest = crate::declaration::PLUGIN_MANIFEST,
    roles = vec![
        bmux_plugin_sdk::definition::ComponentRole::Backend,
        bmux_plugin_sdk::definition::ComponentRole::Frontend
    ],
);

pub fn plugin_definition_with_indicator(
    indicator: Arc<dyn IndicatorPort>,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    use bmux_plugin_sdk::definition::{PluginDefinition, PluginInstance};
    PluginDefinition::new(definition_metadata(), move |_scope| {
        Ok(
            Arc::new(BroadcastPlugin::with_indicator(Arc::clone(&indicator)))
                as Arc<dyn PluginInstance>,
        )
    })
}

fn definition_metadata() -> bmux_plugin_sdk::definition::PluginMetadata {
    bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<BroadcastPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        vec![
            bmux_plugin_sdk::definition::ComponentRole::Backend,
            bmux_plugin_sdk::definition::ComponentRole::Frontend,
        ],
    )
    .expect("plugin manifest must define valid canonical metadata")
    .metadata
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
            super::definition_metadata(),
            |_scope| {
                Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "metadata-only regression",
                ))
            },
        );
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
