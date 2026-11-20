//! Selected standalone transport. Server-side instances have no process terminal or offline storage authority.
use super::{SessionsPlugin, declaration, kill};
use bmux_client::{BmuxClient, ClientError};
use bmux_plugin_sdk::{NativeCommandContext, NativeCommandInvocationSource, PluginCommandError};
use std::sync::Arc;

type CommandFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<u8>> + Send + 'a>>;

type Attach = dyn Fn(Option<&str>, Option<&str>, bool) -> anyhow::Result<u8> + Send + Sync;
#[derive(Clone)]
pub struct TerminalClient {
    /// Poll standalone I/O on the runtime owning the selected transport.
    pub drive: for<'a> fn(CommandFuture<'a>) -> anyhow::Result<u8>,
    pub connect_if_running:
        Arc<dyn Fn(&'static str) -> anyhow::Result<Option<BmuxClient>> + Send + Sync>,
    pub attach: Arc<Attach>,
    pub detach: Arc<dyn Fn() -> anyhow::Result<u8> + Send + Sync>,
    pub map_error: fn(ClientError) -> anyhow::Error,
}

/// Bind a standalone invocation to its selected transport.
///
/// # Panics
/// If the embedded sessions manifest is invalid.
#[must_use]
pub fn definition(client: TerminalClient) -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_plugin_sdk::author::definition::<SessionsPlugin>(
        declaration::PLUGIN_MANIFEST,
        declaration::SERVICE_CONTRACTS,
        move |_| {
            Ok(SessionsPlugin {
                terminal: Some(client.clone()),
                ..SessionsPlugin::default()
            })
        },
    )
    .expect("valid sessions declaration")
}

impl SessionsPlugin {
    pub(super) fn run_terminal_command(
        &self,
        ctx: &NativeCommandContext,
    ) -> Result<i32, PluginCommandError> {
        let client = self
            .terminal
            .as_ref()
            .filter(|_| {
                ctx.invocation_source == NativeCommandInvocationSource::LocalProcess
                    && ctx.caller_client_id.is_none()
            })
            .ok_or_else(|| {
                PluginCommandError::unavailable(
                    "session command requires a standalone terminal client",
                )
            })?;
        let args = &ctx.arguments;
        let result = match ctx.command.as_str() {
            "session-attach" => {
                let mut target = None;
                let mut follow = None;
                let mut global = false;
                let mut iter = args.iter().map(String::as_str);
                while let Some(arg) = iter.next() {
                    match arg {
                        "--follow" => {
                            follow = Some(iter.next().ok_or_else(|| {
                                PluginCommandError::invalid_arguments(
                                    "--follow requires a client UUID",
                                )
                            })?);
                        }
                        "--global" => global = true,
                        _ if target.is_none() => target = Some(arg),
                        _ => {
                            return Err(PluginCommandError::invalid_arguments(
                                "unexpected attach argument",
                            ));
                        }
                    }
                }
                if global && follow.is_none() {
                    return Err(PluginCommandError::invalid_arguments(
                        "--global requires --follow",
                    ));
                }
                (client.attach)(target, follow, global)
            }
            "detach" => (client.detach)(),
            "kill-session" | "kill-all-sessions" => {
                let force = args.iter().any(|s| s == "--force-local");
                (client.drive)(Box::pin(async {
                    if ctx.command == "kill-all-sessions" {
                        kill::run_session_kill_all(force, client).await
                    } else {
                        let target = args
                            .iter()
                            .find(|s| s.as_str() != "--force-local")
                            .ok_or_else(|| anyhow::anyhow!("missing session target"))?;
                        kill::run_session_kill(target, force, client).await
                    }
                }))
            }
            other => return Err(PluginCommandError::unknown_command(other)),
        };
        result
            .map(i32::from)
            .map_err(|error| PluginCommandError::failed(format!("{error:#}")))
    }
}
