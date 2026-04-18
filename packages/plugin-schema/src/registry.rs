//! Runtime schema registry.
//!
//! A [`SchemaRegistry`] holds parsed and validated BPDL schemas indexed
//! by plugin id. At plugin load time the host registers each plugin's
//! schema source; cross-plugin typed dispatch then consults the registry
//! to confirm that a consumer and provider agree on an interface's
//! shape before wiring a call.
//!
//! The registry is pure data — no transport, no dispatch. Slice C (a
//! future step) will call [`SchemaRegistry::check_compatibility`] from
//! the plugin host when resolving typed services.

use std::collections::BTreeMap;

use crate::{Error, ast::Schema};

/// One registered schema entry.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    /// The schema's `plugin <id>` value.
    pub plugin_id: String,
    /// The schema's declared version number.
    pub version: u32,
    /// The parsed, validated AST.
    pub schema: Schema,
    /// The original source text. Retained for error messages and for
    /// non-Rust SDKs that want to re-parse the schema themselves.
    pub source: String,
}

/// Errors that can occur during typed-dispatch compatibility checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatError {
    /// The registry has no entry for the given plugin id.
    PluginNotRegistered(String),
    /// The registered plugin has no interface by that name.
    InterfaceNotFound {
        plugin_id: String,
        interface_name: String,
    },
    /// Provider and consumer declared different shapes for the same
    /// interface. The first mismatched signature is reported.
    SignatureMismatch {
        interface_name: String,
        operation: String,
        provider: String,
        consumer: String,
    },
    /// Provider's version is older than consumer's minimum requirement.
    VersionTooOld {
        provider_version: u32,
        consumer_version: u32,
    },
    /// Provider and consumer disagree on which operations exist.
    OperationMissing {
        interface_name: String,
        operation: String,
        side: &'static str,
    },
}

impl std::fmt::Display for CompatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PluginNotRegistered(id) => {
                write!(f, "plugin `{id}` is not registered")
            }
            Self::InterfaceNotFound {
                plugin_id,
                interface_name,
            } => write!(
                f,
                "plugin `{plugin_id}` has no interface `{interface_name}`"
            ),
            Self::SignatureMismatch {
                interface_name,
                operation,
                provider,
                consumer,
            } => write!(
                f,
                "interface `{interface_name}` op `{operation}`: provider says `{provider}`, consumer expects `{consumer}`"
            ),
            Self::VersionTooOld {
                provider_version,
                consumer_version,
            } => write!(
                f,
                "provider version {provider_version} is older than consumer requires (>= {consumer_version})"
            ),
            Self::OperationMissing {
                interface_name,
                operation,
                side,
            } => write!(
                f,
                "interface `{interface_name}` op `{operation}` is missing on the {side}"
            ),
        }
    }
}

impl std::error::Error for CompatError {}

/// In-memory registry of parsed plugin schemas.
#[derive(Debug, Default)]
pub struct SchemaRegistry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl SchemaRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse, validate, and register a BPDL schema source string.
    ///
    /// Registering a plugin a second time replaces the earlier entry.
    ///
    /// # Errors
    ///
    /// Returns the parse/validate [`Error`] if the source is malformed.
    ///
    /// # Panics
    ///
    /// Never — the unwrap after `insert` is justified because the key
    /// was just inserted and the registry holds no concurrent writers.
    pub fn register(&mut self, source: &str) -> Result<&RegistryEntry, Error> {
        let schema = crate::compile(source)?;
        let entry = RegistryEntry {
            plugin_id: schema.plugin.plugin_id.clone(),
            version: schema.plugin.version,
            schema,
            source: source.to_string(),
        };
        let id = entry.plugin_id.clone();
        self.entries.insert(id.clone(), entry);
        Ok(self.entries.get(&id).expect("just inserted"))
    }

    /// Look up a registered schema entry by plugin id.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<&RegistryEntry> {
        self.entries.get(plugin_id)
    }

    /// Enumerate all registered plugin ids.
    pub fn plugin_ids(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Verify that a provider and consumer agree on a specific
    /// interface's shape.
    ///
    /// Strict v1 rules:
    /// - both plugins must be registered;
    /// - both must declare the named interface;
    /// - provider's plugin version must be `>=` consumer's;
    /// - the union of `query`/`command` names must match between sides;
    /// - for each matching op, the stringified parameter list and
    ///   return type must match.
    ///
    /// # Errors
    ///
    /// Returns a list of [`CompatError`]s enumerating every incompatible
    /// facet. An empty provider/consumer pair returns `Ok(())`.
    pub fn check_compatibility(
        &self,
        provider_plugin_id: &str,
        consumer_plugin_id: &str,
        interface_name: &str,
    ) -> Result<(), Vec<CompatError>> {
        let mut errors = Vec::new();

        let Some(provider) = self.entries.get(provider_plugin_id) else {
            errors.push(CompatError::PluginNotRegistered(
                provider_plugin_id.to_string(),
            ));
            return Err(errors);
        };
        let Some(consumer) = self.entries.get(consumer_plugin_id) else {
            errors.push(CompatError::PluginNotRegistered(
                consumer_plugin_id.to_string(),
            ));
            return Err(errors);
        };

        if provider.version < consumer.version {
            errors.push(CompatError::VersionTooOld {
                provider_version: provider.version,
                consumer_version: consumer.version,
            });
        }

        let provider_iface = provider
            .schema
            .interfaces
            .iter()
            .find(|i| i.name == interface_name);
        let consumer_iface = consumer
            .schema
            .interfaces
            .iter()
            .find(|i| i.name == interface_name);

        let (Some(provider_iface), Some(consumer_iface)) = (provider_iface, consumer_iface) else {
            if provider_iface.is_none() {
                errors.push(CompatError::InterfaceNotFound {
                    plugin_id: provider_plugin_id.to_string(),
                    interface_name: interface_name.to_string(),
                });
            }
            if consumer_iface.is_none() {
                errors.push(CompatError::InterfaceNotFound {
                    plugin_id: consumer_plugin_id.to_string(),
                    interface_name: interface_name.to_string(),
                });
            }
            return Err(errors);
        };

        let provider_ops = collect_ops(provider_iface);
        let consumer_ops = collect_ops(consumer_iface);

        for (name, provider_sig) in &provider_ops {
            match consumer_ops.get(name) {
                Some(consumer_sig) if consumer_sig == provider_sig => {}
                Some(consumer_sig) => errors.push(CompatError::SignatureMismatch {
                    interface_name: interface_name.to_string(),
                    operation: name.clone(),
                    provider: provider_sig.clone(),
                    consumer: consumer_sig.clone(),
                }),
                None => errors.push(CompatError::OperationMissing {
                    interface_name: interface_name.to_string(),
                    operation: name.clone(),
                    side: "consumer",
                }),
            }
        }
        for name in consumer_ops.keys() {
            if !provider_ops.contains_key(name) {
                errors.push(CompatError::OperationMissing {
                    interface_name: interface_name.to_string(),
                    operation: name.clone(),
                    side: "provider",
                });
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

fn collect_ops(iface: &crate::ast::Interface) -> BTreeMap<String, String> {
    use crate::ast::InterfaceItem;
    let mut out = BTreeMap::new();
    for item in &iface.items {
        if let InterfaceItem::Query(op) | InterfaceItem::Command(op) = item {
            out.insert(op.name.clone(), format_op_signature(op));
        }
    }
    out
}

fn format_op_signature(op: &crate::ast::Operation) -> String {
    let params = op
        .params
        .iter()
        .map(|f| format!("{}:{}", f.name, format_type(&f.ty)))
        .collect::<Vec<_>>()
        .join(",");
    format!("({params})->{}", format_type(&op.returns))
}

fn format_type(ty: &crate::ast::TypeRef) -> String {
    use crate::ast::TypeRef;
    match ty {
        TypeRef::Primitive(p) => p.keyword().to_string(),
        TypeRef::Named(n) => n.clone(),
        TypeRef::Qualified { alias, name } => format!("{alias}.{name}"),
        TypeRef::Option(inner) => format!("{}?", format_type(inner)),
        TypeRef::List(inner) => format!("list<{}>", format_type(inner)),
        TypeRef::Map(k, v) => format!("map<{},{}>", format_type(k), format_type(v)),
        TypeRef::Result(a, b) => format!("result<{},{}>", format_type(a), format_type(b)),
        TypeRef::Unit => "unit".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS_V1: &str = "plugin bmux.windows version 1;\n\
        interface windows-state {\n\
          record pane-state { id: uuid }\n\
          query pane-state(id: uuid) -> pane-state?;\n\
          command focus-pane(id: uuid) -> result<unit, string>;\n\
        }";

    #[test]
    fn register_and_lookup_round_trips() {
        let mut reg = SchemaRegistry::new();
        let entry = reg.register(WINDOWS_V1).expect("register");
        assert_eq!(entry.plugin_id, "bmux.windows");
        assert_eq!(entry.version, 1);
        let fetched = reg.get("bmux.windows").expect("registered");
        assert_eq!(fetched.version, 1);
    }

    #[test]
    fn compatibility_passes_for_identical_schemas() {
        let mut reg = SchemaRegistry::new();
        reg.register(WINDOWS_V1).expect("provider");
        reg.register(
            "plugin bmux.windows.consumer version 1;\n\
             interface windows-state {\n\
               record pane-state { id: uuid }\n\
               query pane-state(id: uuid) -> pane-state?;\n\
               command focus-pane(id: uuid) -> result<unit, string>;\n\
             }",
        )
        .expect("consumer");
        let r = reg.check_compatibility("bmux.windows", "bmux.windows.consumer", "windows-state");
        assert!(r.is_ok(), "expected compat, got {r:?}");
    }

    #[test]
    fn compatibility_detects_signature_mismatch() {
        let mut reg = SchemaRegistry::new();
        reg.register(WINDOWS_V1).expect("provider");
        reg.register(
            "plugin consumer version 1;\n\
             interface windows-state {\n\
               record pane-state { id: uuid }\n\
               query pane-state(id: uuid) -> pane-state;\n\
               command focus-pane(id: uuid) -> result<unit, string>;\n\
             }",
        )
        .expect("consumer");
        let err = reg
            .check_compatibility("bmux.windows", "consumer", "windows-state")
            .unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, CompatError::SignatureMismatch { .. })),
            "expected SignatureMismatch, got {err:?}"
        );
    }

    #[test]
    fn compatibility_detects_missing_interface() {
        let mut reg = SchemaRegistry::new();
        reg.register(WINDOWS_V1).expect("provider");
        reg.register(
            "plugin consumer version 1;\n\
             interface other-iface { query q() -> bool; }",
        )
        .expect("consumer");
        let err = reg
            .check_compatibility("bmux.windows", "consumer", "windows-state")
            .unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, CompatError::InterfaceNotFound { .. })),
            "expected InterfaceNotFound, got {err:?}"
        );
    }

    #[test]
    fn compatibility_detects_version_too_old() {
        let mut reg = SchemaRegistry::new();
        reg.register(WINDOWS_V1).expect("provider");
        reg.register(
            "plugin consumer version 3;\n\
             interface windows-state {\n\
               record pane-state { id: uuid }\n\
               query pane-state(id: uuid) -> pane-state?;\n\
               command focus-pane(id: uuid) -> result<unit, string>;\n\
             }",
        )
        .expect("consumer");
        let err = reg
            .check_compatibility("bmux.windows", "consumer", "windows-state")
            .unwrap_err();
        assert!(
            err.iter()
                .any(|e| matches!(e, CompatError::VersionTooOld { .. })),
            "expected VersionTooOld, got {err:?}"
        );
    }
}
