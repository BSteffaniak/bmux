//! Saved paired hosts for clients that embed bmux instead of running its CLI.
//!
//! The mobile core pairs with a `bmux-pair:` link, lists, renames and forgets
//! saved hosts, and opens a `BmuxClient` to one, through the same code paths
//! `bmux connect --pair`, `bmux remote hosts|rename|forget` and
//! `--target <alias>` use, under one client profile of a state directory.
use crate::cli::CLIENT_PROFILE;
use crate::connector::{SavedHostConnector, client_transport};
use crate::identity::DeviceKey;
use crate::known_hosts::{KnownHost, KnownHosts, valid_alias};
use crate::link::PairingLink;
use crate::pairing::client::pair;
use crate::pairing::protocol::sanitize_name;
use crate::transport::manager::ConnectionManager;
use anyhow::Result;
use bmux_client::BmuxClient;

pub use crate::trust::Role;

/// A saved host as a client lists it. Carries no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedHost {
    pub alias: String,
    /// The pinned host key (Iroh endpoint id).
    pub host_key: String,
    /// The role granted at pairing, when the host reported one.
    pub role: Option<Role>,
    /// `direct` or `relay`, from the last connection.
    pub last_path: Option<String>,
}

impl PairedHost {
    fn new(alias: String, host: &KnownHost) -> Self {
        Self {
            alias,
            host_key: host.host_key.clone(),
            role: host.role,
            last_path: host.last_path.clone(),
        }
    }
}

/// One pairing request from a connecting device. Its link carries the
/// pairing secret, so `Debug` leaves it out.
#[derive(Clone)]
pub struct PairRequest {
    /// The `bmux-pair:` link the host printed.
    pub link: String,
    /// The alias to save the host under; derived from the host's name when unset.
    pub alias: Option<String>,
    /// The label the host owner sees for this device.
    pub device_name: String,
}

impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairRequest")
            .field("link", &"[redacted]")
            .field("alias", &self.alias)
            .field("device_name", &self.device_name)
            .finish()
    }
}

/// The saved hosts of the default client profile under `paths.state_dir`.
#[derive(Clone, Debug)]
pub struct PairedHosts {
    paths: bmux_config::ConfigPaths,
    connector: SavedHostConnector,
}

impl PairedHosts {
    #[must_use]
    pub fn new(paths: bmux_config::ConfigPaths) -> Self {
        Self {
            connector: SavedHostConnector::new(paths.clone()),
            paths,
        }
    }

    /// Every saved host, by alias.
    ///
    /// # Errors
    /// Fails when the known-hosts record cannot be read.
    pub fn list(&self) -> Result<Vec<PairedHost>> {
        if !self.connector.has_saved_hosts() {
            return Ok(Vec::new());
        }
        Ok(self
            .known_hosts()?
            .list()?
            .into_iter()
            .map(|(alias, host)| PairedHost::new(alias, &host))
            .collect())
    }

    /// The saved host `alias`, if there is one.
    ///
    /// # Errors
    /// Fails when the known-hosts record cannot be read.
    pub fn get(&self, alias: &str) -> Result<Option<PairedHost>> {
        Ok(self
            .connector
            .saved(alias)?
            .map(|host| PairedHost::new(alias.to_owned(), &host)))
    }

    /// Rename a saved host.
    ///
    /// # Errors
    /// Fails when `old` is unknown, `new` is taken or invalid.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        self.known_hosts()?.rename(old, new)
    }

    /// Forget a saved host on this device. The host keeps listing the device
    /// until its owner revokes it.
    ///
    /// # Errors
    /// Fails when `alias` is unknown.
    pub fn forget(&self, alias: &str) -> Result<()> {
        self.known_hosts()?.forget(alias).map(drop)
    }

    /// Pair through `request.link`: prove the link secret, hand the phrase
    /// both sides show to `show_phrase`, wait for the owner's decision, and
    /// save the host. Returns the saved host.
    ///
    /// # Errors
    /// Fails on an invalid link or alias, an unreachable host, a refusal, or
    /// a known-hosts write failure.
    pub async fn pair(
        &self,
        request: PairRequest,
        show_phrase: impl FnOnce(&str) + Send,
    ) -> Result<PairedHost> {
        if let Some(alias) = &request.alias {
            check_alias(alias)?;
        }
        let link = PairingLink::decode(request.link.trim())?;
        let device = DeviceKey::load_or_create(&self.paths.state_dir, CLIENT_PROFILE)?;
        let hosts = self.known_hosts()?;
        let manager = ConnectionManager::shared(device.secret(), &client_transport(&self.paths)?);
        let paired = pair(
            &manager,
            device.public(),
            &link,
            &request.device_name,
            Role::Full,
            show_phrase,
        )
        .await?;
        let saved = hosts.list()?;
        let host_key = link.host_key.to_string();
        let alias = request.alias.unwrap_or_else(|| {
            let base = sanitize_name(&paired.host_name);
            let base = if valid_alias(&base).is_ok() {
                base
            } else {
                "host".to_owned()
            };
            let free = |name: &str| saved.get(name).is_none_or(|host| host.host_key == host_key);
            if free(&base) {
                return base;
            }
            (2..1_000)
                .map(|n| format!("{base}-{n}"))
                .find(|name| free(name))
                .unwrap_or(base)
        });
        let host = KnownHost {
            host_key,
            addrs: link.addrs,
            relay: link.relay,
            principal: paired.principal,
            role: Some(paired.role),
            last_path: None,
            last_runtime: None,
        };
        hosts.insert(&alias, host.clone())?;
        Ok(PairedHost::new(alias, &host))
    }

    /// Open a client to saved host `alias` on a new stream of the shared
    /// connection to it.
    ///
    /// # Errors
    /// Fails when `alias` is unknown, the host is unreachable, or it refuses
    /// this device.
    pub async fn open(&self, alias: &str, client_name: &str) -> Result<BmuxClient> {
        self.connector.open(alias, client_name).await
    }

    fn known_hosts(&self) -> Result<KnownHosts> {
        KnownHosts::open(&self.paths.state_dir, CLIENT_PROFILE)
    }
}

/// Human name of a role, as `bmux remote list` prints it.
#[must_use]
pub const fn role_name(role: Role) -> &'static str {
    crate::host::role_name(role)
}

/// Check that `link` is a well-formed `bmux-pair:` link without dialing.
///
/// # Errors
/// Fails with the reason the link cannot be used.
pub fn check_link(link: &str) -> Result<()> {
    PairingLink::decode(link.trim()).map(drop)
}

/// Check that `alias` can name a saved host.
///
/// # Errors
/// Fails with the alias rules.
pub fn check_alias(alias: &str) -> Result<()> {
    valid_alias(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_pairing_link() {
        let request = PairRequest {
            link: "bmux-pair:secretsecret".into(),
            alias: None,
            device_name: "laptop".into(),
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("secretsecret"), "{shown}");
        assert!(shown.contains("laptop"));
    }
}
