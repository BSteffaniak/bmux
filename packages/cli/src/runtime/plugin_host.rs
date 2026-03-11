use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_ipc::SessionRole;
use bmux_plugin::{
    ClientQueryService, ClientSummary, ClipboardService, ConfigService, EventService,
    HostConnectionInfo, HostMetadata, HostScope, PluginError, PluginEvent, PluginHost,
    PrincipalIdentityInfo, RegisteredService, RenderService, SessionHandle, SessionRoleValue,
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

    fn clipboard(&self) -> &dyn ClipboardService {
        self
    }
}

impl EventService for CliPluginHost {
    fn emit(&self, _event: PluginEvent) -> bmux_plugin::Result<()> {
        Err(unsupported_operation("emit_event"))
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
        HostScope, PluginHost, RegisteredService, ServiceKind,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn service(capability: &str, kind: ServiceKind, interface_id: &str) -> RegisteredService {
        RegisteredService {
            capability: HostScope::new(capability).expect("capability should parse"),
            kind,
            interface_id: interface_id.to_string(),
            provider: bmux_plugin::ProviderId::Plugin("provider.plugin".to_string()),
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
        let host = host(
            &["example.base.read"],
            &["example.provider.write"],
            Vec::new(),
        );
        assert_eq!(PluginHost::plugin_id(&host), "example.plugin");
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("example.base.read").expect("capability should parse")
        ));
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("example.provider.write").expect("capability should parse")
        ));
    }

    #[test]
    fn provider_owned_service_registration_is_resolvable() {
        let host = host(
            &[],
            &["example.provider.write"],
            vec![service(
                "example.provider.write",
                ServiceKind::Command,
                "provider-command/v1",
            )],
        );
        let capability = HostScope::new("example.provider.write").expect("capability should parse");
        let service = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "provider-command/v1",
        )
        .expect("provider-owned service should resolve");
        assert_eq!(service.interface_id, "provider-command/v1");
    }

    #[test]
    fn missing_registered_service_is_rejected() {
        let host = host(&[], &["example.provider.write"], Vec::new());
        let capability = HostScope::new("example.provider.write").expect("capability should parse");
        let error = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "provider-command/v1",
        )
        .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("resolve_service"));
    }

    #[test]
    fn clipboard_checks_happen_before_unsupported_operation() {
        let host = host(&[], &[], Vec::new());
        let clipboard_error = ClipboardService::copy_text(&host, "hello")
            .expect_err("clipboard should require capability");
        assert!(clipboard_error.to_string().contains("bmux.clipboard"));
    }
}
