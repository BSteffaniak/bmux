use crate::{HostScope, PluginError, PluginEvent, RegisteredService, Result, ServiceKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub product_name: String,
    pub product_version: String,
    pub plugin_api_version: crate::ApiVersion,
    pub plugin_abi_version: crate::ApiVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionHandle(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowHandle(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneHandle(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConnectionInfo {
    pub config_dir: String,
    pub runtime_dir: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRoleValue {
    Owner,
    Writer,
    Observer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub handle: SessionHandle,
    pub name: Option<String>,
    pub window_count: usize,
    pub client_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub handle: WindowHandle,
    pub session: SessionHandle,
    pub number: u32,
    pub name: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub handle: PaneHandle,
    pub session: SessionHandle,
    pub window: WindowHandle,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub pane: PaneSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayoutNode {
    Leaf {
        pane: PaneHandle,
    },
    Split {
        direction: PaneSplitDirection,
        ratio_percent: u8,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub window: WindowSummary,
    pub focused_pane: Option<PaneHandle>,
    pub panes: Vec<PaneSummary>,
    #[serde(default)]
    pub layout_root: Option<PaneLayoutNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session: SessionSummary,
    pub active_window: Option<WindowHandle>,
    pub windows: Vec<WindowSummary>,
    pub clients: Vec<ClientSummary>,
    pub permissions: Vec<PermissionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEntry {
    pub client_id: Uuid,
    pub role: SessionRoleValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSummary {
    pub id: Uuid,
    pub selected_session: Option<SessionHandle>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
    pub role: Option<SessionRoleValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowState {
    pub follower_client_id: Uuid,
    pub leader_client_id: Option<Uuid>,
    pub global: bool,
    pub selected_session: Option<SessionHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceStatus {
    pub enabled: bool,
    pub path: Option<String>,
    pub snapshot_exists: bool,
    pub last_write_epoch_ms: Option<u64>,
    pub last_restore_epoch_ms: Option<u64>,
    pub last_restore_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceRestorePreview {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceRestoreResult {
    pub sessions: usize,
    pub windows: usize,
    pub roles: usize,
    pub follows: usize,
    pub selected_sessions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalIdentityInfo {
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub force_local_authorized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusInfo {
    pub running: bool,
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub snapshot: PersistenceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRef {
    Handle(SessionHandle),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowRef {
    Handle(WindowHandle),
    Number(u32),
    Name(String),
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneRef {
    Handle(PaneHandle),
    Index(u32),
    Active,
}

pub trait PluginHost: Send + Sync {
    fn plugin_id(&self) -> &str;
    fn metadata(&self) -> &HostMetadata;
    fn connection(&self) -> &HostConnectionInfo;
    fn required_capabilities(&self) -> &BTreeSet<HostScope>;
    fn provided_capabilities(&self) -> &BTreeSet<HostScope>;
    fn has_capability(&self, capability: &HostScope) -> bool {
        self.required_capabilities().contains(capability)
            || self.provided_capabilities().contains(capability)
    }
    fn available_services(&self) -> &[RegisteredService];
    fn resolve_service(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Result<&RegisteredService> {
        if !self.has_capability(capability) {
            return Err(PluginError::CapabilityAccessDenied {
                plugin_id: self.plugin_id().to_string(),
                capability: capability.as_str().to_string(),
                operation: "resolve_service",
            });
        }

        self.available_services()
            .iter()
            .find(|service| {
                &service.capability == capability
                    && service.kind == kind
                    && service.interface_id == interface_id
            })
            .ok_or_else(|| PluginError::UnsupportedHostOperation {
                operation: "resolve_service",
            })
    }
    fn events(&self) -> &dyn EventService;
    fn client_queries(&self) -> &dyn ClientQueryService;
    fn render(&self) -> &dyn RenderService;
    fn config(&self) -> &dyn ConfigService;
    fn clipboard(&self) -> &dyn ClipboardService;
}

pub struct PluginContext<'a> {
    pub host: &'a dyn PluginHost,
    pub settings: &'a BTreeMap<String, Value>,
}

pub trait EventService: Send + Sync {
    fn emit(&self, event: PluginEvent) -> Result<()>;
}

pub trait ClientQueryService: Send + Sync {
    fn current_client_id(&self) -> Result<Uuid>;
    fn principal_identity(&self) -> Result<PrincipalIdentityInfo>;
    fn list_clients(&self) -> Result<Vec<ClientSummary>>;
}

pub trait RenderService: Send + Sync {
    fn invalidate(&self) -> Result<()>;
}

pub trait ConfigService: Send + Sync {
    fn plugin_settings(&self, plugin_id: &str) -> Result<BTreeMap<String, Value>>;
}

pub trait ClipboardService: Send + Sync {
    fn copy_text(&self, text: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockNoop;

    impl EventService for MockNoop {
        fn emit(&self, _event: PluginEvent) -> Result<()> {
            Ok(())
        }
    }

    impl ClientQueryService for MockNoop {
        fn current_client_id(&self) -> Result<Uuid> {
            Ok(Uuid::nil())
        }
        fn principal_identity(&self) -> Result<PrincipalIdentityInfo> {
            unreachable!()
        }
        fn list_clients(&self) -> Result<Vec<ClientSummary>> {
            Ok(Vec::new())
        }
    }

    impl RenderService for MockNoop {
        fn invalidate(&self) -> Result<()> {
            Ok(())
        }
    }

    impl ConfigService for MockNoop {
        fn plugin_settings(&self, _plugin_id: &str) -> Result<BTreeMap<String, Value>> {
            Ok(BTreeMap::new())
        }
    }

    impl ClipboardService for MockNoop {
        fn copy_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }
    }

    struct MockHost {
        required: BTreeSet<HostScope>,
        provided: BTreeSet<HostScope>,
        services: Vec<RegisteredService>,
        noop: MockNoop,
        metadata: HostMetadata,
        connection: HostConnectionInfo,
    }

    impl MockHost {
        fn new(required: &[&str], provided: &[&str], services: Vec<RegisteredService>) -> Self {
            Self {
                required: required
                    .iter()
                    .map(|v| HostScope::new(*v).expect("capability should parse"))
                    .collect(),
                provided: provided
                    .iter()
                    .map(|v| HostScope::new(*v).expect("capability should parse"))
                    .collect(),
                services,
                noop: MockNoop,
                metadata: HostMetadata {
                    product_name: "bmux".to_string(),
                    product_version: "0.1.0".to_string(),
                    plugin_api_version: crate::CURRENT_PLUGIN_API_VERSION,
                    plugin_abi_version: crate::CURRENT_PLUGIN_ABI_VERSION,
                },
                connection: HostConnectionInfo {
                    config_dir: "/config".to_string(),
                    runtime_dir: "/runtime".to_string(),
                    data_dir: "/data".to_string(),
                },
            }
        }
    }

    impl PluginHost for MockHost {
        fn plugin_id(&self) -> &str {
            "example.plugin"
        }
        fn metadata(&self) -> &HostMetadata {
            &self.metadata
        }
        fn connection(&self) -> &HostConnectionInfo {
            &self.connection
        }
        fn required_capabilities(&self) -> &BTreeSet<HostScope> {
            &self.required
        }
        fn provided_capabilities(&self) -> &BTreeSet<HostScope> {
            &self.provided
        }
        fn available_services(&self) -> &[RegisteredService] {
            &self.services
        }
        fn events(&self) -> &dyn EventService {
            &self.noop
        }
        fn client_queries(&self) -> &dyn ClientQueryService {
            &self.noop
        }
        fn render(&self) -> &dyn RenderService {
            &self.noop
        }
        fn config(&self) -> &dyn ConfigService {
            &self.noop
        }
        fn clipboard(&self) -> &dyn ClipboardService {
            &self.noop
        }
    }

    #[test]
    fn resolve_service_allows_required_capability() {
        let capability = HostScope::new("bmux.windows.read").expect("capability should parse");
        let host = MockHost::new(
            &["bmux.windows.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Query,
                interface_id: "window-query/v1".to_string(),
                provider: crate::ProviderId::Plugin("bmux.windows".to_string()),
            }],
        );
        let service =
            PluginHost::resolve_service(&host, &capability, ServiceKind::Query, "window-query/v1")
                .expect("service should resolve");
        assert_eq!(service.provider.to_string(), "bmux.windows");
    }

    #[test]
    fn resolve_service_allows_provider_capability() {
        let capability = HostScope::new("bmux.windows.write").expect("capability should parse");
        let host = MockHost::new(
            &[],
            &["bmux.windows.write"],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Command,
                interface_id: "window-command/v1".to_string(),
                provider: crate::ProviderId::Plugin("bmux.windows".to_string()),
            }],
        );
        assert!(
            PluginHost::resolve_service(
                &host,
                &capability,
                ServiceKind::Command,
                "window-command/v1",
            )
            .is_ok()
        );
    }

    #[test]
    fn resolve_service_rejects_missing_capability() {
        let capability = HostScope::new("bmux.permissions.read").expect("capability should parse");
        let host = MockHost::new(
            &[],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
                provider: crate::ProviderId::Plugin("bmux.permissions".to_string()),
            }],
        );
        let error = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Query,
            "permission-query/v1",
        )
        .expect_err("missing capability should fail");
        assert!(error.to_string().contains("bmux.permissions.read"));
    }

    #[test]
    fn resolve_service_rejects_missing_registration() {
        let capability = HostScope::new("bmux.windows.read").expect("capability should parse");
        let host = MockHost::new(&["bmux.windows.read"], &[], Vec::new());
        let error =
            PluginHost::resolve_service(&host, &capability, ServiceKind::Query, "window-query/v1")
                .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("resolve_service"));
    }
}
