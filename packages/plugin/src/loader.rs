use crate::{
    CapabilityProvider, DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_COMMAND_SYMBOL,
    DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL, DEFAULT_NATIVE_DEACTIVATE_SYMBOL,
    DEFAULT_NATIVE_EVENT_SYMBOL, DEFAULT_NATIVE_SERVICE_SYMBOL, HostConnectionInfo, HostMetadata,
    HostScope, PluginDeclaration, PluginEntrypoint, PluginError, PluginEvent, PluginFeature,
    PluginLifecycle, PluginManifestCompatibility, PluginRegistry, PluginService, RegisteredPlugin,
    RegisteredService, Result, ServiceEnvelopeKind, ServiceKind, ServiceRequest, ServiceResponse,
    decode_service_envelope, decode_service_message, discover_registered_plugins_in_roots,
    encode_service_envelope, encode_service_message,
};
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};

type NativeDescriptorFn = unsafe extern "C" fn() -> *const c_char;
type NativeRunCommandFn = unsafe extern "C" fn(*const c_char, usize, *const *const c_char) -> i32;
type NativeRunCommandWithContextFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeLifecycleFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeEventFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeInvokeServiceFn =
    unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32;

const NATIVE_SERVICE_STATUS_OK: i32 = 0;
const NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLifecycleContext {
    pub plugin_id: String,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    #[serde(default)]
    pub services: Vec<crate::RegisteredService>,
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    pub host: HostMetadata,
    pub connection: HostConnectionInfo,
    #[serde(default)]
    pub settings: Option<toml::Value>,
    #[serde(default)]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeCommandContext {
    pub plugin_id: String,
    pub command: String,
    pub arguments: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    #[serde(default)]
    pub services: Vec<crate::RegisteredService>,
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    pub host: HostMetadata,
    pub connection: HostConnectionInfo,
    #[serde(default)]
    pub settings: Option<toml::Value>,
    #[serde(default)]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeServiceContext {
    pub plugin_id: String,
    pub request: ServiceRequest,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub provided_capabilities: Vec<String>,
    #[serde(default)]
    pub services: Vec<RegisteredService>,
    #[serde(default)]
    pub available_capabilities: Vec<String>,
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub plugin_search_roots: Vec<String>,
    pub host: HostMetadata,
    pub connection: HostConnectionInfo,
    #[serde(default)]
    pub settings: Option<toml::Value>,
    #[serde(default)]
    pub plugin_settings_map: BTreeMap<String, toml::Value>,
}

impl NativeCommandContext {
    pub fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }

    pub fn call_service<Request, Response>(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let payload = encode_service_message(request)?;
        let response = self.call_service_raw(capability, kind, interface_id, operation, payload)?;
        decode_service_message(&response)
    }
}

impl NativeLifecycleContext {
    pub fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}

impl NativeServiceContext {
    pub fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}

fn call_service_raw(
    caller_plugin_id: &str,
    required_capabilities: &[String],
    provided_capabilities: &[String],
    services: &[RegisteredService],
    available_capabilities: &[String],
    enabled_plugins: &[String],
    plugin_search_roots: &[String],
    host: &HostMetadata,
    connection: &HostConnectionInfo,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
    capability: &str,
    kind: ServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let capability = HostScope::new(capability)?;
    let allowed = required_capabilities
        .iter()
        .chain(provided_capabilities.iter())
        .filter_map(|value| HostScope::new(value).ok())
        .any(|entry| entry == capability);
    if !allowed {
        return Err(PluginError::CapabilityAccessDenied {
            plugin_id: caller_plugin_id.to_string(),
            capability: capability.as_str().to_string(),
            operation: "call_service",
        });
    }

    let service = services
        .iter()
        .find(|service| {
            service.capability == capability
                && service.kind == kind
                && service.interface_id == interface_id
        })
        .cloned()
        .ok_or_else(|| PluginError::UnsupportedHostOperation {
            operation: "call_service",
        })?;

    let search_roots = plugin_search_roots
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let registry = discover_registered_plugins_in_roots(&search_roots)?;
    let registered = registry.get(&service.provider_plugin_id).ok_or_else(|| {
        PluginError::MissingServiceProvider {
            provider_plugin_id: service.provider_plugin_id.clone(),
            capability: service.capability.as_str().to_string(),
            interface_id: service.interface_id.clone(),
        }
    })?;

    let available_capability_map = available_capabilities
        .iter()
        .filter_map(|value| HostScope::new(value).ok())
        .map(|capability| {
            let provider = CapabilityProvider {
                capability: capability.clone(),
                provider_plugin_id: "available".to_string(),
            };
            (capability, provider)
        })
        .collect::<BTreeMap<_, _>>();

    let loaded = load_registered_plugin(registered, host, &available_capability_map)?;
    let response = loaded.invoke_service(&NativeServiceContext {
        plugin_id: registered.declaration.id.as_str().to_string(),
        request: ServiceRequest {
            caller_plugin_id: caller_plugin_id.to_string(),
            service: service.clone(),
            operation: operation.to_string(),
            payload,
        },
        required_capabilities: registered
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: registered
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: services.to_vec(),
        available_capabilities: available_capabilities.to_vec(),
        enabled_plugins: enabled_plugins.to_vec(),
        plugin_search_roots: plugin_search_roots.to_vec(),
        host: host.clone(),
        connection: connection.clone(),
        settings: plugin_settings_map
            .get(registered.declaration.id.as_str())
            .cloned(),
        plugin_settings_map: plugin_settings_map.clone(),
    })?;

    if let Some(error) = response.error {
        return Err(PluginError::ServiceInvocationFailed {
            provider_plugin_id: service.provider_plugin_id,
            capability: service.capability.as_str().to_string(),
            interface_id: service.interface_id,
            operation: operation.to_string(),
            code: error.code,
            message: error.message,
        });
    }

    Ok(response.payload)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeDescriptor {
    pub id: String,
    pub display_name: String,
    pub plugin_version: String,
    pub plugin_api: PluginManifestCompatibility,
    pub native_abi: PluginManifestCompatibility,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    #[serde(alias = "required_host_scopes")]
    pub required_capabilities: BTreeSet<HostScope>,
    #[serde(default)]
    pub provided_capabilities: BTreeSet<HostScope>,
    #[serde(default)]
    pub provided_features: BTreeSet<PluginFeature>,
    #[serde(default)]
    pub services: Vec<PluginService>,
    #[serde(default)]
    pub commands: Vec<crate::PluginCommand>,
    #[serde(default)]
    pub event_subscriptions: Vec<crate::PluginEventSubscription>,
    #[serde(default)]
    pub dependencies: Vec<crate::PluginDependency>,
    #[serde(default)]
    pub lifecycle: PluginLifecycle,
}

impl NativeDescriptor {
    /// # Errors
    ///
    /// Returns an error when the descriptor cannot be converted into a checked
    /// plugin declaration.
    pub fn into_declaration(self, entrypoint: PluginEntrypoint) -> Result<PluginDeclaration> {
        let plugin_id = self.id.clone();
        let declaration = PluginDeclaration {
            id: crate::PluginId::new(self.id)?,
            display_name: self.display_name,
            plugin_version: self.plugin_version,
            plugin_api: self.plugin_api.to_version_range().map_err(|details| {
                PluginError::InvalidVersionRange {
                    plugin_id: plugin_id.clone(),
                    field: "plugin_api",
                    details,
                }
            })?,
            native_abi: self.native_abi.to_version_range().map_err(|details| {
                PluginError::InvalidVersionRange {
                    plugin_id: plugin_id.clone(),
                    field: "native_abi",
                    details,
                }
            })?,
            entrypoint,
            description: self.description,
            homepage: self.homepage,
            required_capabilities: self.required_capabilities,
            provided_capabilities: self.provided_capabilities,
            provided_features: self.provided_features,
            services: self.services,
            commands: self.commands,
            event_subscriptions: self.event_subscriptions,
            dependencies: self.dependencies,
            lifecycle: self.lifecycle,
        };
        declaration.validate()?;
        Ok(declaration)
    }

    /// # Errors
    ///
    /// Returns an error when the descriptor text cannot be parsed.
    pub fn from_toml_str(value: &str) -> std::result::Result<Self, toml::de::Error> {
        toml::from_str(value)
    }

    /// # Errors
    ///
    /// Returns an error when the descriptor cannot be encoded as TOML.
    pub fn to_toml_string(&self) -> std::result::Result<String, toml::ser::Error> {
        toml::to_string(self)
    }
}

pub struct LoadedPlugin {
    pub registered: RegisteredPlugin,
    pub declaration: PluginDeclaration,
    _library: Library,
}

impl LoadedPlugin {
    #[must_use]
    pub fn commands(&self) -> &[crate::PluginCommand] {
        &self.declaration.commands
    }

    #[must_use]
    pub fn supports_command(&self, command_name: &str) -> bool {
        self.declaration
            .commands
            .iter()
            .any(|command| command.name == command_name)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    pub fn run_command(&self, command_name: &str, arguments: &[String]) -> Result<i32> {
        self.run_command_with_context(command_name, arguments, None)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    pub fn run_command_with_context(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<i32> {
        if !self.supports_command(command_name) {
            return Err(PluginError::UnknownPluginCommand {
                plugin_id: self.declaration.id.as_str().to_string(),
                command: command_name.to_string(),
            });
        }

        if let Some(context) = context {
            let payload = CString::new(
                serde_json::to_string(context).expect("native command context should serialize"),
            )
            .map_err(|_| PluginError::InvalidNativeCommandInput {
                plugin_id: self.declaration.id.as_str().to_string(),
                field: "context",
            })?;

            if let Ok(command_symbol) = unsafe {
                self._library.get::<NativeRunCommandWithContextFn>(
                    DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL.as_bytes(),
                )
            } {
                return Ok(unsafe { command_symbol(payload.as_ptr()) });
            }
        }

        let command_name =
            CString::new(command_name).map_err(|_| PluginError::InvalidNativeCommandInput {
                plugin_id: self.declaration.id.as_str().to_string(),
                field: "command_name",
            })?;
        let argument_values = arguments
            .iter()
            .map(|argument| {
                CString::new(argument.as_str()).map_err(|_| {
                    PluginError::InvalidNativeCommandInput {
                        plugin_id: self.declaration.id.as_str().to_string(),
                        field: "arguments",
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let argument_ptrs = argument_values
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();

        let command_symbol: Symbol<'_, NativeRunCommandFn> =
            unsafe { self._library.get(DEFAULT_NATIVE_COMMAND_SYMBOL.as_bytes()) }.map_err(
                |error| PluginError::NativeCommandSymbol {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: DEFAULT_NATIVE_COMMAND_SYMBOL.to_string(),
                    details: error.to_string(),
                },
            )?;

        Ok(unsafe {
            command_symbol(
                command_name.as_ptr(),
                argument_ptrs.len(),
                argument_ptrs.as_ptr(),
            )
        })
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn activate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_ACTIVATE_SYMBOL, context)
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn deactivate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_DEACTIVATE_SYMBOL, context)
    }

    #[must_use]
    pub fn receives_event(&self, event: &PluginEvent) -> bool {
        self.declaration.event_subscriptions.is_empty()
            || self
                .declaration
                .event_subscriptions
                .iter()
                .any(|subscription| subscription.matches(event))
    }

    /// # Errors
    ///
    /// Returns an error when the event symbol cannot be loaded or the event
    /// payload cannot be encoded.
    pub fn dispatch_event(&self, event: &PluginEvent) -> Result<Option<i32>> {
        if !self.receives_event(event) {
            return Ok(None);
        }

        let payload = CString::new(
            serde_json::to_string(event).expect("plugin event payload should serialize"),
        )
        .map_err(|_| PluginError::InvalidNativeEventInput {
            plugin_id: self.declaration.id.as_str().to_string(),
        })?;

        let event_symbol: Symbol<'_, NativeEventFn> =
            unsafe { self._library.get(DEFAULT_NATIVE_EVENT_SYMBOL.as_bytes()) }.map_err(
                |error| PluginError::NativeEventSymbol {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: DEFAULT_NATIVE_EVENT_SYMBOL.to_string(),
                    details: error.to_string(),
                },
            )?;

        Ok(Some(unsafe { event_symbol(payload.as_ptr()) }))
    }

    /// # Errors
    ///
    /// Returns an error when the service symbol cannot be loaded, the service
    /// payload cannot be encoded, or the plugin returns invalid transport data.
    pub fn invoke_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        let payload = encode_service_envelope(0, ServiceEnvelopeKind::Request, context)?;
        let service_symbol: Symbol<'_, NativeInvokeServiceFn> =
            unsafe { self._library.get(DEFAULT_NATIVE_SERVICE_SYMBOL.as_bytes()) }.map_err(
                |error| PluginError::NativeServiceSymbol {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: DEFAULT_NATIVE_SERVICE_SYMBOL.to_string(),
                    details: error.to_string(),
                },
            )?;

        let mut output = vec![0_u8; 4096];
        let mut output_len = 0_usize;
        let mut status = unsafe {
            service_symbol(
                payload.as_ptr(),
                payload.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut output_len,
            )
        };
        if status == NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL {
            output.resize(output_len.max(output.len() * 2), 0);
            status = unsafe {
                service_symbol(
                    payload.as_ptr(),
                    payload.len(),
                    output.as_mut_ptr(),
                    output.len(),
                    &mut output_len,
                )
            };
        }

        if status != NATIVE_SERVICE_STATUS_OK {
            return Err(PluginError::NativeServiceInvocation {
                plugin_id: self.declaration.id.as_str().to_string(),
                status,
            });
        }

        if output_len > output.len() {
            return Err(PluginError::InvalidNativeServiceOutput {
                plugin_id: self.declaration.id.as_str().to_string(),
                details: format!(
                    "service returned {output_len} bytes into {} byte buffer",
                    output.len(),
                ),
            });
        }
        output.truncate(output_len);

        let (_, response) =
            decode_service_envelope::<ServiceResponse>(&output, ServiceEnvelopeKind::Response)?;
        Ok(response)
    }

    fn run_lifecycle_symbol(&self, symbol: &str, context: &NativeLifecycleContext) -> Result<i32> {
        let payload = CString::new(
            serde_json::to_string(context).expect("native lifecycle context should serialize"),
        )
        .map_err(|_| PluginError::InvalidNativeLifecycleInput {
            plugin_id: self.declaration.id.as_str().to_string(),
        })?;

        let lifecycle_symbol: Symbol<'_, NativeLifecycleFn> = unsafe {
            self._library.get(symbol.as_bytes())
        }
        .map_err(|error| PluginError::NativeLifecycleSymbol {
            plugin_id: self.declaration.id.as_str().to_string(),
            symbol: symbol.to_string(),
            details: error.to_string(),
        })?;

        Ok(unsafe { lifecycle_symbol(payload.as_ptr()) })
    }
}

#[derive(Debug, Default)]
pub struct NativePluginLoader;

impl NativePluginLoader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible, missing, fails to load,
    /// or returns a descriptor that conflicts with its manifest.
    pub fn load_registered_plugin(
        &self,
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
    ) -> Result<LoadedPlugin> {
        PluginRegistry::validate_registered_plugin(
            registered_plugin,
            host,
            available_capabilities,
        )?;

        let entry_path = registered_plugin.manifest.resolve_entry_path(
            registered_plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        );
        let library = unsafe { Library::new(&entry_path) }.map_err(|error| {
            PluginError::NativeLibraryLoad {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                path: entry_path.clone(),
                details: error.to_string(),
            }
        })?;

        let declaration = load_native_declaration(&library, registered_plugin)?;
        PluginRegistry::validate_registered_plugin(
            &RegisteredPlugin {
                declaration: declaration.clone(),
                ..registered_plugin.clone()
            },
            host,
            available_capabilities,
        )?;
        compare_manifest_and_descriptor(registered_plugin, &declaration)?;

        Ok(LoadedPlugin {
            registered: registered_plugin.clone(),
            declaration,
            _library: library,
        })
    }
}

/// # Errors
///
/// Returns an error when the plugin cannot be loaded.
pub fn load_registered_plugin(
    registered_plugin: &RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    NativePluginLoader::new().load_registered_plugin(
        registered_plugin,
        host,
        available_capabilities,
    )
}

fn load_native_declaration(
    library: &Library,
    registered_plugin: &RegisteredPlugin,
) -> Result<PluginDeclaration> {
    let symbol_name = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.as_bytes(),
    };

    let descriptor_symbol: Symbol<'_, NativeDescriptorFn> = unsafe { library.get(symbol_name) }
        .map_err(|error| PluginError::NativeEntrySymbol {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: match &registered_plugin.declaration.entrypoint {
                PluginEntrypoint::Native { symbol } => symbol.clone(),
            },
            details: error.to_string(),
        })?;

    let descriptor_ptr = unsafe { descriptor_symbol() };
    let symbol = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.clone(),
    };
    if descriptor_ptr.is_null() {
        return Err(PluginError::NullNativeDescriptor {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol,
        });
    }

    let descriptor_text = unsafe { CStr::from_ptr(descriptor_ptr) }
        .to_str()
        .map_err(|_| PluginError::InvalidNativeDescriptorUtf8 {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: symbol.clone(),
        })?;

    let descriptor = NativeDescriptor::from_toml_str(descriptor_text).map_err(|error| {
        PluginError::InvalidNativeDescriptor {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: symbol.clone(),
            details: error.to_string(),
        }
    })?;

    descriptor.into_declaration(registered_plugin.declaration.entrypoint.clone())
}

fn compare_manifest_and_descriptor(
    registered_plugin: &RegisteredPlugin,
    declaration: &PluginDeclaration,
) -> Result<()> {
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "id",
        registered_plugin.declaration.id.as_str(),
        declaration.id.as_str(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "display_name",
        &registered_plugin.declaration.display_name,
        &declaration.display_name,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_version",
        &registered_plugin.declaration.plugin_version,
        &declaration.plugin_version,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_api",
        &registered_plugin.declaration.plugin_api.to_string(),
        &declaration.plugin_api.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "native_abi",
        &registered_plugin.declaration.native_abi.to_string(),
        &declaration.native_abi.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "required_capabilities",
        &format!("{:?}", registered_plugin.declaration.required_capabilities),
        &format!("{:?}", declaration.required_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_capabilities",
        &format!("{:?}", registered_plugin.declaration.provided_capabilities),
        &format!("{:?}", declaration.provided_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_features",
        &format!("{:?}", registered_plugin.declaration.provided_features),
        &format!("{:?}", declaration.provided_features),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "services",
        &serde_json::to_string(&registered_plugin.declaration.services)
            .expect("plugin services should serialize"),
        &serde_json::to_string(&declaration.services).expect("plugin services should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "commands",
        &serde_json::to_string(&registered_plugin.declaration.commands)
            .expect("plugin commands should serialize"),
        &serde_json::to_string(&declaration.commands).expect("plugin commands should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "event_subscriptions",
        &serde_json::to_string(&registered_plugin.declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
        &serde_json::to_string(&declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "dependencies",
        &serde_json::to_string(&registered_plugin.declaration.dependencies)
            .expect("plugin dependencies should serialize"),
        &serde_json::to_string(&declaration.dependencies)
            .expect("plugin dependencies should serialize"),
    )?;

    Ok(())
}

fn ensure_match(
    plugin_id: &str,
    field: &'static str,
    manifest_value: &str,
    descriptor_value: &str,
) -> Result<()> {
    if manifest_value == descriptor_value {
        Ok(())
    } else {
        Err(PluginError::NativeDescriptorMismatch {
            plugin_id: plugin_id.to_string(),
            field,
            manifest_value: manifest_value.to_string(),
            descriptor_value: descriptor_value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadedPlugin, NativeDescriptor, NativeLifecycleContext, NativeServiceContext};
    use crate::{
        ApiVersion, DEFAULT_NATIVE_ENTRY_SYMBOL, HostMetadata, PluginEntrypoint, PluginEvent,
        PluginEventKind, PluginEventSubscription, PluginManifest, PluginRegistry,
        ServiceEnvelopeKind, ServiceResponse, decode_service_envelope, encode_service_envelope,
    };
    use libloading::Library;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::c_char;
    use std::ptr;

    const TEST_DESCRIPTOR_TEXT: &str = concat!(
        "id = \"test.plugin\"\n",
        "display_name = \"Test Plugin\"\n",
        "plugin_version = \"0.1.0\"\n",
        "required_capabilities = [\"bmux.commands\"]\n\n",
        "[[commands]]\n",
        "name = \"hello\"\n",
        "summary = \"hello\"\n",
        "execution = \"provider_exec\"\n\n",
        "[plugin_api]\n",
        "minimum = \"1.0\"\n\n",
        "[native_abi]\n",
        "minimum = \"1.0\"\n",
        "\0"
    );

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_entry_v1() -> *const c_char {
        TEST_DESCRIPTOR_TEXT.as_ptr().cast()
    }

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_invoke_service_v1(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let (request_id, context) =
            decode_service_envelope::<NativeServiceContext>(input, ServiceEnvelopeKind::Request)
                .expect("service request should decode");
        let response = ServiceResponse::ok(context.request.payload);
        let encoded = encode_service_envelope(request_id, ServiceEnvelopeKind::Response, &response)
            .expect("service response should encode");
        unsafe {
            *output_len = encoded.len();
        }
        if output_ptr.is_null() || encoded.len() > output_capacity {
            return 4;
        }
        unsafe {
            ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
        }
        0
    }

    #[test]
    fn parses_native_descriptor_document() {
        let descriptor = NativeDescriptor::from_toml_str(
            r#"
id = "git.status"
display_name = "Git Status"
plugin_version = "0.1.0"
required_capabilities = ["bmux.commands"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("descriptor should parse");

        let declaration = descriptor
            .into_declaration(PluginEntrypoint::Native {
                symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            })
            .expect("declaration should convert");
        assert_eq!(declaration.id.as_str(), "git.status");
    }

    #[test]
    fn descriptor_conversion_rejects_invalid_plugin_id() {
        let descriptor = NativeDescriptor::from_toml_str(
            r#"
id = "GitStatus"
display_name = "Git Status"
plugin_version = "0.1.0"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("descriptor should parse");

        assert!(
            descriptor
                .into_declaration(PluginEntrypoint::Native {
                    symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
                })
                .is_err()
        );
    }

    #[test]
    fn loaded_plugin_reports_declared_commands() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: NativeDescriptor::from_toml_str(
                TEST_DESCRIPTOR_TEXT.trim_end_matches('\0'),
            )
            .expect("descriptor should parse")
            .into_declaration(PluginEntrypoint::Native {
                symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            })
            .expect("declaration should build"),
            _library: library,
        };

        assert_eq!(loaded.commands().len(), 1);
        assert!(loaded.supports_command("hello"));
        assert!(loaded.run_command("missing", &[]).is_err());
    }

    #[test]
    fn lifecycle_context_serializes_settings_and_host() {
        let context = NativeLifecycleContext {
            plugin_id: "test.plugin".to_string(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
            },
            settings: Some(toml::Value::String("enabled".to_string())),
            plugin_settings_map: BTreeMap::new(),
        };

        let json = serde_json::to_string(&context).expect("context should serialize");
        assert!(json.contains("test.plugin"));
        assert!(json.contains("bmux"));
        assert!(json.contains("enabled"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_capability() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.permissions.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
                provider_plugin_id: "bmux.permissions".to_string(),
            }],
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                crate::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing capability should fail");
        assert!(error.to_string().contains("bmux.permissions.read"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_registration() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.permissions.read".to_string()],
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                crate::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("call_service"));
    }

    #[test]
    fn loaded_plugin_filters_events_by_subscription() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"

[[event_subscriptions]]
kinds = ["system"]
names = ["server_started"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: crate::PluginDeclaration {
                id: crate::PluginId::new("test.plugin").expect("plugin id should parse"),
                display_name: "Test Plugin".to_string(),
                plugin_version: "0.1.0".to_string(),
                plugin_api: crate::VersionRange::at_least(ApiVersion::new(1, 0)),
                native_abi: crate::VersionRange::at_least(ApiVersion::new(1, 0)),
                entrypoint: PluginEntrypoint::Native {
                    symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
                },
                description: None,
                homepage: None,
                required_capabilities: BTreeSet::new(),
                provided_capabilities: BTreeSet::new(),
                provided_features: BTreeSet::new(),
                services: Vec::new(),
                commands: Vec::new(),
                event_subscriptions: vec![PluginEventSubscription {
                    kinds: BTreeSet::from([PluginEventKind::System]),
                    names: BTreeSet::from(["server_started".to_string()]),
                }],
                dependencies: Vec::new(),
                lifecycle: crate::PluginLifecycle::default(),
            },
            _library: library,
        };

        assert!(loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::System,
            name: "server_started".to_string(),
            payload: serde_json::Value::Null,
        }));
        assert!(!loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::System,
            name: "server_stopping".to_string(),
            payload: serde_json::Value::Null,
        }));
    }
}
