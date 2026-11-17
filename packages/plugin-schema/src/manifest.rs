//! Generate a plugin manifest from a BPDL world.
//!
//! BPDL is the single source of truth for a plugin's contract surface. The
//! handwritten `plugin.toml` keeps only settings BPDL does not model
//! (display name, release version, execution class, component roles,
//! dependencies, keybindings, event publications, owned namespaces, the
//! `bundled_only` packaging constraint and, until they move to BPDL
//! declarations, CLI commands). This module merges
//! those settings with everything derived from the package's single
//! `world`:
//!
//! | Generated key            | Source                                               |
//! |--------------------------|------------------------------------------------------|
//! | `services`               | `provide <iface>` (capability, kind, `@reentrant` ops) and `provide opaque` |
//! | `contracts`              | every provided service: wire id, major = `@interface-version` or 1 |
//! | `provided_capabilities`  | provided services' capabilities and `provide capability` |
//! | `required_capabilities`  | `require <iface>` capabilities and `require capability` |
//! | `optional_capabilities`  | the `require optional ...` forms                     |
//! | `commands`               | plugin-level `command` declarations, when present    |
//!
//! Settings that restate a generated key are rejected, so the two sources
//! can never drift. The result is plain TOML the host reads without running
//! plugin code.

use std::collections::{BTreeMap, BTreeSet};

use crate::Error;
use crate::ir::{Package, PackageSet, ServiceKind, Type, WorldItem};

/// Manifest keys owned by BPDL. `commands` is owned only when the package
/// declares plugin commands.
pub const GENERATED_KEYS: &[&str] = &[
    "contracts",
    "provided_capabilities",
    "services",
    "required_capabilities",
    "optional_capabilities",
];

fn manifest_error(message: impl Into<String>) -> Error {
    Error::Validate {
        message: message.into(),
    }
}

/// One generated `[[services]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEntry {
    pub capability: String,
    pub interface_id: String,
    pub kind: ServiceKind,
    pub reentrant_operations: BTreeSet<String>,
}

/// One generated `[[contracts]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractEntry {
    pub interface_id: String,
    pub major: u32,
    pub minor: u32,
}

/// Everything a package's world contributes to its manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldDeclarations {
    pub services: Vec<ServiceEntry>,
    pub contracts: Vec<ContractEntry>,
    pub provided_capabilities: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub optional_capabilities: Vec<String>,
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

impl WorldDeclarations {
    fn provide(&mut self, service: ServiceEntry, major: u32) -> Result<(), Error> {
        if self.services.iter().any(|existing| {
            existing.capability == service.capability
                && existing.kind == service.kind
                && existing.interface_id == service.interface_id
        }) {
            return Err(manifest_error(format!(
                "world provides `{}` ({}) more than once",
                service.interface_id,
                service.kind.as_str()
            )));
        }
        match self
            .contracts
            .iter()
            .find(|contract| contract.interface_id == service.interface_id)
        {
            Some(existing) if existing.major != major => {
                return Err(manifest_error(format!(
                    "world provides `{}` with conflicting majors {} and {major}",
                    service.interface_id, existing.major
                )));
            }
            Some(_) => {}
            None => self.contracts.push(ContractEntry {
                interface_id: service.interface_id.clone(),
                major,
                minor: 0,
            }),
        }
        push_unique(&mut self.provided_capabilities, &service.capability);
        self.services.push(service);
        Ok(())
    }

    fn require(&mut self, capability: &str, optional: bool) {
        if optional {
            push_unique(&mut self.optional_capabilities, capability);
        } else {
            push_unique(&mut self.required_capabilities, capability);
        }
    }
}

/// The package's only world.
fn single_world(package: &Package) -> Result<&crate::ir::World, Error> {
    match package.worlds.as_slice() {
        [world] => Ok(world),
        [] => Err(manifest_error(format!(
            "package `{}` declares no `world`; manifests are generated from exactly one",
            package.id
        ))),
        _ => Err(manifest_error(format!(
            "package `{}` declares several worlds; manifests are generated from exactly one",
            package.id
        ))),
    }
}

/// Resolve a world's interface reference to the interface and the id of
/// the capability it declares, if any.
fn resolve_interface<'a>(
    set: &'a PackageSet,
    package: &'a Package,
    world: &str,
    reference: &crate::ir::InterfaceRef,
) -> Result<(&'a crate::ir::Interface, Option<&'a str>), Error> {
    let owner = match &reference.import {
        None => package,
        Some(alias) => set.imported(package, alias).ok_or_else(|| {
            manifest_error(format!(
                "world `{world}` names import `{alias}`, whose schema was not supplied"
            ))
        })?,
    };
    let iface = owner
        .find_interface(&reference.name, reference.version)
        .ok_or_else(|| {
            manifest_error(format!(
                "world `{world}` names unknown interface `{}`",
                reference.name
            ))
        })?;
    let capability = match &iface.capability {
        Some(name) => Some(owner.capability_id(name).ok_or_else(|| {
            manifest_error(format!(
                "interface `{}` names undeclared capability `{name}`",
                iface.name
            ))
        })?),
        None => None,
    };
    Ok((iface, capability))
}

/// Resolve the single world of `package` into manifest declarations.
///
/// # Errors
///
/// Returns an error when the package has no world or several, or when a
/// world entry names an interface that cannot be resolved through `set`.
pub fn world_declarations(set: &PackageSet, package: &Package) -> Result<WorldDeclarations, Error> {
    let world = single_world(package)?;
    let resolve = |reference| resolve_interface(set, package, &world.name, reference);
    let mut declarations = WorldDeclarations::default();
    for item in &world.items {
        match item {
            WorldItem::Provide {
                interface,
                capability,
                ..
            } => {
                let (iface, declared) = resolve(interface)?;
                let kind = iface.service_kind().ok_or_else(|| {
                    manifest_error(format!(
                        "world `{}` provides `{}`, which has no operations or events",
                        world.name, iface.name
                    ))
                })?;
                let capability = declared
                    .or(capability.as_deref())
                    .ok_or_else(|| {
                        manifest_error(format!(
                            "world `{}` must name the capability guarding `{}`",
                            world.name, iface.name
                        ))
                    })?
                    .to_string();
                declarations.provide(
                    ServiceEntry {
                        capability,
                        interface_id: iface.wire_id(),
                        kind,
                        reentrant_operations: iface
                            .reentrant_operations()
                            .map(str::to_string)
                            .collect(),
                    },
                    iface.major(),
                )?;
            }
            WorldItem::ProvideOpaque {
                name,
                version,
                kind,
                capability,
                ..
            } => declarations.provide(
                ServiceEntry {
                    capability: capability.clone(),
                    interface_id: version
                        .map_or_else(|| name.clone(), |version| format!("{name}/v{version}")),
                    kind: *kind,
                    reentrant_operations: BTreeSet::new(),
                },
                version.unwrap_or(1),
            )?,
            WorldItem::ProvideCapability { capability, .. } => {
                push_unique(&mut declarations.provided_capabilities, capability);
            }
            WorldItem::Require {
                interface,
                optional,
                ..
            } => {
                if let (_, Some(capability)) = resolve(interface)? {
                    declarations.require(capability, *optional);
                }
            }
            WorldItem::RequireCapability {
                capability,
                optional,
                ..
            } => declarations.require(capability, *optional),
        }
    }
    Ok(declarations)
}

fn strings(values: &[String]) -> toml::Value {
    toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
}

fn command_argument(param: &crate::ir::Field) -> toml::Table {
    let mut argument = toml::Table::new();
    let (inner, optional, multiple) = match &param.ty {
        Type::Option(inner) => (inner.as_ref(), true, false),
        Type::List(inner) => (inner.as_ref(), true, true),
        other => (other, false, false),
    };
    let kind = match inner {
        Type::Primitive(crate::ir::Primitive::Bool) => "boolean",
        Type::Primitive(
            crate::ir::Primitive::U8
            | crate::ir::Primitive::U16
            | crate::ir::Primitive::U32
            | crate::ir::Primitive::U64
            | crate::ir::Primitive::I8
            | crate::ir::Primitive::I16
            | crate::ir::Primitive::I32
            | crate::ir::Primitive::I64,
        ) => "integer",
        _ => "string",
    };
    argument.insert("name".into(), param.name.clone().into());
    argument.insert("long".into(), param.name.clone().into());
    argument.insert("kind".into(), kind.into());
    if !optional && kind != "boolean" {
        argument.insert("required".into(), true.into());
    }
    if multiple {
        argument.insert("multiple".into(), true.into());
    }
    if let Some(summary) = param.docs.first() {
        argument.insert("summary".into(), summary.clone().into());
    }
    argument
}

fn commands(package: &Package) -> toml::Value {
    toml::Value::Array(
        package
            .commands
            .iter()
            .map(|command| {
                let mut entry = toml::Table::new();
                entry.insert("name".into(), command.name.clone().into());
                entry.insert(
                    "path".into(),
                    toml::Value::Array(vec![command.name.clone().into()]),
                );
                let summary = command
                    .title
                    .clone()
                    .or_else(|| command.docs.first().cloned())
                    .unwrap_or_else(|| command.name.clone());
                entry.insert("summary".into(), summary.into());
                entry.insert("expose_in_cli".into(), true.into());
                entry.insert(
                    "arguments".into(),
                    toml::Value::Array(
                        command
                            .params
                            .iter()
                            .map(|param| toml::Value::Table(command_argument(param)))
                            .collect(),
                    ),
                );
                toml::Value::Table(entry)
            })
            .collect(),
    )
}

/// BPDL commands are exposed in the CLI, so the plugin owns their paths;
/// settings may claim further paths, never fewer.
fn claim_command_paths(table: &mut toml::Table, package: &Package) -> Result<(), Error> {
    let mut owned = match table.remove("owns_paths") {
        Some(toml::Value::Array(paths)) => paths,
        Some(_) => return Err(manifest_error("owns_paths must be an array of paths")),
        None => Vec::new(),
    };
    for command in &package.commands {
        let path = toml::Value::Array(vec![command.name.clone().into()]);
        if !owned.contains(&path) {
            owned.push(path);
        }
    }
    table.insert("owns_paths".into(), toml::Value::Array(owned));
    Ok(())
}

/// Merge handwritten `settings` TOML with the declarations generated from
/// the package's world, returning the complete manifest TOML.
///
/// # Errors
///
/// Returns an error when the settings are not a TOML table, name a
/// different plugin id, restate a generated key, or when the world cannot
/// be resolved.
pub fn generate(set: &PackageSet, package: &Package, settings: &str) -> Result<String, Error> {
    let mut table: toml::Table = toml::from_str(settings)
        .map_err(|error| manifest_error(format!("invalid plugin settings TOML: {error}")))?;
    match table.get("id").and_then(toml::Value::as_str) {
        Some(id) if id == package.id => {}
        Some(id) => {
            return Err(manifest_error(format!(
                "plugin settings declare id `{id}` but the BPDL package is `{}`",
                package.id
            )));
        }
        None => {
            return Err(manifest_error(format!(
                "plugin settings must declare `id = \"{}\"`",
                package.id
            )));
        }
    }
    let owned: Vec<&str> = GENERATED_KEYS
        .iter()
        .copied()
        .chain((!package.commands.is_empty()).then_some("commands"))
        .filter(|key| table.contains_key(*key))
        .collect();
    if !owned.is_empty() {
        return Err(manifest_error(format!(
            "plugin settings restate keys generated from BPDL: {}; declare them in the package world instead",
            owned.join(", ")
        )));
    }
    let declarations = world_declarations(set, package)?;
    let mut insert = |key: &str, values: &[String]| {
        if !values.is_empty() {
            table.insert(key.to_string(), strings(values));
        }
    };
    insert("provided_capabilities", &declarations.provided_capabilities);
    insert("required_capabilities", &declarations.required_capabilities);
    insert("optional_capabilities", &declarations.optional_capabilities);
    if !declarations.services.is_empty() {
        let services = declarations
            .services
            .iter()
            .map(|service| {
                let mut entry = BTreeMap::new();
                entry.insert("capability", service.capability.clone().into());
                entry.insert("interface_id", service.interface_id.clone().into());
                entry.insert("kind", service.kind.as_str().into());
                if !service.reentrant_operations.is_empty() {
                    entry.insert(
                        "reentrant_operations",
                        toml::Value::Array(
                            service
                                .reentrant_operations
                                .iter()
                                .cloned()
                                .map(toml::Value::String)
                                .collect(),
                        ),
                    );
                }
                toml::Value::Table(
                    entry
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                )
            })
            .collect();
        table.insert("services".into(), toml::Value::Array(services));
    }
    if !declarations.contracts.is_empty() {
        let contracts = declarations
            .contracts
            .iter()
            .map(|contract| {
                let mut entry = toml::Table::new();
                entry.insert("interface_id".into(), contract.interface_id.clone().into());
                entry.insert("major".into(), i64::from(contract.major).into());
                entry.insert("minor".into(), i64::from(contract.minor).into());
                toml::Value::Table(entry)
            })
            .collect();
        table.insert("contracts".into(), toml::Value::Array(contracts));
    }
    if !package.commands.is_empty() {
        table.insert("commands".into(), commands(package));
        claim_command_paths(&mut table, package)?;
    }
    let body = toml::to_string(&table)
        .map_err(|error| manifest_error(format!("failed to render manifest TOML: {error}")))?;
    Ok(format!(
        "# Generated from BPDL package `{}` and its plugin settings. Do not edit.\n{body}",
        package.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(source: &str) -> Package {
        crate::ir::lower(&crate::compile(source).expect("schema"))
    }

    const API: &str = "plugin acme.files version 1;
        capability READ = acme.files.read;
        capability WRITE = acme.files.write;
        @capability(READ) interface state { @reentrant query get() -> u32; query list() -> u32; }
        @capability(WRITE) @interface-version(2) interface edit { command touch() -> unit; }
        interface changes { record e { n: u32 } events e; }
        world files {
            provide state;
            provide edit@2;
            provide changes capability acme.files.read;
            provide opaque legacy: command capability acme.files.write;
            provide capability acme.files.admin;
            require capability acme.storage;
            require optional capability acme.spawn;
        }";

    #[test]
    fn generates_every_contract_key_from_the_world() {
        let manifest = generate(
            &PackageSet::new(),
            &package(API),
            "id = 'acme.files'\nname = 'Files'\nversion = '0.1.0'\nbundled_only = 'opens sockets'\n[keybindings.runtime]\nf = 'open'\n",
        )
        .expect("manifest");
        let value: toml::Table = toml::from_str(&manifest).expect("valid TOML");
        assert_eq!(value["name"].as_str(), Some("Files"));
        // Packaging constraints are settings, carried through verbatim.
        assert_eq!(value["bundled_only"].as_str(), Some("opens sockets"));
        assert_eq!(
            value["provided_capabilities"],
            strings(&[
                "acme.files.read".into(),
                "acme.files.write".into(),
                "acme.files.admin".into()
            ])
        );
        assert_eq!(
            value["required_capabilities"],
            strings(&["acme.storage".into()])
        );
        assert_eq!(
            value["optional_capabilities"],
            strings(&["acme.spawn".into()])
        );
        let services = value["services"].as_array().expect("services");
        assert_eq!(services.len(), 4);
        assert_eq!(services[0]["interface_id"].as_str(), Some("state"));
        assert_eq!(services[0]["kind"].as_str(), Some("query"));
        assert_eq!(
            services[0]["reentrant_operations"],
            strings(&["get".into()])
        );
        assert_eq!(services[1]["interface_id"].as_str(), Some("edit/v2"));
        assert_eq!(services[2]["kind"].as_str(), Some("event"));
        assert_eq!(services[3]["interface_id"].as_str(), Some("legacy"));
        let contracts = value["contracts"].as_array().expect("contracts");
        assert_eq!(contracts.len(), 4);
        assert_eq!(contracts[1]["major"].as_integer(), Some(2));
        assert_eq!(value["keybindings"]["runtime"]["f"].as_str(), Some("open"));
    }

    #[test]
    fn handwritten_contract_keys_and_identity_drift_are_rejected() {
        let api = package(API);
        for settings in [
            "id = 'acme.files'\nprovided_capabilities = []\n",
            "id = 'acme.files'\n[[services]]\ncapability = 'x'\ninterface_id = 'y'\nkind = 'query'\n",
            "id = 'acme.files'\nrequired_capabilities = ['acme.storage']\n",
            "id = 'acme.other'\n",
            "name = 'missing id'\n",
        ] {
            assert!(
                generate(&PackageSet::new(), &api, settings).is_err(),
                "accepted {settings}"
            );
        }
        let worldless = package("plugin acme.files version 1;");
        assert!(generate(&PackageSet::new(), &worldless, "id = 'acme.files'\n").is_err());
    }

    #[test]
    fn worlds_resolve_imported_interfaces_through_the_package_set() {
        let mut set = PackageSet::new();
        set.insert(package(API));
        let plugin = package(
            "plugin acme.viewer version 1; import files = acme.files;
             world viewer { provide files.state; require files.edit@2; require optional files.state; }",
        );
        let declarations = world_declarations(&set, &plugin).expect("declarations");
        assert_eq!(declarations.services[0].capability, "acme.files.read");
        assert_eq!(declarations.required_capabilities, ["acme.files.write"]);
        assert_eq!(declarations.optional_capabilities, ["acme.files.read"]);
        assert!(world_declarations(&PackageSet::new(), &plugin).is_err());
    }

    #[test]
    fn plugin_commands_generate_command_declarations() {
        let manifest = generate(
            &PackageSet::new(),
            &package(
                "plugin example.hello version 1; world hello {}
                 /// Greets someone.
                 command hello(/// Who to greet.
                 name: string?, count: u32, loud: bool, tags: list<string>) title \"Say hello\";",
            ),
            "id = 'example.hello'\nowns_paths = [['greet', 'more'], ['hello']]\n",
        )
        .expect("manifest");
        let value: toml::Table = toml::from_str(&manifest).unwrap();
        // The CLI path of every BPDL command is owned; settings add, never remove.
        assert_eq!(
            value["owns_paths"],
            toml::Value::Array(vec![
                toml::Value::Array(vec!["greet".into(), "more".into()]),
                toml::Value::Array(vec!["hello".into()]),
            ])
        );
        let command = &value["commands"].as_array().unwrap()[0];
        assert_eq!(command["summary"].as_str(), Some("Say hello"));
        let arguments = command["arguments"].as_array().unwrap();
        assert_eq!(arguments[0]["summary"].as_str(), Some("Who to greet."));
        assert!(arguments[0].get("required").is_none());
        assert_eq!(arguments[1]["kind"].as_str(), Some("integer"));
        assert_eq!(arguments[1]["required"].as_bool(), Some(true));
        assert_eq!(arguments[2]["kind"].as_str(), Some("boolean"));
        assert_eq!(arguments[3]["multiple"].as_bool(), Some(true));
        assert!(
            generate(
                &PackageSet::new(),
                &package("plugin example.hello version 1; world hello {} command hello();"),
                "id = 'example.hello'\n[[commands]]\nname = 'hello'\npath = ['hello']\nsummary = 's'\n",
            )
            .is_err()
        );
    }
}
