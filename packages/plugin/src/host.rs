use crate::{HostScope, PluginError, PluginEvent, Result};
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
    fn events(&self) -> &dyn EventService;
    fn session_queries(&self) -> &dyn SessionQueryService;
    fn session_commands(&self) -> &dyn SessionCommandService;
    fn window_queries(&self) -> &dyn WindowQueryService;
    fn window_commands(&self) -> &dyn WindowCommandService;
    fn pane_queries(&self) -> &dyn PaneQueryService;
    fn pane_commands(&self) -> &dyn PaneCommandService;
    fn permission_queries(&self) -> &dyn PermissionQueryService;
    fn permission_commands(&self) -> &dyn PermissionCommandService;
    fn client_queries(&self) -> &dyn ClientQueryService;
    fn follow_queries(&self) -> &dyn FollowQueryService;
    fn follow_commands(&self) -> &dyn FollowCommandService;
    fn persistence_queries(&self) -> &dyn PersistenceQueryService;
    fn persistence_commands(&self) -> &dyn PersistenceCommandService;
    fn render(&self) -> &dyn RenderService;
    fn config(&self) -> &dyn ConfigService;
    fn storage(&self) -> &dyn PluginStorage;
    fn clipboard(&self) -> &dyn ClipboardService;
}

pub struct PluginContext<'a> {
    pub host: &'a dyn PluginHost,
    pub settings: &'a BTreeMap<String, Value>,
}

pub struct WindowsAccess<'a> {
    queries: &'a dyn WindowQueryService,
    commands: &'a dyn WindowCommandService,
}

impl<'a> WindowsAccess<'a> {
    #[must_use]
    pub fn queries(&self) -> &'a dyn WindowQueryService {
        self.queries
    }

    #[must_use]
    pub fn commands(&self) -> &'a dyn WindowCommandService {
        self.commands
    }
}

pub struct PermissionsAccess<'a> {
    queries: &'a dyn PermissionQueryService,
    commands: &'a dyn PermissionCommandService,
}

impl<'a> PermissionsAccess<'a> {
    #[must_use]
    pub fn queries(&self) -> &'a dyn PermissionQueryService {
        self.queries
    }

    #[must_use]
    pub fn commands(&self) -> &'a dyn PermissionCommandService {
        self.commands
    }
}

impl<'a> PluginContext<'a> {
    fn require_any_capability(&self, capabilities: &[&str], operation: &'static str) -> Result<()> {
        if capabilities.iter().any(|capability| {
            self.host
                .has_capability(&HostScope::new(*capability).expect("capability should parse"))
        }) {
            Ok(())
        } else {
            Err(PluginError::CapabilityAccessDenied {
                plugin_id: self.host.plugin_id().to_string(),
                capability: capabilities.join(" or "),
                operation,
            })
        }
    }

    pub fn windows(&self) -> Result<WindowsAccess<'_>> {
        self.require_any_capability(
            &["bmux.windows.read", "bmux.windows.write"],
            "plugin_context.windows",
        )?;
        Ok(WindowsAccess {
            queries: self.host.window_queries(),
            commands: self.host.window_commands(),
        })
    }

    pub fn permissions(&self) -> Result<PermissionsAccess<'_>> {
        self.require_any_capability(
            &["bmux.permissions.read", "bmux.permissions.write"],
            "plugin_context.permissions",
        )?;
        Ok(PermissionsAccess {
            queries: self.host.permission_queries(),
            commands: self.host.permission_commands(),
        })
    }
}

pub trait EventService: Send + Sync {
    fn emit(&self, event: PluginEvent) -> Result<()>;
}

pub trait SessionQueryService: Send + Sync {
    fn active_session(&self) -> Result<Option<SessionHandle>>;
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn get_session(&self, session: SessionHandle) -> Result<Option<SessionSummary>>;
    fn snapshot_session(&self, session: SessionHandle) -> Result<Option<SessionSnapshot>>;
}

pub trait SessionCommandService: Send + Sync {
    fn create_session(&self, name: Option<String>) -> Result<SessionHandle>;
    fn kill_session(&self, session: SessionRef, force_local: bool) -> Result<SessionHandle>;
}

pub trait WindowQueryService: Send + Sync {
    fn list_windows(&self, session: Option<SessionHandle>) -> Result<Vec<WindowSummary>>;
    fn get_window(&self, window: WindowHandle) -> Result<Option<WindowSummary>>;
    fn snapshot_window(&self, window: WindowHandle) -> Result<Option<WindowSnapshot>>;
}

pub trait WindowCommandService: Send + Sync {
    fn create_window(
        &self,
        session: Option<SessionHandle>,
        name: Option<String>,
    ) -> Result<WindowHandle>;
    fn kill_window(
        &self,
        session: Option<SessionHandle>,
        target: WindowRef,
        force_local: bool,
    ) -> Result<WindowHandle>;
    fn switch_window(
        &self,
        session: Option<SessionHandle>,
        target: WindowRef,
    ) -> Result<WindowHandle>;
}

pub trait PaneQueryService: Send + Sync {
    fn focused_pane(&self, session: Option<SessionHandle>) -> Result<Option<PaneHandle>>;
    fn list_panes(&self, session: Option<SessionHandle>) -> Result<Vec<PaneSummary>>;
    fn get_pane(&self, pane: PaneHandle) -> Result<Option<PaneSummary>>;
    fn snapshot_pane(&self, pane: PaneHandle) -> Result<Option<PaneSnapshot>>;
}

pub trait PaneCommandService: Send + Sync {
    fn split_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: PaneSplitDirection,
    ) -> Result<PaneHandle>;
    fn focus_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: Option<PaneFocusDirection>,
    ) -> Result<PaneHandle>;
    fn resize_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        delta: i16,
    ) -> Result<()>;
    fn close_pane(&self, session: Option<SessionHandle>, target: Option<PaneRef>) -> Result<()>;
}

pub trait PermissionQueryService: Send + Sync {
    fn list_permissions(&self, session: SessionHandle) -> Result<Vec<PermissionEntry>>;
}

pub trait PermissionCommandService: Send + Sync {
    fn grant_role(
        &self,
        session: SessionHandle,
        client_id: Uuid,
        role: SessionRoleValue,
    ) -> Result<()>;
    fn revoke_role(&self, session: SessionHandle, client_id: Uuid) -> Result<()>;
}

pub trait ClientQueryService: Send + Sync {
    fn current_client_id(&self) -> Result<Uuid>;
    fn principal_identity(&self) -> Result<PrincipalIdentityInfo>;
    fn list_clients(&self) -> Result<Vec<ClientSummary>>;
}

pub trait FollowQueryService: Send + Sync {
    fn current_follow_state(&self) -> Result<FollowState>;
}

pub trait FollowCommandService: Send + Sync {
    fn follow_client(&self, target_client_id: Uuid, global: bool) -> Result<()>;
    fn unfollow(&self) -> Result<()>;
}

pub trait PersistenceQueryService: Send + Sync {
    fn status(&self) -> Result<PersistenceStatus>;
    fn server_status(&self) -> Result<ServerStatusInfo>;
}

pub trait PersistenceCommandService: Send + Sync {
    fn save(&self) -> Result<Option<String>>;
    fn restore_dry_run(&self) -> Result<PersistenceRestorePreview>;
    fn restore_apply(&self) -> Result<PersistenceRestoreResult>;
}

pub trait RenderService: Send + Sync {
    fn invalidate(&self) -> Result<()>;
}

pub trait ConfigService: Send + Sync {
    fn plugin_settings(&self, plugin_id: &str) -> Result<BTreeMap<String, Value>>;
}

pub trait PluginStorage: Send + Sync {
    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, plugin_id: &str, key: &str, value: Vec<u8>) -> Result<()>;
}

pub trait ClipboardService: Send + Sync {
    fn copy_text(&self, text: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockWindowService;
    struct MockPermissionService;
    struct MockNoop;

    impl EventService for MockNoop {
        fn emit(&self, _event: PluginEvent) -> Result<()> {
            Ok(())
        }
    }

    impl SessionQueryService for MockNoop {
        fn active_session(&self) -> Result<Option<SessionHandle>> {
            Ok(None)
        }
        fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }
        fn get_session(&self, _session: SessionHandle) -> Result<Option<SessionSummary>> {
            Ok(None)
        }
        fn snapshot_session(&self, _session: SessionHandle) -> Result<Option<SessionSnapshot>> {
            Ok(None)
        }
    }

    impl SessionCommandService for MockNoop {
        fn create_session(&self, _name: Option<String>) -> Result<SessionHandle> {
            unreachable!()
        }
        fn kill_session(&self, _session: SessionRef, _force_local: bool) -> Result<SessionHandle> {
            unreachable!()
        }
    }

    impl WindowQueryService for MockWindowService {
        fn list_windows(&self, _session: Option<SessionHandle>) -> Result<Vec<WindowSummary>> {
            Ok(Vec::new())
        }
        fn get_window(&self, _window: WindowHandle) -> Result<Option<WindowSummary>> {
            Ok(None)
        }
        fn snapshot_window(&self, _window: WindowHandle) -> Result<Option<WindowSnapshot>> {
            Ok(None)
        }
    }

    impl WindowCommandService for MockWindowService {
        fn create_window(
            &self,
            _session: Option<SessionHandle>,
            _name: Option<String>,
        ) -> Result<WindowHandle> {
            unreachable!()
        }
        fn kill_window(
            &self,
            _session: Option<SessionHandle>,
            _target: WindowRef,
            _force_local: bool,
        ) -> Result<WindowHandle> {
            unreachable!()
        }
        fn switch_window(
            &self,
            _session: Option<SessionHandle>,
            _target: WindowRef,
        ) -> Result<WindowHandle> {
            unreachable!()
        }
    }

    impl PaneQueryService for MockNoop {
        fn focused_pane(&self, _session: Option<SessionHandle>) -> Result<Option<PaneHandle>> {
            Ok(None)
        }
        fn list_panes(&self, _session: Option<SessionHandle>) -> Result<Vec<PaneSummary>> {
            Ok(Vec::new())
        }
        fn get_pane(&self, _pane: PaneHandle) -> Result<Option<PaneSummary>> {
            Ok(None)
        }
        fn snapshot_pane(&self, _pane: PaneHandle) -> Result<Option<PaneSnapshot>> {
            Ok(None)
        }
    }

    impl PaneCommandService for MockNoop {
        fn split_pane(
            &self,
            _session: Option<SessionHandle>,
            _target: Option<PaneRef>,
            _direction: PaneSplitDirection,
        ) -> Result<PaneHandle> {
            unreachable!()
        }
        fn focus_pane(
            &self,
            _session: Option<SessionHandle>,
            _target: Option<PaneRef>,
            _direction: Option<PaneFocusDirection>,
        ) -> Result<PaneHandle> {
            unreachable!()
        }
        fn resize_pane(
            &self,
            _session: Option<SessionHandle>,
            _target: Option<PaneRef>,
            _delta: i16,
        ) -> Result<()> {
            unreachable!()
        }
        fn close_pane(
            &self,
            _session: Option<SessionHandle>,
            _target: Option<PaneRef>,
        ) -> Result<()> {
            unreachable!()
        }
    }

    impl PermissionQueryService for MockPermissionService {
        fn list_permissions(&self, _session: SessionHandle) -> Result<Vec<PermissionEntry>> {
            Ok(Vec::new())
        }
    }

    impl PermissionCommandService for MockPermissionService {
        fn grant_role(
            &self,
            _session: SessionHandle,
            _client_id: Uuid,
            _role: SessionRoleValue,
        ) -> Result<()> {
            unreachable!()
        }
        fn revoke_role(&self, _session: SessionHandle, _client_id: Uuid) -> Result<()> {
            unreachable!()
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

    impl FollowQueryService for MockNoop {
        fn current_follow_state(&self) -> Result<FollowState> {
            unreachable!()
        }
    }

    impl FollowCommandService for MockNoop {
        fn follow_client(&self, _target_client_id: Uuid, _global: bool) -> Result<()> {
            unreachable!()
        }
        fn unfollow(&self) -> Result<()> {
            unreachable!()
        }
    }

    impl PersistenceQueryService for MockNoop {
        fn status(&self) -> Result<PersistenceStatus> {
            unreachable!()
        }
        fn server_status(&self) -> Result<ServerStatusInfo> {
            unreachable!()
        }
    }

    impl PersistenceCommandService for MockNoop {
        fn save(&self) -> Result<Option<String>> {
            unreachable!()
        }
        fn restore_dry_run(&self) -> Result<PersistenceRestorePreview> {
            unreachable!()
        }
        fn restore_apply(&self) -> Result<PersistenceRestoreResult> {
            unreachable!()
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

    impl PluginStorage for MockNoop {
        fn get(&self, _plugin_id: &str, _key: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn set(&self, _plugin_id: &str, _key: &str, _value: Vec<u8>) -> Result<()> {
            Ok(())
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
        noop: MockNoop,
        windows: MockWindowService,
        permissions: MockPermissionService,
        metadata: HostMetadata,
        connection: HostConnectionInfo,
    }

    impl MockHost {
        fn new(required: &[&str], provided: &[&str]) -> Self {
            Self {
                required: required
                    .iter()
                    .map(|v| HostScope::new(*v).expect("capability should parse"))
                    .collect(),
                provided: provided
                    .iter()
                    .map(|v| HostScope::new(*v).expect("capability should parse"))
                    .collect(),
                noop: MockNoop,
                windows: MockWindowService,
                permissions: MockPermissionService,
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
        fn events(&self) -> &dyn EventService {
            &self.noop
        }
        fn session_queries(&self) -> &dyn SessionQueryService {
            &self.noop
        }
        fn session_commands(&self) -> &dyn SessionCommandService {
            &self.noop
        }
        fn window_queries(&self) -> &dyn WindowQueryService {
            &self.windows
        }
        fn window_commands(&self) -> &dyn WindowCommandService {
            &self.windows
        }
        fn pane_queries(&self) -> &dyn PaneQueryService {
            &self.noop
        }
        fn pane_commands(&self) -> &dyn PaneCommandService {
            &self.noop
        }
        fn permission_queries(&self) -> &dyn PermissionQueryService {
            &self.permissions
        }
        fn permission_commands(&self) -> &dyn PermissionCommandService {
            &self.permissions
        }
        fn client_queries(&self) -> &dyn ClientQueryService {
            &self.noop
        }
        fn follow_queries(&self) -> &dyn FollowQueryService {
            &self.noop
        }
        fn follow_commands(&self) -> &dyn FollowCommandService {
            &self.noop
        }
        fn persistence_queries(&self) -> &dyn PersistenceQueryService {
            &self.noop
        }
        fn persistence_commands(&self) -> &dyn PersistenceCommandService {
            &self.noop
        }
        fn render(&self) -> &dyn RenderService {
            &self.noop
        }
        fn config(&self) -> &dyn ConfigService {
            &self.noop
        }
        fn storage(&self) -> &dyn PluginStorage {
            &self.noop
        }
        fn clipboard(&self) -> &dyn ClipboardService {
            &self.noop
        }
    }

    #[test]
    fn windows_access_allows_required_capability() {
        let host = MockHost::new(&["bmux.windows.read"], &[]);
        let settings = BTreeMap::new();
        let context = PluginContext {
            host: &host,
            settings: &settings,
        };
        let access = context.windows().expect("windows access should succeed");
        assert_eq!(
            access
                .queries()
                .list_windows(None)
                .expect("list should work"),
            Vec::new()
        );
    }

    #[test]
    fn windows_access_allows_provider_capability() {
        let host = MockHost::new(&[], &["bmux.windows.write"]);
        let settings = BTreeMap::new();
        let context = PluginContext {
            host: &host,
            settings: &settings,
        };
        assert!(context.windows().is_ok());
    }

    #[test]
    fn permissions_access_rejects_missing_capabilities() {
        let host = MockHost::new(&[], &[]);
        let settings = BTreeMap::new();
        let context = PluginContext {
            host: &host,
            settings: &settings,
        };
        let error = match context.permissions() {
            Err(error) => error,
            Ok(_) => panic!("permissions access should fail"),
        };
        assert!(
            error
                .to_string()
                .contains("bmux.permissions.read or bmux.permissions.write")
        );
    }
}
