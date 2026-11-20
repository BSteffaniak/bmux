//! Pinned executors for `attach` and `connect` on the selected command path.
//!
//! A plugin reaching these through the core CLI bridge already runs inside an
//! explicitly selected runtime. The executors attach against that runtime's
//! config and paths instead of re-resolving ambient process configuration.

use anyhow::Result;
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use std::time::Duration;

use super::plugin_runtime::RuntimeCommandState;

/// Run a selected `attach` or `connect` command against `state`.
///
/// # Errors
/// Returns parse-shape, connection, or attachment failures.
pub(super) fn run_selected(
    state: &RuntimeCommandState,
    command: &bmux_cli_schema::Command,
) -> Result<i32> {
    let run = async {
        match command {
            bmux_cli_schema::Command::Attach {
                target,
                follow,
                global,
                observer_socket,
                web,
                web_assets,
                web_only,
                ..
            } => {
                anyhow::ensure!(
                    observer_socket.is_none() && !*web && !*web_only && web_assets.is_none(),
                    "attach observer and browser mirroring are not available on the selected command path; run `bmux attach` directly"
                );
                Box::pin(run_attach(
                    state,
                    target.as_deref(),
                    follow.as_deref(),
                    *global,
                ))
                .await
            }
            _ => anyhow::bail!("selected attach executor received another command"),
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(run)).map(i32::from)
        }
        Ok(_) => anyhow::bail!("selected attach executor requires a blocking execution context"),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(run)
            .map(i32::from),
    }
}

/// Attach to `target` (or follow a client) on the selected runtime.
///
/// Provider-owned targets open without a local server; every other target
/// connects to the selected runtime's local server.
pub(super) async fn run_attach(
    state: &RuntimeCommandState,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let (config, paths) = (&state.config, &state.paths);
    let provider = super::attach::provider::resolve(target)?;
    let fallback_client = if provider.requires_fallback_client() {
        Some(connect_local(config, paths, "bmux-cli-attach").await?)
    } else {
        None
    };
    let session = provider
        .open(
            None,
            bmux_client::AttachProviderOpenContext {
                fallback_client,
                endpoint_connector: Some(std::sync::Arc::new(
                    crate::connection::CliAttachEndpointConnector,
                )),
            },
        )
        .await?;
    Box::pin(
        super::attach::runtime::run_session_attach_with_provider_session_config(
            session,
            target,
            follow,
            global,
            None,
            Some(config.clone()),
            // Caller-process plugin commands run with this selected runtime
            // (and its owner), carried explicitly into the attachment.
            Some(state.clone()),
        ),
    )
    .await
    .map(|outcome| outcome.status_code)
}

/// Connect to the selected runtime's local server.
pub(super) async fn connect_local(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    client_name: &'static str,
) -> Result<BmuxClient> {
    BmuxClient::connect_with_paths_and_timeout(
        paths,
        client_name,
        Duration::from_millis(config.general.server_timeout.max(1)),
    )
    .await
    .map_err(super::map_attach_client_error)
}
