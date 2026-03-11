use crate::{
    Result, ServiceCaller, ServiceKind, SessionRoleValue, decode_service_message,
    encode_service_message,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct PermissionsClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
}

impl<'a, C: ServiceCaller + ?Sized> PermissionsClient<'a, C> {
    #[must_use]
    pub const fn new(caller: &'a C) -> Self {
        Self { caller }
    }

    pub fn list(&self, session: impl Into<String>) -> Result<Vec<PermissionRecord>> {
        let request = PermissionListRequest {
            session: session.into(),
        };
        let response: PermissionListResponse = self.caller.call_service(
            "bmux.permissions.read",
            ServiceKind::Query,
            "permission-query/v1",
            "list",
            &request,
        )?;
        Ok(response.permissions)
    }

    pub fn grant(
        &self,
        session: impl Into<String>,
        client_id: Uuid,
        role: SessionRoleValue,
    ) -> Result<PermissionGrantResponse> {
        self.caller.call_service(
            "bmux.permissions.write",
            ServiceKind::Command,
            "permission-command/v1",
            "grant",
            &PermissionGrantRequest {
                session: session.into(),
                client_id,
                role,
            },
        )
    }

    pub fn revoke(
        &self,
        session: impl Into<String>,
        client_id: Uuid,
    ) -> Result<PermissionRevokeResponse> {
        self.caller.call_service(
            "bmux.permissions.write",
            ServiceKind::Command,
            "permission-command/v1",
            "revoke",
            &PermissionRevokeRequest {
                session: session.into(),
                client_id,
            },
        )
    }
}

pub struct WindowsClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
}

impl<'a, C: ServiceCaller + ?Sized> WindowsClient<'a, C> {
    #[must_use]
    pub const fn new(caller: &'a C) -> Self {
        Self { caller }
    }

    pub fn list(&self, session: Option<impl Into<String>>) -> Result<Vec<WindowRecord>> {
        let response: ListWindowsResponse = self.caller.call_service(
            "bmux.windows.read",
            ServiceKind::Query,
            "window-query/v1",
            "list",
            &ListWindowsRequest {
                session: session.map(Into::into),
            },
        )?;
        Ok(response.windows)
    }

    pub fn new_window(
        &self,
        session: Option<impl Into<String>>,
        name: Option<String>,
    ) -> Result<NewWindowResponse> {
        self.caller.call_service(
            "bmux.windows.write",
            ServiceKind::Command,
            "window-command/v1",
            "new",
            &NewWindowRequest {
                session: session.map(Into::into),
                name,
            },
        )
    }
}

pub struct ConfigClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
}

impl<'a, C: ServiceCaller + ?Sized> ConfigClient<'a, C> {
    #[must_use]
    pub const fn new(caller: &'a C) -> Self {
        Self { caller }
    }

    pub fn plugin_settings(&self, plugin_id: impl Into<String>) -> Result<PluginSettingsResponse> {
        self.caller.call_service(
            "bmux.config.read",
            ServiceKind::Query,
            "config-query/v1",
            "plugin_settings",
            &PluginSettingsRequest {
                plugin_id: plugin_id.into(),
            },
        )
    }
}

pub struct StorageClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
}

impl<'a, C: ServiceCaller + ?Sized> StorageClient<'a, C> {
    #[must_use]
    pub const fn new(caller: &'a C) -> Self {
        Self { caller }
    }

    pub fn get(&self, key: impl Into<String>) -> Result<StorageGetResponse> {
        self.caller.call_service(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            &StorageGetRequest { key: key.into() },
        )
    }

    pub fn set(&self, key: impl Into<String>, value: Vec<u8>) -> Result<()> {
        let payload = encode_service_message(&StorageSetRequest {
            key: key.into(),
            value,
        })?;
        let response = self.caller.call_service_raw(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            payload,
        )?;
        let _: () = decode_service_message(&response)?;
        Ok(())
    }
}

pub trait ServiceCallerExt: ServiceCaller {
    fn permissions(&self) -> PermissionsClient<'_, Self>
    where
        Self: Sized,
    {
        PermissionsClient::new(self)
    }

    fn windows(&self) -> WindowsClient<'_, Self>
    where
        Self: Sized,
    {
        WindowsClient::new(self)
    }

    fn config_client(&self) -> ConfigClient<'_, Self>
    where
        Self: Sized,
    {
        ConfigClient::new(self)
    }

    fn storage(&self) -> StorageClient<'_, Self>
    where
        Self: Sized,
    {
        StorageClient::new(self)
    }
}

impl<T: ServiceCaller + ?Sized> ServiceCallerExt for T {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecord {
    pub client_id: Uuid,
    pub role: SessionRoleValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrantResponse {
    pub client_id: Uuid,
    pub role: SessionRoleValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRevokeResponse {
    pub client_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub number: u32,
    pub name: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewWindowResponse {
    pub window: WindowRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSettingsResponse {
    pub settings: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageGetResponse {
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionListRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionListResponse {
    permissions: Vec<PermissionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionGrantRequest {
    session: String,
    client_id: Uuid,
    role: SessionRoleValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionRevokeRequest {
    session: String,
    client_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsRequest {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsResponse {
    windows: Vec<WindowRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    session: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginSettingsRequest {
    plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageGetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageSetRequest {
    key: String,
    value: Vec<u8>,
}
