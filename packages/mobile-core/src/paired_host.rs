//! The mobile target model: saved paired hosts.
//!
//! A phone pairs with a host the way `bmux connect --pair` does (the host owner
//! runs `bmux remote pair` and shares its `bmux-pair:` link), and connects to a
//! saved host through the connections plugin's saved-host connector. Device key,
//! known hosts and their pinned host keys live under the app's private storage
//! directory, in the same layout desktop clients use.
use crate::error::{MobileCoreError, Result};
use bmux_client::BmuxClient;
use bmux_connections_plugin::paired_hosts::{self, PairRequest, PairedHosts, Role};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use uuid::Uuid;

/// The name a mobile client announces in its `Hello`.
pub const CLIENT_NAME: &str = "bmux-mobile";

/// The one async runtime of this process.
///
/// The connections plugin keeps one Iroh endpoint per device key for the
/// whole process, and that endpoint's tasks live on the runtime that first
/// bound it, so every mobile API object shares this runtime instead of
/// owning one.
///
/// # Panics
/// Panics if the runtime cannot start (no threads or I/O driver available).
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("bmux-mobile")
            .build()
            .expect("tokio runtime should initialize for the mobile core")
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PairedHostRole {
    Full,
    ViewOnly,
}

impl From<Role> for PairedHostRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Full => Self::Full,
            Role::ViewOnly => Self::ViewOnly,
        }
    }
}

/// A saved host. It carries no secret: the device key stays in storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedHost {
    pub alias: String,
    /// The pinned host key (Iroh endpoint id).
    pub host_key: String,
    /// The role granted at pairing. The host owner can change it later, so it
    /// is a label; the host enforces the current role.
    pub role: Option<PairedHostRole>,
    /// `direct` or `relay`, from the last connection.
    pub last_path: Option<String>,
}

impl From<paired_hosts::PairedHost> for PairedHost {
    fn from(host: paired_hosts::PairedHost) -> Self {
        Self {
            alias: host.alias,
            host_key: host.host_key,
            role: host.role.map(Into::into),
            last_path: host.last_path,
        }
    }
}

/// Its link carries the pairing secret, so `Debug` leaves it out.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRequest {
    /// The `bmux-pair:` link `bmux remote pair` printed on the host.
    pub link: String,
    /// The alias to save the host under; derived from the host's name when unset.
    pub alias: Option<String>,
    /// The label the host owner sees for this device.
    pub device_name: String,
}

impl std::fmt::Debug for PairingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingRequest")
            .field("link", &"[redacted]")
            .field("alias", &self.alias)
            .field("device_name", &self.device_name)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    /// Dialing the host and proving the link secret.
    Connecting,
    /// The host accepted the proof; `phrase` is shown on both devices while
    /// the owner decides.
    AwaitingApproval,
    /// Approved and saved; `host` is set.
    Paired,
    /// Refused, expired or unreachable; `error` is set.
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingState {
    pub id: Uuid,
    pub status: PairingStatus,
    pub phrase: Option<String>,
    pub host: Option<PairedHost>,
    pub error: Option<String>,
}

struct Pairing {
    state: Arc<Mutex<PairingState>>,
    task: tokio::task::JoinHandle<()>,
}

/// The saved hosts and the pairing in progress of one storage directory.
pub struct PairedHostStore {
    hosts: PairedHosts,
    root: PathBuf,
    pairing: Mutex<Option<Pairing>>,
}

impl std::fmt::Debug for PairedHostStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairedHostStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn store_error(error: &anyhow::Error) -> MobileCoreError {
    MobileCoreError::HostStore(format!("{error:#}"))
}

impl PairedHostStore {
    /// The store under `root`, an app-private directory. The device key and
    /// known hosts live in `root/state`; an optional `root/config/bmux.toml`
    /// holds connections plugin settings such as `remote.relay`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let paths = bmux_config::ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        );
        Self {
            hosts: PairedHosts::new(paths),
            root: root.to_path_buf(),
            pairing: Mutex::new(None),
        }
    }

    /// Every saved host, by alias.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::HostStore`] when the saved hosts cannot be read.
    pub fn list(&self) -> Result<Vec<PairedHost>> {
        self.hosts
            .list()
            .map(|hosts| hosts.into_iter().map(Into::into).collect())
            .map_err(|error| store_error(&error))
    }

    /// The saved host `alias`.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::HostNotFound`] for an unknown alias.
    pub fn get(&self, alias: &str) -> Result<PairedHost> {
        self.hosts
            .get(alias)
            .map_err(|error| store_error(&error))?
            .map(Into::into)
            .ok_or_else(|| MobileCoreError::HostNotFound(alias.to_owned()))
    }

    /// Rename a saved host.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::HostNotFound`] for an unknown alias and
    /// [`MobileCoreError::HostStore`] when the new alias is invalid or taken.
    pub fn rename(&self, old: &str, new: &str) -> Result<PairedHost> {
        self.get(old)?;
        self.hosts
            .rename(old, new)
            .map_err(|error| store_error(&error))?;
        self.get(new)
    }

    /// Forget a saved host on this device. The host keeps listing the device
    /// until its owner runs `bmux remote revoke`.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::HostNotFound`] for an unknown alias.
    pub fn forget(&self, alias: &str) -> Result<()> {
        self.get(alias)?;
        self.hosts
            .forget(alias)
            .map_err(|error| store_error(&error))
    }

    /// Start pairing through `request.link`. Pairing runs in the background:
    /// read its progress with [`Self::pairing_state`]. A pairing already in
    /// progress is cancelled first; one device pairs with one host at a time.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::InvalidPairingLink`] for a malformed link
    /// and [`MobileCoreError::HostStore`] for an invalid alias, before dialing.
    pub fn start_pairing(&self, request: PairingRequest) -> Result<PairingState> {
        paired_hosts::check_link(&request.link)
            .map_err(|error| MobileCoreError::InvalidPairingLink(format!("{error:#}")))?;
        let alias = request
            .alias
            .map(|alias| alias.trim().to_owned())
            .filter(|alias| !alias.is_empty());
        if let Some(alias) = &alias {
            paired_hosts::check_alias(alias).map_err(|error| store_error(&error))?;
        }
        let device_name = match request.device_name.trim() {
            "" => "phone".to_owned(),
            name => name.to_owned(),
        };
        let state = Arc::new(Mutex::new(PairingState {
            id: Uuid::new_v4(),
            status: PairingStatus::Connecting,
            phrase: None,
            host: None,
            error: None,
        }));
        let hosts = self.hosts.clone();
        let progress = Arc::clone(&state);
        let task = runtime().spawn(async move {
            let request = PairRequest {
                link: request.link,
                alias,
                device_name,
            };
            let phrase_state = Arc::clone(&progress);
            let outcome = hosts
                .pair(request, move |phrase| {
                    let mut state = lock(&phrase_state);
                    state.phrase = Some(phrase.to_owned());
                    state.status = PairingStatus::AwaitingApproval;
                })
                .await;
            let mut state = lock(&progress);
            match outcome {
                Ok(host) => {
                    state.status = PairingStatus::Paired;
                    state.host = Some(host.into());
                }
                Err(error) => {
                    state.status = PairingStatus::Failed;
                    state.error = Some(format!("{error:#}"));
                }
            }
        });
        let snapshot = lock(&state).clone();
        let previous = lock(&self.pairing).replace(Pairing { state, task });
        if let Some(previous) = previous {
            cancel(&previous);
        }
        Ok(snapshot)
    }

    /// The progress of pairing `id`.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::PairingNotFound`] when `id` is not the
    /// current pairing.
    pub fn pairing_state(&self, id: Uuid) -> Result<PairingState> {
        let pairing = lock(&self.pairing);
        pairing
            .as_ref()
            .map(|pairing| lock(&pairing.state).clone())
            .filter(|state| state.id == id)
            .ok_or_else(|| MobileCoreError::PairingNotFound(id.to_string()))
    }

    /// Cancel pairing `id`. Dropping the pairing connection withdraws the
    /// request on the host. A finished pairing keeps its outcome.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::PairingNotFound`] when `id` is not the
    /// current pairing.
    pub fn cancel_pairing(&self, id: Uuid) -> Result<PairingState> {
        let pairing = lock(&self.pairing);
        let current = pairing
            .as_ref()
            .filter(|pairing| lock(&pairing.state).id == id)
            .ok_or_else(|| MobileCoreError::PairingNotFound(id.to_string()))?;
        cancel(current);
        let state = lock(&current.state).clone();
        drop(pairing);
        Ok(state)
    }

    /// Open a client to saved host `alias` through the saved-host connector.
    ///
    /// # Errors
    /// Returns [`MobileCoreError::HostNotFound`] for an unknown alias and
    /// [`MobileCoreError::ConnectionFailed`] when the host is unreachable or
    /// refuses this device.
    pub async fn open_client(&self, alias: &str) -> Result<BmuxClient> {
        self.get(alias)?;
        self.hosts
            .open(alias, CLIENT_NAME)
            .await
            .map_err(|error| MobileCoreError::ConnectionFailed(format!("{error:#}")))
    }
}

impl Drop for PairedHostStore {
    fn drop(&mut self) {
        let pairing = lock(&self.pairing).take();
        if let Some(pairing) = pairing {
            cancel(&pairing);
        }
    }
}

fn cancel(pairing: &Pairing) {
    pairing.task.abort();
    let mut state = lock(&pairing.state);
    if matches!(
        state.status,
        PairingStatus::Connecting | PairingStatus::AwaitingApproval
    ) {
        state.status = PairingStatus::Cancelled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_pairing_link() {
        let request = PairingRequest {
            link: "bmux-pair:secretsecret".into(),
            alias: Some("desk".into()),
            device_name: "phone".into(),
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("secretsecret"), "{shown}");
        assert!(shown.contains("desk") && shown.contains("phone"));
    }

    #[test]
    fn empty_store_lists_nothing_and_creates_no_state() {
        let root = tempfile::tempdir().unwrap();
        let store = PairedHostStore::new(root.path());
        assert_eq!(store.list().unwrap(), Vec::new());
        assert_eq!(
            store.get("desktop"),
            Err(MobileCoreError::HostNotFound("desktop".into()))
        );
        assert_eq!(
            store.forget("desktop"),
            Err(MobileCoreError::HostNotFound("desktop".into()))
        );
        assert!(matches!(
            store.rename("desktop", "laptop"),
            Err(MobileCoreError::HostNotFound(_))
        ));
        assert!(!root.path().join("state").exists());
    }

    #[test]
    fn malformed_links_and_aliases_fail_before_dialing() {
        let root = tempfile::tempdir().unwrap();
        let store = PairedHostStore::new(root.path());
        for link in ["", "iroh://node", "ssh://me@host", "bmux-pair:zzzz"] {
            assert!(
                matches!(
                    store.start_pairing(PairingRequest {
                        link: link.into(),
                        alias: None,
                        device_name: "phone".into(),
                    }),
                    Err(MobileCoreError::InvalidPairingLink(_))
                ),
                "{link:?} must be rejected"
            );
        }
        assert!(matches!(
            store.pairing_state(Uuid::new_v4()),
            Err(MobileCoreError::PairingNotFound(_))
        ));
        assert!(!root.path().join("state").exists());
    }
}
