#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Remote access for bmux: a paired device reaches this machine's runtime
//! over Iroh (QUIC), and `bmux connect <host>` attaches through a saved-host
//! transport connector. See `docs/remote-pairing.md`.

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_connections_plugin_api::export_schema,
            crate_path: ::bmux_connections_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod audit;
mod cli;
mod config;
mod connector;
mod host;
mod identity;
mod invoke;
mod known_hosts;
mod link;
pub mod paired_hosts;
mod pairing;
mod roles;
mod service;
mod storage;
mod transport;
mod trust;

pub use cli::kiosk::KioskAttachPort;

use bmux_plugin_sdk::author::{Call, Deactivation, Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use std::sync::{Arc, RwLock};

/// Write text to the selected client clipboard.
pub type ClipboardPort = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

/// Resources of one standalone CLI invocation; never installed in a server.
#[derive(Clone)]
pub struct ClientHost {
    pub paths: bmux_config::ConfigPaths,
    pub config: bmux_config::BmuxConfig,
    pub runtime_name: String,
    pub copy: ClipboardPort,
    pub kiosk_attach: KioskAttachPort,
}

/// The connections plugin. In a server it hosts remote access (when
/// enabled); in a standalone CLI invocation it runs client and host commands.
pub struct ConnectionsInstance {
    client: Option<ClientHost>,
    host: RwLock<Option<Arc<host::Host>>>,
}

impl ConnectionsInstance {
    const fn new(client: Option<ClientHost>) -> Self {
        Self {
            client,
            host: RwLock::new(None),
        }
    }

    fn host(&self) -> Option<Arc<host::Host>> {
        self.host
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn activate(&self, context: &NativeLifecycleContext) -> Result<(), PluginCommandError> {
        // A standalone CLI instance never owns host identity or listeners.
        if self.client.is_some() {
            return Ok(());
        }
        let settings = config::RemoteSettings::load(context.settings.as_ref())
            .map_err(|error| PluginCommandError::failed(format!("{error:#}")))?;
        let host = host::Host::new(
            std::path::Path::new(&context.connection.state_dir),
            settings,
        )
        .map_err(|error| {
            PluginCommandError::failed(format!("remote host key unavailable: {error:#}"))
        })?;
        let states = bmux_plugin::activation_state_registry(context);
        host.set_ingress(
            bmux_plugin::stream_admission::StreamIngressHandle::from_host_registry(&states),
        );
        let caller = bmux_plugin::TypedServiceCaller::from_lifecycle_context(context);
        host.start_at_activation(&roles::PermissionsGrants { caller: &caller })
            .map_err(|error| PluginCommandError::failed(format!("{error:#}")))?;
        *self
            .host
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(host));
        Ok(())
    }

    /// Stop hosting with the activation: every remote connection closes.
    async fn deactivate_host(&self) {
        let host = self
            .host
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(host) = host {
            host.shutdown().await;
        }
    }

    /// Commands run in the invoking CLI process, on the host runtime. Terminal
    /// prompts and host calls that block go to the blocking pool.
    async fn command(&self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        let failed =
            |error: anyhow::Error| PluginCommandError::failed(format!("bmux error: {error:#}"));
        cli::require_local_process(&context).map_err(failed)?;
        let name = context.command.as_str();
        if let Some(leaf) = name.strip_prefix("kiosk-") {
            let client = self.client.as_ref().ok_or_else(|| {
                PluginCommandError::failed("kiosk commands run from a local terminal")
            })?;
            let (leaf, arguments, attach) = (
                leaf.to_owned(),
                context.arguments.clone(),
                client.kiosk_attach.clone(),
            );
            return cli::blocking(move || Ok(cli::kiosk::run(&leaf, &arguments, &attach, None)))
                .await
                .map_err(failed)??
                .map(i32::from)
                .map_err(failed);
        }
        let result = match name {
            "connect" => cli::client::connect(&context).await,
            "remote-hosts" => cli::client::hosts(&context),
            "remote-rename" => cli::client::rename(&context),
            "remote-forget" => cli::client::forget(&context),
            "remote-status" => cli::status::status(&context).await,
            "remote-log" => cli::status::log(&context).await,
            "remote-doctor" => cli::doctor::run(&context).await,
            _ => cli::host::run(&cli::paths(&context), name, &context.arguments).await,
        };
        result.map_err(failed)
    }
}

fn run_command<'a>(
    instance: &'a ConnectionsInstance,
    _cx: &'a Call,
    context: NativeCommandContext,
) -> impl std::future::Future<Output = Result<i32, PluginCommandError>> + Send + 'a {
    instance.command(context)
}

impl Plugin for ConnectionsInstance {
    fn declare(d: &mut Declarations<Self>) {
        // Activation and services stay synchronous: their permission and
        // invocation-origin host calls must run inside the callback entry.
        d.blocking_activate(|instance, context, _| instance.activate(context));
        for command in [
            "connect",
            "remote-enable",
            "remote-disable",
            "remote-pair",
            "remote-list",
            "remote-revoke",
            "remote-role",
            "remote-status",
            "remote-log",
            "remote-doctor",
            "remote-hosts",
            "remote-rename",
            "remote-forget",
            "kiosk-status",
            "kiosk-init",
            "kiosk-issue-token",
            "kiosk-revoke-token",
            "kiosk-attach",
            "kiosk-ssh-print-config",
        ] {
            d.native_command(command, run_command);
        }
        d.blocking_services(
            |instance, context| service::invoke(instance.host().as_deref(), context),
            None,
        );
    }

    async fn deactivate(&self, _cx: &mut Deactivation) -> Result<(), PluginCommandError> {
        self.deactivate_host().await;
        Ok(())
    }
}

// The one definition both bundling and generated dynamic packaging use.
bmux_plugin_sdk::plugin!(
    ConnectionsInstance,
    create = |_| Ok(ConnectionsInstance::new(None))
);

/// Bind standalone commands to this CLI invocation, not a server runtime.
/// # Panics
/// Panics if the embedded plugin manifest is invalid.
#[must_use]
pub fn standalone_definition(client: ClientHost) -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_plugin_sdk::author::definition::<ConnectionsInstance>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |_| Ok(ConnectionsInstance::new(Some(client.clone()))),
    )
    .expect("embedded connections declaration must be valid")
}

/// The saved-host transport connector for `--target <alias>`.
#[must_use]
pub fn client_transport_connector(
    paths: &bmux_config::ConfigPaths,
) -> Arc<dyn bmux_client::TransportConnector> {
    Arc::new(connector::SavedHostConnector::new(paths.clone()))
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        super::plugin_definition()
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
