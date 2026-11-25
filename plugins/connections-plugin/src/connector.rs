//! The saved-host transport connector (D1): `--target <alias>` for an alias in
//! this client's known hosts opens a `BmuxClient` on a stream of the shared
//! QUIC connection to that host.
use crate::cli::CLIENT_PROFILE;
use crate::config::RemoteSettings;
use crate::identity::DeviceKey;
use crate::known_hosts::{KnownHost, KnownHosts, host_key_changed};
use crate::transport::manager::{ClientTransport, ConnectionManager, HostRoute, Refused, refused};
use anyhow::{Context, Result};
use bmux_client::{BmuxClient, ClientError, TransportConnectFuture, TransportConnector};
use std::path::PathBuf;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct SavedHostConnector {
    paths: bmux_config::ConfigPaths,
}

impl SavedHostConnector {
    pub const fn new(paths: bmux_config::ConfigPaths) -> Self {
        Self { paths }
    }

    fn known_hosts_file(&self) -> PathBuf {
        self.paths
            .state_dir
            .join("connections")
            .join("known-hosts")
            .join(format!("{CLIENT_PROFILE}.json"))
    }

    fn saved(&self, alias: &str) -> Result<Option<KnownHost>> {
        // Never create client state just to answer `supports`.
        if !self.known_hosts_file().is_file() {
            return Ok(None);
        }
        KnownHosts::open(&self.paths.state_dir, CLIENT_PROFILE)?.get(alias)
    }

    /// Open a client for saved host `alias`.
    pub async fn open(&self, alias: &str, client_name: &str) -> Result<BmuxClient> {
        let known = self
            .saved(alias)?
            .with_context(|| format!("no saved host named '{alias}'"))?;
        let host_key: iroh::PublicKey = known
            .host_key
            .parse()
            .map_err(|_| anyhow::anyhow!("saved host '{alias}' has an invalid host key"))?;
        let device = DeviceKey::load_or_create(&self.paths.state_dir, CLIENT_PROFILE)?;
        let manager = ConnectionManager::shared(device.secret(), &client_transport(&self.paths)?);
        let route = HostRoute {
            host_key,
            addrs: known.addrs.clone(),
            relay: known.relay.clone(),
        };
        let (stream, path) = manager
            .open_stream(&route, CONNECT_TIMEOUT)
            .await
            .map_err(|error| explain(alias, error))?;
        let client = match BmuxClient::connect_with_bridge_stream(
            stream,
            HELLO_TIMEOUT,
            client_name.to_owned(),
            known.principal,
        )
        .await
        {
            Ok(client) => client,
            Err(error) => {
                // The host closes refused connections right after the handshake.
                if let Ok(connection) = manager.connection(&route, CONNECT_TIMEOUT).await {
                    let _ = tokio::time::timeout(Duration::from_secs(1), connection.closed()).await;
                    if let Some(refused) = refused(&connection) {
                        return Err(anyhow::Error::new(refused).context(alias.to_owned()));
                    }
                }
                return Err(anyhow::Error::new(error).context(format!("{alias}: handshake failed")));
            }
        };
        if let Ok(hosts) = KnownHosts::open(&self.paths.state_dir, CLIENT_PROFILE) {
            let _ = hosts.record_connection(alias, &known.host_key, path);
        }
        Ok(client)
    }
}

/// Turn a dial failure into a distinct, actionable message.
pub fn explain(alias: &str, error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    if text.contains("invalid peer certificate") {
        return host_key_changed(alias);
    }
    if text.contains("did not answer") {
        return anyhow::anyhow!(
            "{alias}: the host did not answer. Check that it is online and has remote access enabled (`bmux remote status` on the host)"
        );
    }
    error.context(format!("{alias}: could not connect"))
}

/// This client's endpoint settings (`remote.relay`, `remote.bind_addr`),
/// from the connections plugin settings.
pub fn client_transport(paths: &bmux_config::ConfigPaths) -> Result<ClientTransport> {
    let config = bmux_config::BmuxConfig::load_isolated_with_paths_and_overrides(
        paths,
        &bmux_config::ConfigLoadOverrides::default(),
    )?;
    let settings = RemoteSettings::load(config.plugins.settings.get("bmux.connections"))?;
    Ok(ClientTransport {
        relay: settings.relay,
        bind_addr: settings.bind_addr,
    })
}

impl TransportConnector for SavedHostConnector {
    fn supports(&self, target: &str) -> bool {
        crate::storage::valid_component(target) && matches!(self.saved(target), Ok(Some(_)))
    }

    fn connect<'a>(
        &'a self,
        target: &'a str,
        client_name: &'static str,
    ) -> TransportConnectFuture<'a> {
        Box::pin(async move {
            self.open(target, client_name).await.map_err(|error| {
                // A host that refuses this device (revoked, never paired) is
                // `PermissionDenied`: retrying cannot succeed, so callers such
                // as a resuming attach stop instead of dialing again.
                let kind = if error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<Refused>())
                    .any(Refused::is_permanent)
                {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::Other
                };
                ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(
                    std::io::Error::new(kind, format!("{error:#}")),
                ))
            })
        })
    }
}
