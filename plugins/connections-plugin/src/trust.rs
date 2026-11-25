//! Host trust store: device key -> principal, name, state and revision.
//!
//! The in-memory map is the authority the connection path reads (no file IO,
//! no locks beyond a short `RwLock`). Every change is written through to redb
//! before it becomes visible, and published to subscribers (the listener's
//! live-connection registry).
use anyhow::{Context, Result, bail};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::RwLock;
use uuid::Uuid;

const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices");
/// The audit log (see `crate::audit`), keyed by a growing sequence number.
const AUDIT: TableDefinition<u64, &[u8]> = TableDefinition::new("audit");
const EVENT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceState {
    Pending,
    Active,
    Revoked,
}

/// Remote role of a device's principal (held by the permissions plugin).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Full,
    ViewOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// Device public key (Iroh endpoint id).
    pub key: String,
    pub principal: Uuid,
    /// Untrusted display label chosen at approval.
    pub name: String,
    pub state: DeviceState,
    /// Role last granted through this store (the permissions plugin is authoritative).
    pub role: Role,
    pub revision: u64,
    pub approved_at: Option<u64>,
    pub last_seen: Option<u64>,
}

/// Published after a change is durable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustChange {
    /// The device must not connect any more.
    Revoked { key: String },
    /// The device's role changed; live connections keep running and every
    /// later request is checked against the new role.
    RoleChanged { key: String },
    /// A device became active.
    Activated { key: String },
}

pub struct TrustStore {
    database: Database,
    devices: RwLock<BTreeMap<String, Device>>,
    events: tokio::sync::broadcast::Sender<TrustChange>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl TrustStore {
    /// Open `directory/trust.redb`. The directory must be owner-only.
    pub fn open(directory: &Path) -> Result<Self> {
        let path = directory.join("trust.redb");
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .context("create trust store")?;
        }
        let database = Database::create(&path).context("open trust store")?;
        let mut devices = BTreeMap::new();
        {
            let write = database.begin_write()?;
            write.open_table(DEVICES)?;
            write.open_table(AUDIT)?;
            write.commit()?;
            let read = database.begin_read()?;
            let table = read.open_table(DEVICES)?;
            for entry in table.iter()? {
                let (key, value) = entry?;
                let device: Device = serde_json::from_slice(value.value())
                    .map_err(|_| anyhow::anyhow!("invalid trust store record"))?;
                if device.key != key.value() {
                    bail!("trust store record key mismatch");
                }
                devices.insert(device.key.clone(), device);
            }
        }
        let (events, _) = tokio::sync::broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            database,
            devices: RwLock::new(devices),
            events,
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TrustChange> {
        self.events.subscribe()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Device>> {
        self.devices
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Persist `device` (or remove `key`), then update memory under the same lock.
    fn commit(&self, key: &str, device: Option<Device>) -> Result<()> {
        let mut devices = self
            .devices
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(DEVICES)?;
            if let Some(device) = &device {
                table.insert(key, serde_json::to_vec(device)?.as_slice())?;
            } else {
                table.remove(key)?;
            }
        }
        write.commit()?;
        match device {
            Some(device) => devices.insert(key.to_owned(), device),
            None => devices.remove(key),
        };
        drop(devices);
        Ok(())
    }

    fn publish(&self, change: TrustChange) {
        // No subscriber is fine (no listener running).
        let _ = self.events.send(change);
    }

    /// The principal of an active device, the only check on the connection path.
    pub fn active_principal(&self, key: &str) -> Option<Uuid> {
        self.read()
            .get(key)
            .filter(|device| device.state == DeviceState::Active)
            .map(|device| device.principal)
    }

    pub fn get(&self, key: &str) -> Option<Device> {
        self.read().get(key).cloned()
    }

    pub fn list(&self) -> Vec<Device> {
        self.read().values().cloned().collect()
    }

    /// Resolve a device by exact name, principal, or a unique key prefix (>= 8 chars).
    pub fn find(&self, selector: &str) -> Result<Device> {
        let devices = self.list();
        let named: Vec<_> = devices
            .iter()
            .filter(|device| {
                device.name == selector
                    || device.principal.to_string() == selector
                    || device.key == selector
            })
            .collect();
        if let [device] = named.as_slice() {
            return Ok((*device).clone());
        }
        if named.len() > 1 {
            bail!("several devices match '{selector}'; use the device key");
        }
        let prefixed: Vec<_> = devices
            .iter()
            .filter(|device| selector.len() >= 8 && device.key.starts_with(selector))
            .collect();
        match prefixed.as_slice() {
            [device] => Ok((*device).clone()),
            [] => bail!("no paired device matches '{selector}'"),
            _ => bail!("several devices match '{selector}'; use more of the key"),
        }
    }

    /// Step 1 of approval: write the device `pending` with a fresh principal.
    pub fn stage_pending(&self, key: &str, name: &str, role: Role) -> Result<Device> {
        if let Some(existing) = self.get(key)
            && existing.state == DeviceState::Active
        {
            bail!("this device is already paired; revoke it first to pair again");
        }
        if self.read().values().any(|device| {
            device.name == name && device.key != key && device.state != DeviceState::Revoked
        }) {
            bail!("a paired device is already named '{name}'");
        }
        let revision = self.get(key).map_or(1, |device| device.revision + 1);
        let device = Device {
            key: key.to_owned(),
            principal: Uuid::new_v4(),
            name: name.to_owned(),
            state: DeviceState::Pending,
            role,
            revision,
            approved_at: None,
            last_seen: None,
        };
        self.commit(key, Some(device.clone()))?;
        Ok(device)
    }

    /// Step 3 of approval: the grant is committed, so the device becomes usable.
    pub fn activate(&self, key: &str) -> Result<Device> {
        let mut device = self.get(key).context("pending device disappeared")?;
        if device.state != DeviceState::Pending {
            bail!("device is not pending");
        }
        device.state = DeviceState::Active;
        device.revision += 1;
        device.approved_at = Some(now_secs());
        self.commit(key, Some(device.clone()))?;
        self.publish(TrustChange::Activated {
            key: key.to_owned(),
        });
        Ok(device)
    }

    /// Drop a pending device whose approval did not complete.
    pub fn discard_pending(&self, key: &str) -> Result<()> {
        if self
            .get(key)
            .is_some_and(|device| device.state == DeviceState::Pending)
        {
            self.commit(key, None)?;
        }
        Ok(())
    }

    /// Revoke: the device can no longer connect and live connections close.
    pub fn revoke(&self, key: &str) -> Result<Device> {
        let mut device = self.get(key).context("unknown device")?;
        device.state = DeviceState::Revoked;
        device.revision += 1;
        self.commit(key, Some(device.clone()))?;
        self.publish(TrustChange::Revoked {
            key: key.to_owned(),
        });
        Ok(device)
    }

    /// Record a committed role change and publish it.
    pub fn set_role(&self, key: &str, role: Role) -> Result<Device> {
        let mut device = self.get(key).context("unknown device")?;
        if device.state != DeviceState::Active {
            bail!("device is not active");
        }
        device.role = role;
        device.revision += 1;
        self.commit(key, Some(device.clone()))?;
        self.publish(TrustChange::RoleChanged {
            key: key.to_owned(),
        });
        Ok(device)
    }

    /// Note a successful connection (best effort).
    pub fn touch(&self, key: &str) {
        if let Some(mut device) = self.get(key) {
            device.last_seen = Some(now_secs());
            let _ = self.commit(key, Some(device));
        }
    }

    /// Append `event` to the audit log, dropping the oldest entries beyond
    /// [`crate::audit::CAPACITY`]. Best effort: a failure is logged only.
    pub fn record(&self, event: &crate::audit::Event) {
        if let Err(error) = self.append(event) {
            tracing::warn!("audit log write failed: {error:#}");
        }
    }

    fn append(&self, event: &crate::audit::Event) -> Result<()> {
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(AUDIT)?;
            let next = table.last()?.map_or(0, |(key, _)| key.value() + 1);
            table.insert(next, serde_json::to_vec(event)?.as_slice())?;
            while table.len()? > crate::audit::CAPACITY {
                if table.pop_first()?.is_none() {
                    break;
                }
            }
        }
        write.commit()?;
        Ok(())
    }

    /// The newest `limit` audit events, oldest first. Unreadable entries
    /// are skipped.
    pub fn audit(&self, limit: usize) -> Result<Vec<crate::audit::Event>> {
        let read = self.database.begin_read()?;
        let table = read.open_table(AUDIT)?;
        let mut events = Vec::new();
        for entry in table.iter()?.rev().take(limit) {
            let (_, value) = entry?;
            if let Ok(event) = serde_json::from_slice(value.value()) {
                events.push(event);
            }
        }
        events.reverse();
        Ok(events)
    }

    /// Finish approvals interrupted between their steps: a pending device whose
    /// grant exists becomes active; one without a grant is removed.
    pub fn recover(&self, grant_exists: impl Fn(Uuid) -> Result<bool>) -> Result<()> {
        let pending: Vec<_> = self
            .list()
            .into_iter()
            .filter(|device| device.state == DeviceState::Pending)
            .collect();
        for device in pending {
            if grant_exists(device.principal)? {
                self.activate(&device.key)?;
            } else {
                self.discard_pending(&device.key)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        iroh::SecretKey::from_bytes(&[n; 32]).public().to_string()
    }

    #[test]
    fn approval_transition_persists_and_reloads() {
        let root = tempfile::tempdir().unwrap();
        let store = TrustStore::open(root.path()).unwrap();
        let pending = store.stage_pending(&key(1), "laptop", Role::Full).unwrap();
        assert_eq!(pending.state, DeviceState::Pending);
        assert_eq!(store.active_principal(&key(1)), None);
        let active = store.activate(&key(1)).unwrap();
        assert_eq!(store.active_principal(&key(1)), Some(pending.principal));
        assert!(active.approved_at.is_some());
        drop(store);
        let reopened = TrustStore::open(root.path()).unwrap();
        assert_eq!(reopened.get(&key(1)).unwrap(), active);
        assert_eq!(reopened.find("laptop").unwrap().key, key(1));
        assert_eq!(reopened.find(&key(1)[..10]).unwrap().key, key(1));
        assert!(reopened.find("nope").is_err());
        assert!(
            reopened
                .stage_pending(&key(1), "again", Role::Full)
                .is_err()
        );
        assert!(
            reopened
                .stage_pending(&key(2), "laptop", Role::Full)
                .is_err()
        );
    }

    #[test]
    fn recovery_completes_or_discards_interrupted_approvals() {
        let root = tempfile::tempdir().unwrap();
        let store = TrustStore::open(root.path()).unwrap();
        let granted = store.stage_pending(&key(1), "a", Role::Full).unwrap();
        store.stage_pending(&key(2), "b", Role::ViewOnly).unwrap();
        drop(store);
        let store = TrustStore::open(root.path()).unwrap();
        store
            .recover(|principal| Ok(principal == granted.principal))
            .unwrap();
        assert_eq!(store.active_principal(&key(1)), Some(granted.principal));
        assert!(store.get(&key(2)).is_none());
    }

    #[test]
    fn audit_log_is_bounded_and_persists() {
        use crate::audit::{CAPACITY, Event, Kind};
        let root = tempfile::tempdir().unwrap();
        let store = TrustStore::open(root.path()).unwrap();
        assert_eq!(store.audit(10).unwrap(), Vec::new());
        for n in 0..CAPACITY + 5 {
            store.record(
                &Event::new(Kind::Connected)
                    .key(&key(1))
                    .detail(n.to_string()),
            );
        }
        drop(store);
        let store = TrustStore::open(root.path()).unwrap();
        let all = store.audit(usize::MAX).unwrap();
        assert_eq!(all.len() as u64, CAPACITY);
        assert_eq!(all.first().unwrap().detail.as_deref(), Some("5"));
        let newest = store.audit(2).unwrap();
        let details: Vec<_> = newest.iter().map(|e| e.detail.clone().unwrap()).collect();
        assert_eq!(
            details,
            [(CAPACITY + 3).to_string(), (CAPACITY + 4).to_string()]
        );
    }

    #[test]
    fn revocation_and_role_changes_publish_events() {
        let root = tempfile::tempdir().unwrap();
        let store = TrustStore::open(root.path()).unwrap();
        let mut events = store.subscribe();
        store.stage_pending(&key(1), "a", Role::Full).unwrap();
        store.activate(&key(1)).unwrap();
        assert_eq!(
            events.try_recv().unwrap(),
            TrustChange::Activated { key: key(1) }
        );
        let changed = store.set_role(&key(1), Role::ViewOnly).unwrap();
        assert_eq!(changed.role, Role::ViewOnly);
        assert_eq!(
            events.try_recv().unwrap(),
            TrustChange::RoleChanged { key: key(1) }
        );
        let revoked = store.revoke(&key(1)).unwrap();
        assert!(revoked.revision > changed.revision);
        assert_eq!(
            events.try_recv().unwrap(),
            TrustChange::Revoked { key: key(1) }
        );
        assert_eq!(store.active_principal(&key(1)), None);
        drop(store);
        let store = TrustStore::open(root.path()).unwrap();
        assert_eq!(store.get(&key(1)).unwrap().state, DeviceState::Revoked);
        // A revoked device may pair again, as a new principal.
        let again = store.stage_pending(&key(1), "a", Role::Full).unwrap();
        assert_ne!(again.principal, revoked.principal);
    }
}
