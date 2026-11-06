//! Server-side store for plugin-contributed status items and toasts.
//!
//! Third-party plugins (including dynamically loaded cdylibs) reach this
//! module only through the typed `status-items`, `status-items-state`, and
//! `notifications` BPDL services. Every mutation is namespaced by the
//! host-supplied caller plugin id and republishes a retained snapshot on the
//! plugin event bus; the server forwards those snapshots to attach clients,
//! where the tab-bar companion renders them.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse};
use bmux_tab_bar_plugin_api::{
    notifications::{NotificationError, client::NotifyRequest},
    notifications_list::{self, Notification, NotificationSnapshot},
    status_items::{StatusItemError, client::SetStatusItemRequest},
    status_items_list::{self, StatusItem, StatusItemSnapshot},
};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

/// Most items one plugin may keep on the bar at once.
pub const MAX_ITEMS_PER_OWNER: usize = 8;
/// Most items across all plugins.
pub const MAX_ITEMS_TOTAL: usize = 64;
/// Longest accepted item id, in bytes.
pub const MAX_ID_BYTES: usize = 64;
/// Item text is truncated to this many graphemes.
pub const MAX_TEXT_GRAPHEMES: usize = 64;
/// Tooltip text is truncated to this many graphemes.
pub const MAX_TOOLTIP_GRAPHEMES: usize = 256;
/// Retained toast queue length; the oldest toast is evicted first.
pub const NOTIFICATION_QUEUE_LIMIT: usize = 16;
/// Toast title is truncated to this many graphemes.
pub const MAX_TITLE_GRAPHEMES: usize = 80;
/// Toast body is truncated to this many graphemes.
pub const MAX_BODY_GRAPHEMES: usize = 400;
/// Lifetime used when the caller omits `ttl-ms`.
pub const DEFAULT_TTL_MS: u64 = 5_000;
/// Shortest accepted toast lifetime.
pub const MIN_TTL_MS: u64 = 500;
/// Longest accepted toast lifetime.
pub const MAX_TTL_MS: u64 = 60_000;

/// Owner recorded when the host supplies no caller plugin id.
const HOST_OWNER: &str = "bmux.core";

#[derive(Debug, Default)]
pub struct ExternalPresentationStore {
    items: BTreeMap<(String, String), StatusItem>,
    item_revision: u64,
    notifications: VecDeque<Notification>,
    notification_revision: u64,
}

impl ExternalPresentationStore {
    /// Create or replace `owner`'s item.
    pub fn set_item(
        &mut self,
        owner: &str,
        request: SetStatusItemRequest,
    ) -> Result<(), StatusItemError> {
        let id = request.id.trim();
        if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
            return Err(StatusItemError::InvalidArgument {
                reason: format!("id must be 1..={MAX_ID_BYTES} bytes without control characters"),
            });
        }
        let text = sanitize(&request.text, MAX_TEXT_GRAPHEMES);
        if text.is_empty() {
            return Err(StatusItemError::InvalidArgument {
                reason: "text must not be empty".to_string(),
            });
        }
        let key = (owner.to_string(), id.to_string());
        if !self.items.contains_key(&key) {
            let owner_item_count = self
                .items
                .keys()
                .filter(|(item_owner, _)| item_owner == owner)
                .count();
            if owner_item_count >= MAX_ITEMS_PER_OWNER {
                return Err(StatusItemError::LimitExceeded {
                    limit: limit_u32(MAX_ITEMS_PER_OWNER),
                });
            }
            if self.items.len() >= MAX_ITEMS_TOTAL {
                return Err(StatusItemError::LimitExceeded {
                    limit: limit_u32(MAX_ITEMS_TOTAL),
                });
            }
        }
        let item = StatusItem {
            owner: key.0.clone(),
            id: key.1.clone(),
            text,
            severity: request.severity,
            priority: request.priority,
            tooltip: request
                .tooltip
                .map(|tooltip| sanitize(&tooltip, MAX_TOOLTIP_GRAPHEMES))
                .filter(|tooltip| !tooltip.is_empty()),
        };
        if self.items.get(&key) != Some(&item) {
            self.items.insert(key, item);
            self.item_revision = self.item_revision.saturating_add(1);
        }
        Ok(())
    }

    /// Remove `owner`'s item `id`.
    pub fn clear_item(&mut self, owner: &str, id: &str) -> bool {
        let removed = self
            .items
            .remove(&(owner.to_string(), id.trim().to_string()))
            .is_some();
        if removed {
            self.item_revision = self.item_revision.saturating_add(1);
        }
        removed
    }

    /// Remove every item `owner` contributed.
    pub fn clear_owner(&mut self, owner: &str) -> u32 {
        let before = self.items.len();
        self.items.retain(|(item_owner, _), _| item_owner != owner);
        let removed = before - self.items.len();
        if removed > 0 {
            self.item_revision = self.item_revision.saturating_add(1);
        }
        limit_u32(removed)
    }

    /// Items in render order, optionally only those of one owner.
    pub fn list_items(&self, owner: Option<&str>) -> Vec<StatusItem> {
        let mut items = self
            .items
            .values()
            .filter(|item| owner.is_none_or(|owner| item.owner == owner))
            .cloned()
            .collect::<Vec<_>>();
        // BTreeMap iteration is already (owner, id) ordered; a stable sort
        // by descending priority keeps that tie-break deterministic.
        items.sort_by_key(|item| std::cmp::Reverse(item.priority));
        items
    }

    pub fn item_snapshot(&self) -> StatusItemSnapshot {
        StatusItemSnapshot {
            items: self.list_items(None),
            revision: self.item_revision,
        }
    }

    /// Queue a toast accepted at `now_ms`, returning its id.
    pub fn notify(
        &mut self,
        owner: &str,
        request: NotifyRequest,
        id: Uuid,
        now_ms: u64,
    ) -> Result<Uuid, NotificationError> {
        let title = sanitize(&request.title, MAX_TITLE_GRAPHEMES);
        if title.is_empty() {
            return Err(NotificationError::InvalidArgument {
                reason: "title must not be empty".to_string(),
            });
        }
        let body = request
            .body
            .map(|body| sanitize(&body, MAX_BODY_GRAPHEMES))
            .filter(|body| !body.is_empty());
        let ttl_ms = request
            .ttl_ms
            .unwrap_or(DEFAULT_TTL_MS)
            .clamp(MIN_TTL_MS, MAX_TTL_MS);
        self.prune_expired(now_ms);
        while self.notifications.len() >= NOTIFICATION_QUEUE_LIMIT {
            self.notifications.pop_front();
        }
        self.notifications.push_back(Notification {
            id,
            owner: owner.to_string(),
            title,
            body,
            severity: request.severity,
            created_at_ms: now_ms,
            ttl_ms,
        });
        self.notification_revision = self.notification_revision.saturating_add(1);
        Ok(id)
    }

    /// Dismiss one of `owner`'s toasts.
    pub fn dismiss(&mut self, owner: &str, id: Uuid) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| !(notification.id == id && notification.owner == owner));
        let removed = before != self.notifications.len();
        if removed {
            self.notification_revision = self.notification_revision.saturating_add(1);
        }
        removed
    }

    /// Drop toasts whose lifetime has elapsed at `now_ms`.
    pub fn prune_expired(&mut self, now_ms: u64) -> bool {
        let before = self.notifications.len();
        self.notifications
            .retain(|notification| !notification_expired(notification, now_ms));
        let removed = before != self.notifications.len();
        if removed {
            self.notification_revision = self.notification_revision.saturating_add(1);
        }
        removed
    }

    pub fn notification_snapshot(&self) -> NotificationSnapshot {
        NotificationSnapshot {
            notifications: self.notifications.iter().cloned().collect(),
            revision: self.notification_revision,
        }
    }
}

/// Whether `notification` is past its lifetime at `now_ms`.
pub const fn notification_expired(notification: &Notification, now_ms: u64) -> bool {
    notification
        .created_at_ms
        .saturating_add(notification.ttl_ms)
        <= now_ms
}

/// Collapse control characters to spaces, trim, and bound grapheme count.
fn sanitize(value: &str, maximum_graphemes: usize) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    cleaned
        .trim()
        .graphemes(true)
        .take(maximum_graphemes)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn limit_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Current wall-clock time in Unix epoch milliseconds.
///
/// Toast lifetimes are wall-clock based so a retained snapshot replayed to a
/// late-joining attach client does not resurrect toasts that already expired.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn store() -> &'static Mutex<ExternalPresentationStore> {
    static STORE: OnceLock<Mutex<ExternalPresentationStore>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(ExternalPresentationStore::default()))
}

/// The host-authenticated caller id used to namespace contributions.
fn caller_owner(context: &NativeServiceContext) -> &str {
    let owner = context.request.caller_plugin_id.trim();
    if owner.is_empty() { HOST_OWNER } else { owner }
}

/// Register the retained state channels this plugin publishes.
///
/// Called from activation so the server's streaming forwarders find the
/// channels before any client connects.
pub fn register_state_channels() {
    let events = bmux_plugin::global_event_bus();
    let (items, notifications) = store().lock().map_or_else(
        |_| {
            (
                StatusItemSnapshot {
                    items: Vec::new(),
                    revision: 0,
                },
                NotificationSnapshot {
                    notifications: Vec::new(),
                    revision: 0,
                },
            )
        },
        |store| (store.item_snapshot(), store.notification_snapshot()),
    );
    events.register_state_channel(status_items_list::STATE_KIND, items);
    events.register_state_channel(notifications_list::STATE_KIND, notifications);
}

// The store lock covers publication so concurrent mutations publish their
// snapshots in revision order.
#[allow(clippy::significant_drop_tightening)]
fn mutate<T>(
    apply: impl FnOnce(&mut ExternalPresentationStore) -> T,
) -> Result<T, ServiceResponse> {
    let mut store = store()
        .lock()
        .map_err(|_| ServiceResponse::error("unavailable", "tab-bar status store lock poisoned"))?;
    let item_revision = store.item_revision;
    let notification_revision = store.notification_revision;
    let result = apply(&mut store);
    let events = bmux_plugin::global_event_bus();
    if store.item_revision != item_revision
        && let Err(error) =
            events.publish_state(&status_items_list::STATE_KIND, store.item_snapshot())
    {
        tracing::debug!(%error, "tab-bar status item snapshot not published");
    }
    if store.notification_revision != notification_revision
        && let Err(error) = events.publish_state(
            &notifications_list::STATE_KIND,
            store.notification_snapshot(),
        )
    {
        tracing::debug!(%error, "tab-bar notification snapshot not published");
    }
    Ok(result)
}

pub fn set_status_item(
    request: SetStatusItemRequest,
    context: &NativeServiceContext,
) -> Result<Result<(), StatusItemError>, ServiceResponse> {
    let owner = caller_owner(context).to_string();
    mutate(|store| store.set_item(&owner, request))
}

pub fn clear_status_item(
    id: &str,
    context: &NativeServiceContext,
) -> Result<bool, ServiceResponse> {
    let owner = caller_owner(context).to_string();
    mutate(|store| store.clear_item(&owner, id))
}

pub fn clear_status_items(context: &NativeServiceContext) -> Result<u32, ServiceResponse> {
    let owner = caller_owner(context).to_string();
    mutate(|store| store.clear_owner(&owner))
}

pub fn list_status_items(owner: Option<&str>) -> Result<Vec<StatusItem>, ServiceResponse> {
    store()
        .lock()
        .map(|store| store.list_items(owner))
        .map_err(|_| ServiceResponse::error("unavailable", "tab-bar status store lock poisoned"))
}

pub fn notify(
    request: NotifyRequest,
    context: &NativeServiceContext,
) -> Result<Result<Uuid, NotificationError>, ServiceResponse> {
    let owner = caller_owner(context).to_string();
    mutate(|store| store.notify(&owner, request, Uuid::new_v4(), now_unix_ms()))
}

pub fn dismiss(id: Uuid, context: &NativeServiceContext) -> Result<bool, ServiceResponse> {
    let owner = caller_owner(context).to_string();
    mutate(|store| {
        let removed = store.dismiss(&owner, id);
        store.prune_expired(now_unix_ms());
        removed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tab_bar_plugin_api::status_items_list::Severity;

    fn item(id: &str, text: &str, priority: i32) -> SetStatusItemRequest {
        SetStatusItemRequest {
            id: id.into(),
            text: text.into(),
            severity: Severity::Normal,
            priority,
            tooltip: None,
        }
    }

    fn toast(title: &str, ttl_ms: Option<u64>) -> NotifyRequest {
        NotifyRequest {
            title: title.into(),
            body: None,
            severity: Severity::Info,
            ttl_ms,
        }
    }

    #[test]
    fn items_order_by_priority_then_owner_then_id() {
        let mut store = ExternalPresentationStore::default();
        store.set_item("b.plugin", item("x", "bx", 5)).unwrap();
        store.set_item("a.plugin", item("z", "az", 5)).unwrap();
        store.set_item("a.plugin", item("y", "ay", 5)).unwrap();
        store.set_item("c.plugin", item("hi", "high", 50)).unwrap();
        store.set_item("c.plugin", item("lo", "low", -3)).unwrap();
        let order = store
            .item_snapshot()
            .items
            .into_iter()
            .map(|item| item.text)
            .collect::<Vec<_>>();
        assert_eq!(order, ["high", "ay", "az", "bx", "low"]);
    }

    #[test]
    fn items_are_namespaced_per_owner() {
        let mut store = ExternalPresentationStore::default();
        store
            .set_item("a.plugin", item("build", "a build", 0))
            .unwrap();
        store
            .set_item("b.plugin", item("build", "b build", 0))
            .unwrap();
        assert_eq!(store.list_items(None).len(), 2);
        // Replacing is scoped to the caller.
        store
            .set_item("a.plugin", item("build", "a rebuilt", 0))
            .unwrap();
        let b = store.list_items(Some("b.plugin"));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].text, "b build");
        // Another plugin cannot clear an item it does not own.
        assert!(!store.clear_item("b.plugin", "missing"));
        assert!(store.clear_item("b.plugin", "build"));
        let remaining = store.list_items(None);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].owner, "a.plugin");
        assert_eq!(remaining[0].text, "a rebuilt");
    }

    #[test]
    fn clearing_an_owner_removes_only_its_items_and_advances_revision() {
        let mut store = ExternalPresentationStore::default();
        store.set_item("a.plugin", item("one", "1", 0)).unwrap();
        store.set_item("a.plugin", item("two", "2", 0)).unwrap();
        store.set_item("b.plugin", item("one", "b1", 0)).unwrap();
        let revision = store.item_snapshot().revision;
        assert_eq!(store.clear_owner("a.plugin"), 2);
        assert!(store.item_snapshot().revision > revision);
        assert_eq!(store.list_items(None).len(), 1);
        let revision = store.item_snapshot().revision;
        assert_eq!(store.clear_owner("a.plugin"), 0);
        assert_eq!(store.item_snapshot().revision, revision);
    }

    #[test]
    fn unchanged_set_does_not_advance_revision() {
        let mut store = ExternalPresentationStore::default();
        store.set_item("a.plugin", item("one", "1", 0)).unwrap();
        let revision = store.item_snapshot().revision;
        store.set_item("a.plugin", item("one", "1", 0)).unwrap();
        assert_eq!(store.item_snapshot().revision, revision);
    }

    #[test]
    fn items_are_validated_sanitized_and_bounded() {
        let mut store = ExternalPresentationStore::default();
        assert!(matches!(
            store.set_item("a", item("", "x", 0)),
            Err(StatusItemError::InvalidArgument { .. })
        ));
        assert!(matches!(
            store.set_item("a", item("id", " \n\t ", 0)),
            Err(StatusItemError::InvalidArgument { .. })
        ));
        assert!(matches!(
            store.set_item("a", item(&"x".repeat(MAX_ID_BYTES + 1), "x", 0)),
            Err(StatusItemError::InvalidArgument { .. })
        ));
        store
            .set_item("a", item("multi", "line\none\u{1b}[31m", 0))
            .unwrap();
        assert_eq!(store.list_items(None)[0].text, "line one [31m");
        store
            .set_item("a", item("long", &"y".repeat(200), 0))
            .unwrap();
        assert!(
            store
                .list_items(None)
                .iter()
                .all(|item| item.text.graphemes(true).count() <= MAX_TEXT_GRAPHEMES)
        );
        for index in 2..MAX_ITEMS_PER_OWNER {
            store
                .set_item("a", item(&format!("i{index}"), "x", 0))
                .unwrap();
        }
        assert_eq!(
            store.set_item("a", item("overflow", "x", 0)),
            Err(StatusItemError::LimitExceeded {
                limit: limit_u32(MAX_ITEMS_PER_OWNER)
            })
        );
        // Replacing an existing id is still allowed at the limit.
        store.set_item("a", item("long", "short", 0)).unwrap();
    }

    #[test]
    fn notifications_clamp_ttl_bound_queue_and_expire() {
        let mut store = ExternalPresentationStore::default();
        let now = 1_000_000;
        store
            .notify("a", toast("default", None), Uuid::from_u128(1), now)
            .unwrap();
        store
            .notify("a", toast("short", Some(1)), Uuid::from_u128(2), now)
            .unwrap();
        store
            .notify("a", toast("long", Some(u64::MAX)), Uuid::from_u128(3), now)
            .unwrap();
        let ttls = store
            .notification_snapshot()
            .notifications
            .iter()
            .map(|notification| notification.ttl_ms)
            .collect::<Vec<_>>();
        assert_eq!(ttls, [DEFAULT_TTL_MS, MIN_TTL_MS, MAX_TTL_MS]);

        assert!(store.prune_expired(now + MIN_TTL_MS));
        assert_eq!(store.notification_snapshot().notifications.len(), 2);
        assert!(!store.prune_expired(now + MIN_TTL_MS));

        for index in 0..(NOTIFICATION_QUEUE_LIMIT as u128 + 4) {
            store
                .notify("b", toast("flood", None), Uuid::from_u128(100 + index), now)
                .unwrap();
        }
        let snapshot = store.notification_snapshot();
        assert_eq!(snapshot.notifications.len(), NOTIFICATION_QUEUE_LIMIT);
        // Oldest entries were evicted first.
        assert_eq!(
            snapshot.notifications.last().unwrap().id,
            Uuid::from_u128(100 + NOTIFICATION_QUEUE_LIMIT as u128 + 3)
        );
        assert!(
            snapshot
                .notifications
                .iter()
                .all(|notification| notification.id != Uuid::from_u128(1))
        );
    }

    #[test]
    fn notifications_validate_and_dismiss_only_own_toasts() {
        let mut store = ExternalPresentationStore::default();
        assert!(matches!(
            store.notify("a", toast("  ", None), Uuid::from_u128(1), 0),
            Err(NotificationError::InvalidArgument { .. })
        ));
        let id = store
            .notify("a", toast("hello", None), Uuid::from_u128(9), 0)
            .unwrap();
        assert!(!store.dismiss("b", id));
        assert!(store.dismiss("a", id));
        assert!(!store.dismiss("a", id));
    }

    fn service_context(
        caller: &str,
        capability: &str,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> NativeServiceContext {
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
            ServiceKind, ServiceRequest,
        };
        NativeServiceContext {
            plugin_id: "bmux.tab_bar".into(),
            request: ServiceRequest {
                caller_plugin_id: caller.into(),
                service: RegisteredService {
                    capability: HostScope::new(capability).unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: interface.into(),
                    provider: ProviderId::Plugin("bmux.tab_bar".into()),
                },
                operation: operation.into(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: "test".into(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        }
    }

    fn invoke<Request: serde::Serialize, Response: serde::de::DeserializeOwned>(
        caller: &str,
        capability: &str,
        interface: &str,
        operation: &str,
        request: &Request,
    ) -> Response {
        use bmux_plugin_sdk::RustPlugin;
        let payload = bmux_plugin_sdk::encode_service_message(request).unwrap();
        let response = crate::TabBarPlugin.invoke_service(service_context(
            caller, capability, interface, operation, payload,
        ));
        assert!(response.error.is_none(), "{:?}", response.error);
        bmux_plugin_sdk::decode_service_message(&response.payload).unwrap()
    }

    #[test]
    fn services_namespace_by_host_caller_id_and_publish_snapshots() {
        use bmux_tab_bar_plugin_api::{notifications, status_items, status_items_state};
        register_state_channels();
        let (_, mut items_rx) = bmux_plugin::global_event_bus()
            .subscribe_state::<StatusItemSnapshot>(&status_items_list::STATE_KIND)
            .unwrap();
        let write = "bmux.tab_bar.status.write";
        for caller in ["svc.alpha", "svc.beta"] {
            let result: Result<(), StatusItemError> = invoke(
                caller,
                write,
                status_items::INTERFACE_ID.as_str(),
                "set-status-item",
                &item("shared-id", caller, 1),
            );
            result.unwrap();
        }
        assert!(items_rx.has_changed().unwrap());
        let published = items_rx.borrow_and_update().clone();
        let mine = published
            .items
            .iter()
            .filter(|item| item.id == "shared-id")
            .map(|item| (item.owner.as_str(), item.text.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(mine, [("svc.alpha", "svc.alpha"), ("svc.beta", "svc.beta")]);

        let listed: Vec<StatusItem> = invoke(
            "svc.reader",
            "bmux.tab_bar.status.read",
            status_items_state::INTERFACE_ID.as_str(),
            "list-status-items",
            &status_items_state::client::ListStatusItemsRequest {
                owner: Some("svc.beta".into()),
            },
        );
        assert_eq!(listed.len(), 1);

        let removed: bool = invoke(
            "svc.alpha",
            write,
            status_items::INTERFACE_ID.as_str(),
            "clear-status-item",
            &status_items::client::ClearStatusItemRequest {
                id: "shared-id".into(),
            },
        );
        assert!(removed);
        let cleared: u32 = invoke(
            "svc.beta",
            write,
            status_items::INTERFACE_ID.as_str(),
            "clear-status-items",
            &(),
        );
        assert_eq!(cleared, 1);
        assert!(
            items_rx
                .borrow_and_update()
                .items
                .iter()
                .all(|item| !item.owner.starts_with("svc."))
        );

        let id: Result<Uuid, NotificationError> = invoke(
            "svc.alpha",
            "bmux.tab_bar.notify",
            notifications::INTERFACE_ID.as_str(),
            "notify",
            &toast("Deploy finished", Some(10_000)),
        );
        let id = id.unwrap();
        let (snapshot, _) = bmux_plugin::global_event_bus()
            .subscribe_state::<NotificationSnapshot>(&notifications_list::STATE_KIND)
            .unwrap();
        assert!(
            snapshot
                .notifications
                .iter()
                .any(|toast| toast.id == id && toast.owner == "svc.alpha")
        );
        let dismissed: bool = invoke(
            "svc.beta",
            "bmux.tab_bar.notify",
            notifications::INTERFACE_ID.as_str(),
            "dismiss",
            &notifications::client::DismissRequest { id },
        );
        assert!(!dismissed, "another plugin must not dismiss the toast");
        let dismissed: bool = invoke(
            "svc.alpha",
            "bmux.tab_bar.notify",
            notifications::INTERFACE_ID.as_str(),
            "dismiss",
            &notifications::client::DismissRequest { id },
        );
        assert!(dismissed);
    }

    #[test]
    fn caller_owner_comes_from_host_service_context() {
        let mut context = service_context(
            "example.native",
            "bmux.tab_bar.notify",
            "notifications",
            "notify",
            Vec::new(),
        );
        assert_eq!(caller_owner(&context), "example.native");
        context.request.caller_plugin_id = String::new();
        assert_eq!(caller_owner(&context), HOST_OWNER);
    }
}
