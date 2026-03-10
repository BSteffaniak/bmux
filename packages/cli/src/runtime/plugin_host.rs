use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::{
    PaneFocusDirection as IpcPaneFocusDirection, PaneSelector,
    PaneSplitDirection as IpcPaneSplitDirection, SessionRole, SessionSelector,
};
use bmux_plugin::{
    ClientQueryService, ClientSummary, ClipboardService, ConfigService, EventService,
    FollowCommandService, FollowQueryService, FollowState, HostConnectionInfo, HostMetadata,
    HostScope, PaneCommandService, PaneFocusDirection, PaneHandle, PaneQueryService, PaneRef,
    PaneSnapshot, PaneSplitDirection, PaneSummary, PermissionEntry, PersistenceCommandService,
    PersistenceQueryService, PersistenceRestorePreview, PersistenceRestoreResult,
    PersistenceStatus, PluginError, PluginEvent, PluginHost, PluginStorage, PrincipalIdentityInfo,
    RegisteredService, RenderService, ServerStatusInfo, SessionCommandService, SessionHandle,
    SessionQueryService, SessionRef, SessionRoleValue, SessionSnapshot, SessionSummary,
    WindowHandle, WindowSummary,
};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;
use uuid::Uuid;

pub struct CliPluginHost {
    plugin_id: String,
    metadata: HostMetadata,
    connection: HostConnectionInfo,
    config: BmuxConfig,
    required_capabilities: BTreeSet<HostScope>,
    provided_capabilities: BTreeSet<HostScope>,
    available_services: Vec<RegisteredService>,
}

impl CliPluginHost {
    pub fn for_plugin(
        plugin_id: impl Into<String>,
        metadata: HostMetadata,
        paths: &ConfigPaths,
        config: BmuxConfig,
        required_capabilities: BTreeSet<HostScope>,
        provided_capabilities: BTreeSet<HostScope>,
        available_services: Vec<RegisteredService>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            metadata,
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
            },
            config,
            required_capabilities,
            provided_capabilities,
            available_services,
        }
    }

    fn assert_capability(
        &self,
        capability: &str,
        operation: &'static str,
    ) -> bmux_plugin::Result<()> {
        let capability = HostScope::new(capability).expect("capability id should parse");
        if self.has_capability(&capability) {
            Ok(())
        } else {
            Err(PluginError::CapabilityAccessDenied {
                plugin_id: self.plugin_id.clone(),
                capability: capability.as_str().to_string(),
                operation,
            })
        }
    }
}

fn paths_from_connection(connection: &HostConnectionInfo) -> ConfigPaths {
    ConfigPaths::new(
        connection.config_dir.clone().into(),
        connection.runtime_dir.clone().into(),
        connection.data_dir.clone().into(),
    )
}

fn with_client<T>(
    connection: &HostConnectionInfo,
    operation: impl FnOnce(&mut BmuxClient) -> bmux_plugin::Result<T>,
) -> bmux_plugin::Result<T> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut client = BmuxClient::connect_with_paths(
                &paths_from_connection(connection),
                "bmux-plugin-host",
            )
            .await
            .map_err(|error| unsupported_operation(&format!("client connect failed: {error}")))?;
            operation(&mut client)
        })
    })
}

fn unsupported_operation(operation: &str) -> PluginError {
    PluginError::UnsupportedHostOperation {
        operation: Box::leak(operation.to_string().into_boxed_str()),
    }
}

fn map_role(role: SessionRole) -> SessionRoleValue {
    match role {
        SessionRole::Owner => SessionRoleValue::Owner,
        SessionRole::Writer => SessionRoleValue::Writer,
        SessionRole::Observer => SessionRoleValue::Observer,
    }
}

fn map_window_summary(entry: bmux_ipc::WindowSummary) -> WindowSummary {
    WindowSummary {
        handle: WindowHandle(entry.id),
        session: SessionHandle(entry.session_id),
        number: entry.number,
        name: entry.name,
        active: entry.active,
    }
}

fn map_session_ref(session: SessionRef) -> SessionSelector {
    match session {
        SessionRef::Handle(handle) => SessionSelector::ById(handle.0),
        SessionRef::Name(name) => SessionSelector::ByName(name),
    }
}

fn map_optional_session(session: Option<SessionHandle>) -> Option<SessionSelector> {
    session.map(|handle| SessionSelector::ById(handle.0))
}

fn map_pane_ref(pane: PaneRef) -> PaneSelector {
    match pane {
        PaneRef::Handle(handle) => PaneSelector::ById(handle.0),
        PaneRef::Index(index) => PaneSelector::ByIndex(index),
        PaneRef::Active => PaneSelector::Active,
    }
}

fn map_split_direction(direction: PaneSplitDirection) -> IpcPaneSplitDirection {
    match direction {
        PaneSplitDirection::Vertical => IpcPaneSplitDirection::Vertical,
        PaneSplitDirection::Horizontal => IpcPaneSplitDirection::Horizontal,
    }
}

fn map_focus_direction(direction: PaneFocusDirection) -> IpcPaneFocusDirection {
    match direction {
        PaneFocusDirection::Next => IpcPaneFocusDirection::Next,
        PaneFocusDirection::Prev => IpcPaneFocusDirection::Prev,
    }
}

fn map_session_summary(entry: bmux_ipc::SessionSummary) -> SessionSummary {
    SessionSummary {
        handle: SessionHandle(entry.id),
        name: entry.name,
        window_count: entry.window_count,
        client_count: entry.client_count,
    }
}

fn map_pane_summary(
    entry: bmux_ipc::PaneSummary,
    session: SessionHandle,
    window: WindowHandle,
) -> PaneSummary {
    PaneSummary {
        handle: PaneHandle(entry.id),
        session,
        window,
        index: entry.index,
        name: entry.name,
        focused: entry.focused,
    }
}

fn map_client_summary(entry: bmux_ipc::ClientSummary) -> ClientSummary {
    ClientSummary {
        id: entry.id,
        selected_session: entry.selected_session_id.map(SessionHandle),
        following_client_id: entry.following_client_id,
        following_global: entry.following_global,
        role: entry.session_role.map(map_role),
    }
}

impl PluginHost for CliPluginHost {
    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn metadata(&self) -> &HostMetadata {
        &self.metadata
    }

    fn connection(&self) -> &HostConnectionInfo {
        &self.connection
    }

    fn required_capabilities(&self) -> &BTreeSet<HostScope> {
        &self.required_capabilities
    }

    fn provided_capabilities(&self) -> &BTreeSet<HostScope> {
        &self.provided_capabilities
    }

    fn available_services(&self) -> &[RegisteredService] {
        &self.available_services
    }

    fn events(&self) -> &dyn EventService {
        self
    }

    fn client_queries(&self) -> &dyn ClientQueryService {
        self
    }

    fn render(&self) -> &dyn RenderService {
        self
    }

    fn config(&self) -> &dyn ConfigService {
        self
    }

    fn storage(&self) -> &dyn PluginStorage {
        self
    }

    fn clipboard(&self) -> &dyn ClipboardService {
        self
    }
}

impl EventService for CliPluginHost {
    fn emit(&self, _event: PluginEvent) -> bmux_plugin::Result<()> {
        Err(unsupported_operation("emit_event"))
    }
}

impl SessionQueryService for CliPluginHost {
    fn active_session(&self) -> bmux_plugin::Result<Option<SessionHandle>> {
        self.assert_capability("bmux.sessions.read", "session.active")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                Ok(sessions.first().map(|session| SessionHandle(session.id)))
            })
        })
    }

    fn list_sessions(&self) -> bmux_plugin::Result<Vec<SessionSummary>> {
        self.assert_capability("bmux.sessions.read", "session.list")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                Ok(sessions.into_iter().map(map_session_summary).collect())
            })
        })
    }

    fn get_session(&self, session: SessionHandle) -> bmux_plugin::Result<Option<SessionSummary>> {
        self.assert_capability("bmux.sessions.read", "session.get")?;
        self.list_sessions()
            .map(|sessions| sessions.into_iter().find(|entry| entry.handle == session))
    }

    fn snapshot_session(
        &self,
        session: SessionHandle,
    ) -> bmux_plugin::Result<Option<SessionSnapshot>> {
        self.assert_capability("bmux.sessions.read", "session.snapshot")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                let Some(session_entry) = sessions.into_iter().find(|entry| entry.id == session.0)
                else {
                    return Ok(None);
                };

                let windows = client
                    .list_windows(Some(SessionSelector::ById(session.0)))
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_windows failed: {error}"))
                    })?;
                let clients = client.list_clients().await.map_err(|error| {
                    unsupported_operation(&format!("list_clients failed: {error}"))
                })?;
                let permissions = client
                    .list_permissions(SessionSelector::ById(session.0))
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_permissions failed: {error}"))
                    })?;

                Ok(Some(SessionSnapshot {
                    session: map_session_summary(session_entry),
                    active_window: windows
                        .iter()
                        .find(|entry| entry.active)
                        .map(|entry| WindowHandle(entry.id)),
                    windows: windows.into_iter().map(map_window_summary).collect(),
                    clients: clients
                        .into_iter()
                        .filter(|entry| entry.selected_session_id == Some(session.0))
                        .map(map_client_summary)
                        .collect(),
                    permissions: permissions
                        .into_iter()
                        .map(|entry| PermissionEntry {
                            client_id: entry.client_id,
                            role: map_role(entry.role),
                        })
                        .collect(),
                }))
            })
        })
    }
}

impl SessionCommandService for CliPluginHost {
    fn create_session(&self, name: Option<String>) -> bmux_plugin::Result<SessionHandle> {
        self.assert_capability("bmux.sessions.write", "session.create")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .new_session(name)
                    .await
                    .map(SessionHandle)
                    .map_err(|error| unsupported_operation(&format!("new_session failed: {error}")))
            })
        })
    }

    fn kill_session(
        &self,
        session: SessionRef,
        force_local: bool,
    ) -> bmux_plugin::Result<SessionHandle> {
        self.assert_capability("bmux.sessions.write", "session.kill")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .kill_session_with_options(map_session_ref(session), force_local)
                    .await
                    .map(SessionHandle)
                    .map_err(|error| {
                        unsupported_operation(&format!("kill_session failed: {error}"))
                    })
            })
        })
    }
}

impl PaneQueryService for CliPluginHost {
    fn focused_pane(
        &self,
        session: Option<SessionHandle>,
    ) -> bmux_plugin::Result<Option<PaneHandle>> {
        self.assert_capability("bmux.panes.read", "pane.focused")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let session_selector = map_optional_session(session);
                let windows = client
                    .list_windows(session_selector.clone())
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_windows failed: {error}"))
                    })?;
                let Some(active_window) = windows.into_iter().find(|entry| entry.active) else {
                    return Ok(None);
                };
                let panes = client.list_panes(session_selector).await.map_err(|error| {
                    unsupported_operation(&format!("list_panes failed: {error}"))
                })?;
                Ok(panes.into_iter().find(|entry| entry.focused).map(|entry| {
                    let _ = active_window;
                    PaneHandle(entry.id)
                }))
            })
        })
    }

    fn list_panes(&self, session: Option<SessionHandle>) -> bmux_plugin::Result<Vec<PaneSummary>> {
        self.assert_capability("bmux.panes.read", "pane.list")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let session_selector = map_optional_session(session);
                let windows = client
                    .list_windows(session_selector.clone())
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_windows failed: {error}"))
                    })?;
                let Some(active_window) = windows.into_iter().find(|entry| entry.active) else {
                    return Ok(Vec::new());
                };
                let session_handle = SessionHandle(active_window.session_id);
                let window_handle = WindowHandle(active_window.id);
                client
                    .list_panes(session_selector)
                    .await
                    .map(|panes| {
                        panes
                            .into_iter()
                            .map(|entry| map_pane_summary(entry, session_handle, window_handle))
                            .collect()
                    })
                    .map_err(|error| unsupported_operation(&format!("list_panes failed: {error}")))
            })
        })
    }

    fn get_pane(&self, pane: PaneHandle) -> bmux_plugin::Result<Option<PaneSummary>> {
        self.assert_capability("bmux.panes.read", "pane.get")?;
        self.list_sessions_and_active_panes()
            .map(|panes| panes.into_iter().find(|entry| entry.handle == pane))
    }

    fn snapshot_pane(&self, pane: PaneHandle) -> bmux_plugin::Result<Option<PaneSnapshot>> {
        self.assert_capability("bmux.panes.read", "pane.snapshot")?;
        self.get_pane(pane)
            .map(|pane| pane.map(|pane| PaneSnapshot { pane }))
    }
}

impl CliPluginHost {
    fn list_sessions_and_active_panes(&self) -> bmux_plugin::Result<Vec<PaneSummary>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                let mut panes = Vec::new();
                for session in sessions {
                    let windows = client
                        .list_windows(Some(SessionSelector::ById(session.id)))
                        .await
                        .map_err(|error| {
                            unsupported_operation(&format!("list_windows failed: {error}"))
                        })?;
                    let Some(active_window) = windows.into_iter().find(|entry| entry.active) else {
                        continue;
                    };
                    let session_handle = SessionHandle(session.id);
                    let window_handle = WindowHandle(active_window.id);
                    let active_panes = client
                        .list_panes(Some(SessionSelector::ById(session.id)))
                        .await
                        .map_err(|error| {
                            unsupported_operation(&format!("list_panes failed: {error}"))
                        })?;
                    panes.extend(
                        active_panes
                            .into_iter()
                            .map(|pane| map_pane_summary(pane, session_handle, window_handle)),
                    );
                }
                Ok(panes)
            })
        })
    }
}

impl PaneCommandService for CliPluginHost {
    fn split_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: PaneSplitDirection,
    ) -> bmux_plugin::Result<PaneHandle> {
        self.assert_capability("bmux.panes.write", "pane.split")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let pane_id = match target {
                    Some(target) => {
                        client
                            .split_pane_target(
                                map_optional_session(session),
                                map_pane_ref(target),
                                map_split_direction(direction),
                            )
                            .await
                    }
                    None => {
                        client
                            .split_pane(
                                map_optional_session(session),
                                map_split_direction(direction),
                            )
                            .await
                    }
                }
                .map_err(|error| unsupported_operation(&format!("split_pane failed: {error}")))?;
                Ok(PaneHandle(pane_id))
            })
        })
    }

    fn focus_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: Option<PaneFocusDirection>,
    ) -> bmux_plugin::Result<PaneHandle> {
        self.assert_capability("bmux.panes.write", "pane.focus")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let pane_id = match (target, direction) {
                    (Some(_), Some(_)) => {
                        return Err(unsupported_operation(
                            "focus_pane with both target and direction",
                        ));
                    }
                    (Some(target), None) => {
                        client
                            .focus_pane_target(map_optional_session(session), map_pane_ref(target))
                            .await
                    }
                    (None, Some(direction)) => {
                        client
                            .focus_pane(
                                map_optional_session(session),
                                map_focus_direction(direction),
                            )
                            .await
                    }
                    (None, None) => {
                        return Err(unsupported_operation(
                            "focus_pane requires either a target or direction",
                        ));
                    }
                }
                .map_err(|error| unsupported_operation(&format!("focus_pane failed: {error}")))?;
                Ok(PaneHandle(pane_id))
            })
        })
    }

    fn resize_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        delta: i16,
    ) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.panes.write", "pane.resize")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                match target {
                    Some(target) => {
                        client
                            .resize_pane_target(
                                map_optional_session(session),
                                map_pane_ref(target),
                                delta,
                            )
                            .await
                    }
                    None => {
                        client
                            .resize_pane(map_optional_session(session), delta)
                            .await
                    }
                }
                .map_err(|error| unsupported_operation(&format!("resize_pane failed: {error}")))
            })
        })
    }

    fn close_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
    ) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.panes.write", "pane.close")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                match target {
                    Some(target) => {
                        client
                            .close_pane_target(map_optional_session(session), map_pane_ref(target))
                            .await
                    }
                    None => client.close_pane(map_optional_session(session)).await,
                }
                .map_err(|error| unsupported_operation(&format!("close_pane failed: {error}")))
            })
        })
    }
}

impl ClientQueryService for CliPluginHost {
    fn current_client_id(&self) -> bmux_plugin::Result<Uuid> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .whoami()
                    .await
                    .map_err(|error| unsupported_operation(&format!("whoami failed: {error}")))
            })
        })
    }

    fn principal_identity(&self) -> bmux_plugin::Result<PrincipalIdentityInfo> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .whoami_principal()
                    .await
                    .map(|identity| PrincipalIdentityInfo {
                        principal_id: identity.principal_id,
                        server_owner_principal_id: identity.server_owner_principal_id,
                        force_local_authorized: identity.force_local_authorized,
                    })
                    .map_err(|error| {
                        unsupported_operation(&format!("whoami_principal failed: {error}"))
                    })
            })
        })
    }

    fn list_clients(&self) -> bmux_plugin::Result<Vec<ClientSummary>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .list_clients()
                    .await
                    .map(|clients| clients.into_iter().map(map_client_summary).collect())
                    .map_err(|error| {
                        unsupported_operation(&format!("list_clients failed: {error}"))
                    })
            })
        })
    }
}

impl FollowQueryService for CliPluginHost {
    fn current_follow_state(&self) -> bmux_plugin::Result<FollowState> {
        self.assert_capability("bmux.follow.read", "follow.current")?;
        let current_client_id = self.current_client_id()?;
        let clients = self.list_clients()?;
        let current = clients
            .into_iter()
            .find(|client| client.id == current_client_id)
            .ok_or_else(|| unsupported_operation("current client missing from client list"))?;
        Ok(FollowState {
            follower_client_id: current.id,
            leader_client_id: current.following_client_id,
            global: current.following_global,
            selected_session: current.selected_session,
        })
    }
}

impl FollowCommandService for CliPluginHost {
    fn follow_client(&self, target_client_id: Uuid, global: bool) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.follow.write", "follow.start")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .follow_client(target_client_id, global)
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("follow_client failed: {error}"))
                    })
            })
        })
    }

    fn unfollow(&self) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.follow.write", "follow.stop")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .unfollow()
                    .await
                    .map_err(|error| unsupported_operation(&format!("unfollow failed: {error}")))
            })
        })
    }
}

impl PersistenceQueryService for CliPluginHost {
    fn status(&self) -> bmux_plugin::Result<PersistenceStatus> {
        self.assert_capability("bmux.persistence.read", "persistence.status")?;
        self.server_status().map(|status| status.snapshot)
    }

    fn server_status(&self) -> bmux_plugin::Result<ServerStatusInfo> {
        self.assert_capability("bmux.persistence.read", "persistence.server_status")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .server_status()
                    .await
                    .map(|status| ServerStatusInfo {
                        running: status.running,
                        principal_id: status.principal_id,
                        server_owner_principal_id: status.server_owner_principal_id,
                        snapshot: PersistenceStatus {
                            enabled: status.snapshot.enabled,
                            path: status.snapshot.path,
                            snapshot_exists: status.snapshot.snapshot_exists,
                            last_write_epoch_ms: status.snapshot.last_write_epoch_ms,
                            last_restore_epoch_ms: status.snapshot.last_restore_epoch_ms,
                            last_restore_error: status.snapshot.last_restore_error,
                        },
                    })
                    .map_err(|error| {
                        unsupported_operation(&format!("server_status failed: {error}"))
                    })
            })
        })
    }
}

impl PersistenceCommandService for CliPluginHost {
    fn save(&self) -> bmux_plugin::Result<Option<String>> {
        self.assert_capability("bmux.persistence.write", "persistence.save")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .server_save()
                    .await
                    .map_err(|error| unsupported_operation(&format!("server_save failed: {error}")))
            })
        })
    }

    fn restore_dry_run(&self) -> bmux_plugin::Result<PersistenceRestorePreview> {
        self.assert_capability("bmux.persistence.write", "persistence.restore_dry_run")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .server_restore_dry_run()
                    .await
                    .map(|(ok, message)| PersistenceRestorePreview { ok, message })
                    .map_err(|error| {
                        unsupported_operation(&format!("server_restore_dry_run failed: {error}"))
                    })
            })
        })
    }

    fn restore_apply(&self) -> bmux_plugin::Result<PersistenceRestoreResult> {
        self.assert_capability("bmux.persistence.write", "persistence.restore_apply")?;
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                client
                    .server_restore_apply()
                    .await
                    .map(|summary| PersistenceRestoreResult {
                        sessions: summary.sessions,
                        windows: summary.windows,
                        roles: summary.roles,
                        follows: summary.follows,
                        selected_sessions: summary.selected_sessions,
                    })
                    .map_err(|error| {
                        unsupported_operation(&format!("server_restore_apply failed: {error}"))
                    })
            })
        })
    }
}

impl RenderService for CliPluginHost {
    fn invalidate(&self) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.attach.overlay", "render.invalidate")?;
        Err(unsupported_operation("render_invalidate"))
    }
}

impl ConfigService for CliPluginHost {
    fn plugin_settings(&self, plugin_id: &str) -> bmux_plugin::Result<BTreeMap<String, Value>> {
        Ok(self
            .config
            .plugins
            .settings
            .get(plugin_id)
            .and_then(|value| value.as_table())
            .map(|table| {
                table
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl PluginStorage for CliPluginHost {
    fn get(&self, _plugin_id: &str, _key: &str) -> bmux_plugin::Result<Option<Vec<u8>>> {
        self.assert_capability("bmux.storage", "storage.get")?;
        Err(unsupported_operation("storage_get"))
    }

    fn set(&self, _plugin_id: &str, _key: &str, _value: Vec<u8>) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.storage", "storage.set")?;
        Err(unsupported_operation("storage_set"))
    }
}

impl ClipboardService for CliPluginHost {
    fn copy_text(&self, _text: &str) -> bmux_plugin::Result<()> {
        self.assert_capability("bmux.clipboard", "clipboard.copy")?;
        Err(unsupported_operation("clipboard_copy"))
    }
}

#[cfg(test)]
mod tests {
    use super::CliPluginHost;
    use bmux_config::{BmuxConfig, ConfigPaths};
    use bmux_plugin::{
        CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, ClipboardService, HostMetadata,
        HostScope, PersistenceQueryService, PluginHost, PluginStorage, RegisteredService,
        ServiceKind, SessionQueryService,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn service(capability: &str, kind: ServiceKind, interface_id: &str) -> RegisteredService {
        RegisteredService {
            capability: HostScope::new(capability).expect("capability should parse"),
            kind,
            interface_id: interface_id.to_string(),
            provider_plugin_id: "provider.plugin".to_string(),
        }
    }

    fn host(
        required: &[&str],
        provided: &[&str],
        services: Vec<RegisteredService>,
    ) -> CliPluginHost {
        CliPluginHost::for_plugin(
            "example.plugin",
            HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            &ConfigPaths::new(
                PathBuf::from("/tmp/config"),
                PathBuf::from("/tmp/runtime"),
                PathBuf::from("/tmp/data"),
            ),
            BmuxConfig::default(),
            required
                .iter()
                .map(|value| HostScope::new(*value).expect("capability should parse"))
                .collect::<BTreeSet<_>>(),
            provided
                .iter()
                .map(|value| HostScope::new(*value).expect("capability should parse"))
                .collect::<BTreeSet<_>>(),
            services,
        )
    }

    #[test]
    fn reports_required_and_provided_capabilities() {
        let host = host(&["bmux.sessions.read"], &["bmux.windows.write"], Vec::new());
        assert_eq!(PluginHost::plugin_id(&host), "example.plugin");
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("bmux.sessions.read").expect("capability should parse")
        ));
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("bmux.windows.write").expect("capability should parse")
        ));
    }

    #[test]
    fn session_queries_require_sessions_read_capability() {
        let host = host(&[], &[], Vec::new());
        let error = SessionQueryService::list_sessions(&host)
            .expect_err("missing capability should be rejected");
        assert!(error.to_string().contains("bmux.sessions.read"));
    }

    #[test]
    fn provider_owned_service_registration_is_resolvable() {
        let host = host(
            &[],
            &["bmux.windows.write"],
            vec![service(
                "bmux.windows.write",
                ServiceKind::Command,
                "window-command/v1",
            )],
        );
        let capability = HostScope::new("bmux.windows.write").expect("capability should parse");
        let service = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "window-command/v1",
        )
        .expect("provider-owned service should resolve");
        assert_eq!(service.interface_id, "window-command/v1");
    }

    #[test]
    fn missing_registered_service_is_rejected() {
        let host = host(&[], &["bmux.windows.write"], Vec::new());
        let capability = HostScope::new("bmux.windows.write").expect("capability should parse");
        let error = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "window-command/v1",
        )
        .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("resolve_service"));
    }

    #[test]
    fn storage_and_clipboard_checks_happen_before_unsupported_operation() {
        let host = host(&[], &[], Vec::new());
        let storage_error = PluginStorage::get(&host, "example.plugin", "key")
            .expect_err("storage should require capability");
        assert!(storage_error.to_string().contains("bmux.storage"));

        let clipboard_error = ClipboardService::copy_text(&host, "hello")
            .expect_err("clipboard should require capability");
        assert!(clipboard_error.to_string().contains("bmux.clipboard"));
    }

    #[test]
    fn persistence_queries_require_capability() {
        let host = host(&[], &[], Vec::new());
        let error = PersistenceQueryService::status(&host)
            .expect_err("persistence status should require capability");
        assert!(error.to_string().contains("bmux.persistence.read"));
    }
}
