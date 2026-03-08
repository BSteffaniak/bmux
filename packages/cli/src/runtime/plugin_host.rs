use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::{SessionRole, SessionSelector};
use bmux_plugin::{
    ClientInfo, ClientService, ClipboardService, CommandService, ConfigService, EventService,
    HostConnectionInfo, HostMetadata, PaneHandle, PaneInfo, PaneService, PermissionEntry,
    PermissionService, PluginError, PluginEvent, PluginHost, PluginStorage, RenderService,
    ServerService, ServerStatusInfo, SessionHandle, SessionInfo, SessionService, WindowHandle,
    WindowInfo, WindowService,
};
use std::collections::BTreeMap;
use toml::Value;

pub struct CliPluginHost {
    metadata: HostMetadata,
    connection: HostConnectionInfo,
    config: BmuxConfig,
}

impl CliPluginHost {
    pub fn new(metadata: HostMetadata, paths: &ConfigPaths, config: BmuxConfig) -> Self {
        Self {
            metadata,
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
            },
            config,
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

fn session_role_name(role: SessionRole) -> String {
    match role {
        SessionRole::Owner => "owner",
        SessionRole::Writer => "writer",
        SessionRole::Observer => "observer",
    }
    .to_string()
}

impl PluginHost for CliPluginHost {
    fn metadata(&self) -> &HostMetadata {
        &self.metadata
    }

    fn connection(&self) -> &HostConnectionInfo {
        &self.connection
    }

    fn events(&self) -> &dyn EventService {
        self
    }

    fn commands(&self) -> &dyn CommandService {
        self
    }

    fn sessions(&self) -> &dyn SessionService {
        self
    }

    fn windows(&self) -> &dyn WindowService {
        self
    }

    fn panes(&self) -> &dyn PaneService {
        self
    }

    fn permissions(&self) -> &dyn PermissionService {
        self
    }

    fn clients(&self) -> &dyn ClientService {
        self
    }

    fn server(&self) -> &dyn ServerService {
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

impl CommandService for CliPluginHost {
    fn invoke(&self, _command_name: &str, _arguments: &[String]) -> bmux_plugin::Result<()> {
        Err(unsupported_operation("invoke_command"))
    }
}

impl SessionService for CliPluginHost {
    fn active_session(&self) -> bmux_plugin::Result<Option<SessionHandle>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                Ok(sessions.first().map(|session| SessionHandle(session.id)))
            })
        })
    }

    fn list_sessions(&self) -> bmux_plugin::Result<Vec<SessionHandle>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                Ok(sessions
                    .into_iter()
                    .map(|session| SessionHandle(session.id))
                    .collect())
            })
        })
    }

    fn get_session(&self, session: SessionHandle) -> bmux_plugin::Result<Option<SessionInfo>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                Ok(sessions
                    .into_iter()
                    .find(|entry| entry.id == session.0)
                    .map(|entry| SessionInfo {
                        handle: SessionHandle(entry.id),
                        name: entry.name,
                        window_count: entry.window_count,
                        client_count: entry.client_count,
                    }))
            })
        })
    }
}

impl WindowService for CliPluginHost {
    fn list_windows(&self, session: SessionHandle) -> bmux_plugin::Result<Vec<WindowHandle>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let windows = client
                    .list_windows(Some(SessionSelector::ById(session.0)))
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_windows failed: {error}"))
                    })?;
                Ok(windows
                    .into_iter()
                    .map(|window| WindowHandle(window.id))
                    .collect())
            })
        })
    }

    fn get_window(&self, window: WindowHandle) -> bmux_plugin::Result<Option<WindowInfo>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let sessions = client.list_sessions().await.map_err(|error| {
                    unsupported_operation(&format!("list_sessions failed: {error}"))
                })?;
                for session in sessions {
                    let windows = client
                        .list_windows(Some(SessionSelector::ById(session.id)))
                        .await
                        .map_err(|error| {
                            unsupported_operation(&format!("list_windows failed: {error}"))
                        })?;
                    if let Some(entry) = windows.into_iter().find(|entry| entry.id == window.0) {
                        return Ok(Some(WindowInfo {
                            handle: WindowHandle(entry.id),
                            session: SessionHandle(entry.session_id),
                            number: entry.number,
                            name: entry.name,
                            active: entry.active,
                        }));
                    }
                }
                Ok(None)
            })
        })
    }
}

impl PaneService for CliPluginHost {
    fn focused_pane(&self) -> bmux_plugin::Result<Option<PaneHandle>> {
        Err(unsupported_operation("focused_pane"))
    }

    fn list_panes(&self, _window: WindowHandle) -> bmux_plugin::Result<Vec<PaneHandle>> {
        Err(unsupported_operation("list_panes"))
    }

    fn get_pane(&self, _pane: PaneHandle) -> bmux_plugin::Result<Option<PaneInfo>> {
        Err(unsupported_operation("get_pane"))
    }
}

impl PermissionService for CliPluginHost {
    fn list_permissions(
        &self,
        session: SessionHandle,
    ) -> bmux_plugin::Result<Vec<PermissionEntry>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let permissions = client
                    .list_permissions(SessionSelector::ById(session.0))
                    .await
                    .map_err(|error| {
                        unsupported_operation(&format!("list_permissions failed: {error}"))
                    })?;
                Ok(permissions
                    .into_iter()
                    .map(|entry| PermissionEntry {
                        client_id: entry.client_id,
                        role: session_role_name(entry.role),
                    })
                    .collect())
            })
        })
    }
}

impl ClientService for CliPluginHost {
    fn list_clients(&self) -> bmux_plugin::Result<Vec<ClientInfo>> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let clients = client.list_clients().await.map_err(|error| {
                    unsupported_operation(&format!("list_clients failed: {error}"))
                })?;
                Ok(clients
                    .into_iter()
                    .map(|entry| ClientInfo {
                        id: entry.id,
                        selected_session: entry.selected_session_id.map(SessionHandle),
                        following_client_id: entry.following_client_id,
                        following_global: entry.following_global,
                        role: entry.session_role.map(session_role_name),
                    })
                    .collect())
            })
        })
    }
}

impl ServerService for CliPluginHost {
    fn status(&self) -> bmux_plugin::Result<ServerStatusInfo> {
        with_client(&self.connection, |client| {
            tokio::runtime::Handle::current().block_on(async {
                let status = client.server_status().await.map_err(|error| {
                    unsupported_operation(&format!("server_status failed: {error}"))
                })?;
                Ok(ServerStatusInfo {
                    running: status.running,
                    principal_id: status.principal_id,
                    server_owner_principal_id: status.server_owner_principal_id,
                    snapshot_enabled: status.snapshot.enabled,
                    snapshot_exists: status.snapshot.snapshot_exists,
                })
            })
        })
    }
}

impl RenderService for CliPluginHost {
    fn invalidate(&self) -> bmux_plugin::Result<()> {
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
        Err(unsupported_operation("storage_get"))
    }

    fn set(&self, _plugin_id: &str, _key: &str, _value: Vec<u8>) -> bmux_plugin::Result<()> {
        Err(unsupported_operation("storage_set"))
    }
}

impl ClipboardService for CliPluginHost {
    fn copy_text(&self, _text: &str) -> bmux_plugin::Result<()> {
        Err(unsupported_operation("clipboard_copy"))
    }
}
