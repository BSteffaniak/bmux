//! Client known-hosts store: alias -> pinned host key, address hints and the
//! principal this device acts as on that host. One owner-only record per
//! client profile at `state_dir/connections/known-hosts/<profile>.json`.
use crate::storage::{self, RecordStore};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownHost {
    /// Pinned host public key (Iroh endpoint id).
    pub host_key: String,
    /// Direct address hints.
    pub addrs: Vec<SocketAddr>,
    /// Relay hint.
    pub relay: Option<String>,
    /// Principal the host issued to this device.
    pub principal: Uuid,
    /// `direct` or `relay`, from the last connection.
    pub last_path: Option<String>,
    pub last_runtime: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    hosts: BTreeMap<String, KnownHost>,
}

pub struct KnownHosts {
    store: RecordStore,
}

pub fn valid_alias(alias: &str) -> Result<()> {
    ensure!(
        storage::valid_component(alias),
        "host alias must be 1-64 letters, digits, '-' or '_' and not start with '-'"
    );
    Ok(())
}

impl KnownHosts {
    pub fn open(state_dir: &Path, profile: &str) -> Result<Self> {
        ensure!(
            storage::valid_component(profile),
            "invalid client profile name"
        );
        let (_, directory) = storage::private_subdirectory(state_dir, "known-hosts")?;
        Ok(Self {
            store: RecordStore::new(directory, &format!("{profile}.json"))?,
        })
    }

    fn decode(bytes: Option<&[u8]>) -> Result<Record> {
        let Some(bytes) = bytes else {
            return Ok(Record {
                version: 1,
                hosts: BTreeMap::new(),
            });
        };
        let record: Record = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid known-hosts record"))?;
        ensure!(record.version == 1, "unsupported known-hosts version");
        Ok(record)
    }

    fn update<T>(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, KnownHost>) -> Result<T>,
    ) -> Result<T> {
        self.store.transaction(|bytes| {
            let mut record = Self::decode(bytes.as_deref())?;
            let result = f(&mut record.hosts)?;
            *bytes = Some(serde_json::to_vec(&record)?);
            Ok(result)
        })
    }

    pub fn list(&self) -> Result<BTreeMap<String, KnownHost>> {
        Ok(Self::decode(self.store.read()?.as_deref())?.hosts)
    }

    pub fn get(&self, alias: &str) -> Result<Option<KnownHost>> {
        Ok(self.list()?.remove(alias))
    }

    /// Save a newly paired host. An alias already pinned to a different host
    /// key is never overwritten silently.
    pub fn insert(&self, alias: &str, host: KnownHost) -> Result<()> {
        valid_alias(alias)?;
        self.update(|hosts| {
            if let Some(existing) = hosts.get(alias)
                && existing.host_key != host.host_key
            {
                bail!(
                    "saved host '{alias}' belongs to a different host key; choose another --name or run `bmux remote forget {alias}` first"
                );
            }
            hosts.insert(alias.to_owned(), host);
            Ok(())
        })
    }

    /// Refresh address hints and the last path after a successful connection.
    pub fn record_connection(&self, alias: &str, host_key: &str, path: &str) -> Result<()> {
        self.update(|hosts| {
            let host = hosts.get_mut(alias).context("unknown saved host")?;
            ensure!(host.host_key == host_key, "saved host key changed");
            host.last_path = Some(path.to_owned());
            Ok(())
        })
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        valid_alias(new)?;
        self.update(|hosts| {
            ensure!(
                !hosts.contains_key(new),
                "a saved host is already named '{new}'"
            );
            let host = hosts
                .remove(old)
                .with_context(|| format!("no saved host named '{old}'"))?;
            hosts.insert(new.to_owned(), host);
            Ok(())
        })
    }

    pub fn forget(&self, alias: &str) -> Result<KnownHost> {
        self.update(|hosts| {
            hosts
                .remove(alias)
                .with_context(|| format!("no saved host named '{alias}'"))
        })
    }
}

/// The error for a host that answered with a key other than the pinned one.
pub fn host_key_changed(alias: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the host saved as '{alias}' now presents a different host key. This happens when the host's key was reset, or when something else answers at its address. If the host was reset on purpose, run `bmux remote forget {alias}` and pair again with `bmux connect --pair`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str) -> KnownHost {
        KnownHost {
            host_key: key.into(),
            addrs: vec!["127.0.0.1:1".parse().unwrap()],
            relay: None,
            principal: Uuid::new_v4(),
            last_path: None,
            last_runtime: None,
        }
    }

    #[test]
    fn hosts_are_pinned_renamed_and_forgotten() {
        let root = tempfile::tempdir().unwrap();
        let hosts = KnownHosts::open(root.path(), "default").unwrap();
        hosts.insert("desk", host("k1")).unwrap();
        assert!(hosts.insert("desk", host("k2")).is_err());
        hosts.insert("desk", host("k1")).unwrap();
        assert!(hosts.insert("bad alias", host("k1")).is_err());
        hosts.rename("desk", "desktop").unwrap();
        assert!(hosts.get("desk").unwrap().is_none());
        assert_eq!(hosts.get("desktop").unwrap().unwrap().host_key, "k1");
        hosts.record_connection("desktop", "k1", "direct").unwrap();
        assert!(hosts.record_connection("desktop", "k2", "direct").is_err());
        let reopened = KnownHosts::open(root.path(), "default").unwrap();
        assert_eq!(
            reopened
                .get("desktop")
                .unwrap()
                .unwrap()
                .last_path
                .as_deref(),
            Some("direct")
        );
        assert!(
            KnownHosts::open(root.path(), "other")
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        reopened.forget("desktop").unwrap();
        assert!(reopened.forget("desktop").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.path().join("connections/known-hosts/default.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(
            host_key_changed("desk")
                .to_string()
                .contains("bmux remote forget desk")
        );
    }
}
