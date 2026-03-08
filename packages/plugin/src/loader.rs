use crate::{
    DEFAULT_NATIVE_ACTIVATE_SYMBOL, DEFAULT_NATIVE_COMMAND_SYMBOL,
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL, HostMetadata, PluginCapability, PluginDeclaration,
    PluginEntrypoint, PluginError, PluginLifecycle, PluginManifestCompatibility, PluginRegistry,
    RegisteredPlugin, Result,
};
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char};
use std::path::Path;

type NativeDescriptorFn = unsafe extern "C" fn() -> *const c_char;
type NativeRunCommandFn = unsafe extern "C" fn(*const c_char, usize, *const *const c_char) -> i32;
type NativeLifecycleFn = unsafe extern "C" fn(*const c_char) -> i32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLifecycleContext {
    pub plugin_id: String,
    pub host: HostMetadata,
    #[serde(default)]
    pub settings: Option<toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    pub capabilities: BTreeSet<PluginCapability>,
    #[serde(default)]
    pub commands: Vec<crate::PluginCommand>,
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
            capabilities: self.capabilities,
            commands: self.commands,
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
        if !self.supports_command(command_name) {
            return Err(PluginError::UnknownPluginCommand {
                plugin_id: self.declaration.id.as_str().to_string(),
                command: command_name.to_string(),
            });
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
        supported_capabilities: &[PluginCapability],
    ) -> Result<LoadedPlugin> {
        PluginRegistry::validate_registered_plugin(
            registered_plugin,
            host,
            supported_capabilities,
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
            supported_capabilities,
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
    supported_capabilities: &[PluginCapability],
) -> Result<LoadedPlugin> {
    NativePluginLoader::new().load_registered_plugin(
        registered_plugin,
        host,
        supported_capabilities,
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
        "capabilities",
        &format!("{:?}", registered_plugin.declaration.capabilities),
        &format!("{:?}", declaration.capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "commands",
        &serde_json::to_string(&registered_plugin.declaration.commands)
            .expect("plugin commands should serialize"),
        &serde_json::to_string(&declaration.commands).expect("plugin commands should serialize"),
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
    use super::{LoadedPlugin, NativeDescriptor, NativeLifecycleContext};
    use crate::{
        ApiVersion, DEFAULT_NATIVE_ENTRY_SYMBOL, HostMetadata, PluginEntrypoint, PluginManifest,
        PluginRegistry,
    };
    use libloading::Library;
    use std::ffi::c_char;

    const TEST_DESCRIPTOR_TEXT: &str = concat!(
        "id = \"test.plugin\"\n",
        "display_name = \"Test Plugin\"\n",
        "plugin_version = \"0.1.0\"\n",
        "capabilities = [\"commands\"]\n\n",
        "[[commands]]\n",
        "name = \"hello\"\n",
        "summary = \"hello\"\n",
        "execution = \"host_callback\"\n\n",
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

    #[test]
    fn parses_native_descriptor_document() {
        let descriptor = NativeDescriptor::from_toml_str(
            r#"
id = "git.status"
display_name = "Git Status"
plugin_version = "0.1.0"
capabilities = ["commands"]

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
capabilities = ["commands"]

[[commands]]
name = "hello"
summary = "hello"
execution = "host_callback"

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
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            settings: Some(toml::Value::String("enabled".to_string())),
        };

        let json = serde_json::to_string(&context).expect("context should serialize");
        assert!(json.contains("test.plugin"));
        assert!(json.contains("bmux"));
        assert!(json.contains("enabled"));
    }
}
