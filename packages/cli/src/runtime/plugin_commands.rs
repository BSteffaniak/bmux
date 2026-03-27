use super::built_in_commands::reserved_built_in_paths;
use anyhow::{Context, Result, bail};
use bmux_config::BmuxConfig;
use bmux_plugin::{
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginRegistry,
};
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct ResolvedPluginCommand {
    pub plugin_id: String,
    pub command_name: String,
    pub arguments: Vec<String>,
    pub schema: PluginCommand,
}

#[derive(Debug, Clone)]
pub struct RegisteredPluginCommand {
    pub plugin_id: String,
    pub command_name: String,
    pub canonical_path: Vec<String>,
    pub aliases: Vec<Vec<String>>,
    pub schema: PluginCommand,
}

#[derive(Debug, Default, Clone)]
pub struct PluginCommandRegistry {
    commands: Vec<RegisteredPluginCommand>,
}

impl PluginCommandRegistry {
    pub fn build(config: &BmuxConfig, plugins: &PluginRegistry) -> Result<Self> {
        let mut registry = Self::default();
        let enabled = config.plugins.enabled.iter().collect::<BTreeSet<_>>();
        let reserved = reserved_built_in_paths();
        let mut claimed: BTreeMap<Vec<String>, (String, String)> = BTreeMap::new();

        for plugin in plugins.iter() {
            if !enabled.contains(&plugin.declaration.id.as_str().to_string()) {
                continue;
            }

            for command in plugin
                .declaration
                .commands
                .iter()
                .filter(|command| command.expose_in_cli)
            {
                let canonical = command.canonical_path();
                validate_path_collision(
                    &canonical,
                    &reserved,
                    &claimed,
                    plugin.declaration.id.as_str(),
                    &command.name,
                )?;
                validate_prefix_collision(
                    &canonical,
                    &reserved,
                    &claimed,
                    plugin.declaration.id.as_str(),
                    &command.name,
                )?;
                claimed.insert(
                    canonical.clone(),
                    (
                        plugin.declaration.id.as_str().to_string(),
                        command.name.clone(),
                    ),
                );

                let mut aliases = Vec::new();
                for alias in &command.aliases {
                    validate_path_collision(
                        alias,
                        &reserved,
                        &claimed,
                        plugin.declaration.id.as_str(),
                        &command.name,
                    )?;
                    validate_prefix_collision(
                        alias,
                        &reserved,
                        &claimed,
                        plugin.declaration.id.as_str(),
                        &command.name,
                    )?;
                    claimed.insert(
                        alias.clone(),
                        (
                            plugin.declaration.id.as_str().to_string(),
                            command.name.clone(),
                        ),
                    );
                    aliases.push(alias.clone());
                }

                registry.commands.push(RegisteredPluginCommand {
                    plugin_id: plugin.declaration.id.as_str().to_string(),
                    command_name: command.name.clone(),
                    canonical_path: canonical,
                    aliases,
                    schema: command.clone(),
                });
            }
        }

        Ok(registry)
    }

    pub fn resolve(&self, raw: &[String]) -> Option<ResolvedPluginCommand> {
        self.commands
            .iter()
            .flat_map(|command| {
                std::iter::once((&command.canonical_path, command))
                    .chain(command.aliases.iter().map(move |alias| (alias, command)))
            })
            .filter(|(path, _)| raw.starts_with(path))
            .max_by_key(|(path, _)| path.len())
            .map(|(path, command)| ResolvedPluginCommand {
                plugin_id: command.plugin_id.clone(),
                command_name: command.command_name.clone(),
                arguments: raw[path.len()..].to_vec(),
                schema: command.schema.clone(),
            })
    }

    pub fn resolve_exact_path(&self, path: &[String]) -> Option<ResolvedPluginCommand> {
        self.commands.iter().find_map(|command| {
            let matches_path =
                command.canonical_path == path || command.aliases.iter().any(|alias| alias == path);
            if matches_path {
                Some(ResolvedPluginCommand {
                    plugin_id: command.plugin_id.clone(),
                    command_name: command.command_name.clone(),
                    arguments: Vec::new(),
                    schema: command.schema.clone(),
                })
            } else {
                None
            }
        })
    }

    pub fn validate_arguments(
        command: &PluginCommand,
        arguments: &[String],
    ) -> Result<Vec<String>> {
        let mut clap_command =
            Command::new(leak_string(&command.name)).disable_help_subcommand(true);
        for argument in &command.arguments {
            clap_command = clap_command.arg(build_clap_arg(argument)?);
        }
        let mut argv = Vec::with_capacity(arguments.len() + 1);
        argv.push(command.name.clone());
        argv.extend(arguments.iter().cloned());
        let matches = clap_command.try_get_matches_from(argv).with_context(|| {
            format!(
                "invalid arguments for plugin command '{}': {}",
                command.name,
                arguments.join(" ")
            )
        })?;

        let mut normalized = Vec::new();
        for argument in &command.arguments {
            if let Some(long) = &argument.long {
                if matches.value_source(&argument.name).is_none() {
                    continue;
                }
                if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
                    if matches.get_flag(&argument.name) {
                        normalized.push(format!("--{long}"));
                    }
                    continue;
                }
                if argument.multiple {
                    if let Some(values) = matches.get_many::<String>(&argument.name) {
                        for value in values {
                            normalized.push(format!("--{long}"));
                            normalized.push(value.clone());
                        }
                    }
                } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                    normalized.push(format!("--{long}"));
                    normalized.push(value.clone());
                }
                continue;
            }

            if argument.multiple {
                if let Some(values) = matches.get_many::<String>(&argument.name) {
                    normalized.extend(values.cloned());
                }
            } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                normalized.push(value.clone());
            }
        }

        Ok(normalized)
    }

    pub fn normalize_arguments_from_matches(
        command: &PluginCommand,
        matches: &ArgMatches,
    ) -> Vec<String> {
        let mut normalized = Vec::new();
        for argument in &command.arguments {
            if let Some(long) = &argument.long {
                if matches.value_source(&argument.name).is_none() {
                    continue;
                }
                if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
                    if matches.get_flag(&argument.name) {
                        normalized.push(format!("--{long}"));
                    }
                    continue;
                }
                if argument.multiple {
                    if let Some(values) = matches.get_many::<String>(&argument.name) {
                        for value in values {
                            normalized.push(format!("--{long}"));
                            normalized.push(value.clone());
                        }
                    }
                } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                    normalized.push(format!("--{long}"));
                    normalized.push(value.clone());
                }
                continue;
            }

            if argument.multiple {
                if let Some(values) = matches.get_many::<String>(&argument.name) {
                    normalized.extend(values.cloned());
                }
            } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                normalized.push(value.clone());
            }
        }
        normalized
    }

    pub fn augment_clap_command(&self, root: Command) -> Result<Command> {
        let mut root = root;
        for command in &self.commands {
            for path in std::iter::once(&command.canonical_path).chain(command.aliases.iter()) {
                insert_plugin_path(&mut root, path, &command.schema)?;
            }
        }
        Ok(root)
    }
}

pub fn selected_subcommand_path(matches: &ArgMatches) -> (Vec<String>, &ArgMatches) {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, next)) = current.subcommand() {
        path.push(name.to_string());
        current = next;
    }
    (path, current)
}

fn insert_plugin_path(root: &mut Command, path: &[String], schema: &PluginCommand) -> Result<()> {
    if path.is_empty() {
        bail!("plugin command path cannot be empty");
    }

    if path.len() == 1 {
        let updated = std::mem::replace(root, Command::new("bmux-temp-root")).subcommand(
            build_plugin_leaf_command(path.last().expect("leaf exists"), schema)?,
        );
        *root = updated;
        return Ok(());
    }

    let head = &path[0];
    let tail = &path[1..];
    if root.find_subcommand(head).is_none() {
        let updated = std::mem::replace(root, Command::new("bmux-temp-root"))
            .subcommand(build_plugin_namespace_command(head));
        *root = updated;
    }
    let child = root.find_subcommand_mut(head).with_context(|| {
        format!(
            "missing clap namespace for plugin path '{}' after namespace creation",
            path.join(" ")
        )
    })?;
    if tail.len() == 1 {
        let updated = std::mem::replace(child, Command::new("bmux-temp-child")).subcommand(
            build_plugin_leaf_command(tail.last().expect("leaf exists"), schema)?,
        );
        *child = updated;
        return Ok(());
    }

    insert_plugin_path(child, tail, schema)
}

fn build_plugin_namespace_command(name: &str) -> Command {
    Command::new(leak_string(name))
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
}

fn build_plugin_leaf_command(name: &str, schema: &PluginCommand) -> Result<Command> {
    let mut command = Command::new(leak_string(name))
        .about(leak_string(&schema.summary))
        .disable_help_subcommand(true);
    if let Some(description) = &schema.description {
        command = command.long_about(leak_string(description));
    }
    for argument in &schema.arguments {
        command = command.arg(build_clap_arg(argument)?);
    }
    Ok(command)
}

fn build_clap_arg(argument: &PluginCommandArgument) -> Result<Arg> {
    let mut arg = Arg::new(leak_string(&argument.name)).required(argument.required);
    if let Some(position) = argument.position {
        arg = arg.index(position + 1);
    }
    if let Some(long) = &argument.long {
        arg = arg.long(leak_string(long));
    }
    if let Some(short) = argument.short {
        arg = arg.short(short);
    }
    if let Some(summary) = &argument.summary {
        arg = arg.help(summary);
    }
    if let Some(value_name) = &argument.value_name {
        arg = arg.value_name(leak_string(value_name));
    }
    if argument.multiple {
        arg = arg.action(ArgAction::Append);
    } else if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
        arg = arg.action(ArgAction::SetTrue);
    } else {
        arg = arg.action(ArgAction::Set);
    }
    if argument.trailing_var_arg {
        arg = arg.trailing_var_arg(true);
    }
    if argument.allow_hyphen_values {
        arg = arg.allow_hyphen_values(true);
    }
    match &argument.kind {
        PluginCommandArgumentKind::Integer => {
            arg = arg.value_parser(clap::value_parser!(i64));
        }
        PluginCommandArgumentKind::Choice => {
            let leaked = argument
                .choice_values
                .iter()
                .map(|value| Box::leak(value.clone().into_boxed_str()) as &'static str)
                .collect::<Vec<_>>();
            arg = arg.value_parser(leaked);
        }
        PluginCommandArgumentKind::String
        | PluginCommandArgumentKind::Boolean
        | PluginCommandArgumentKind::Path => {}
    }
    Ok(arg)
}

fn leak_string(value: &str) -> &'static str {
    Box::leak(value.to_string().into_boxed_str())
}

fn validate_path_collision(
    path: &[String],
    reserved: &BTreeSet<Vec<String>>,
    claimed: &BTreeMap<Vec<String>, (String, String)>,
    plugin_id: &str,
    command_name: &str,
) -> Result<()> {
    if reserved.contains(path) {
        bail!(
            "plugin '{plugin_id}' command '{command_name}' collides with built-in CLI path '{}')",
            path.join(" ")
        );
    }
    if let Some((owner_plugin, owner_command)) = claimed.get(path) {
        bail!(
            "plugin '{plugin_id}' command '{command_name}' collides with plugin '{owner_plugin}' command '{owner_command}' on CLI path '{}'",
            path.join(" ")
        );
    }
    Ok(())
}

fn validate_prefix_collision(
    path: &[String],
    reserved: &BTreeSet<Vec<String>>,
    claimed: &BTreeMap<Vec<String>, (String, String)>,
    plugin_id: &str,
    command_name: &str,
) -> Result<()> {
    for reserved_path in reserved {
        if is_prefix_collision(path, reserved_path) {
            bail!(
                "plugin '{plugin_id}' command '{command_name}' creates ambiguous CLI nesting with built-in path '{}'",
                reserved_path.join(" ")
            );
        }
    }
    for (claimed_path, (owner_plugin, owner_command)) in claimed {
        if is_prefix_collision(path, claimed_path) {
            bail!(
                "plugin '{plugin_id}' command '{command_name}' creates ambiguous CLI nesting with plugin '{owner_plugin}' command '{owner_command}' on path '{}'",
                claimed_path.join(" ")
            );
        }
    }
    Ok(())
}

fn is_prefix_collision(left: &[String], right: &[String]) -> bool {
    left != right && (left.starts_with(right) || right.starts_with(left))
}

#[cfg(test)]
mod tests {
    use super::PluginCommandRegistry;
    use crate::cli::Cli;
    use bmux_config::BmuxConfig;
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use clap::{Command, CommandFactory};
    use std::path::Path;

    fn config_with_enabled(plugin_id: &str) -> BmuxConfig {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push(plugin_id.to_string());
        config
    }

    #[test]
    fn resolves_nested_aliases_by_longest_prefix() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "example.plugin"
name = "Example"
version = "0.1.0"
entry = "plugin.dylib"

[[commands]]
name = "roles"
path = ["acl", "list"]
aliases = [["acl", "roles"]]
summary = "list"
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
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        let commands =
            PluginCommandRegistry::build(&config_with_enabled("example.plugin"), &registry)
                .expect("command registry should build");
        let resolved = commands
            .resolve(&["acl".into(), "roles".into(), "dev".into()])
            .expect("command should resolve");
        assert_eq!(resolved.command_name, "roles");
        assert_eq!(resolved.arguments, vec!["dev"]);
    }

    #[test]
    fn plugin_aliases_build_without_collision_for_session_namespace() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "policy.plugin"
name = "Policy"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "roles"
path = ["roles"]
aliases = [["session", "roles"]]
summary = "list"
execution = "provider_exec"
expose_in_cli = true

[[commands]]
name = "assign"
path = ["assign"]
aliases = [["session", "assign"]]
summary = "assign"
execution = "provider_exec"
expose_in_cli = true

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        PluginCommandRegistry::build(&config_with_enabled("policy.plugin"), &registry)
            .expect("policy command registry should build");
    }

    #[test]
    fn plugin_aliases_build_without_collision_for_dynamic_namespace() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "workspace.plugin"
name = "Workspace"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "item-open"
path = ["item-open"]
aliases = [["item", "open"]]
summary = "open"
execution = "provider_exec"
expose_in_cli = true

[[commands]]
name = "item-focus"
path = ["item-focus"]
aliases = [["item", "focus"]]
summary = "focus"
execution = "provider_exec"
expose_in_cli = true

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        PluginCommandRegistry::build(&config_with_enabled("workspace.plugin"), &registry)
            .expect("workspace command registry should build");
    }

    #[test]
    fn plugin_cannot_claim_current_static_core_command_path() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "example.plugin"
name = "Example"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "new-session"
path = ["new-session"]
summary = "new"
execution = "provider_exec"
expose_in_cli = true

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");

        let error = PluginCommandRegistry::build(&config_with_enabled("example.plugin"), &registry)
            .expect_err("plugin should not shadow active static core command path");
        assert!(error.to_string().contains("new-session"));
    }

    #[test]
    fn augment_clap_command_creates_missing_namespace_roots() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "workspace.plugin"
name = "Workspace"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "item-open"
path = ["item-open"]
aliases = [["item", "open"]]
summary = "open"
execution = "provider_exec"
expose_in_cli = true

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        let commands =
            PluginCommandRegistry::build(&config_with_enabled("workspace.plugin"), &registry)
                .expect("workspace command registry should build");

        let clap = commands
            .augment_clap_command(Command::new("bmux"))
            .expect("dynamic namespace should be created");
        let matches = clap
            .try_get_matches_from(["bmux", "item", "open"])
            .expect("dynamic namespace path should parse");
        let (path, _) = super::selected_subcommand_path(&matches);
        assert_eq!(path, vec!["item".to_string(), "open".to_string()]);
    }

    #[test]
    fn augment_clap_command_extends_existing_session_namespace() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "policy.plugin"
name = "Policy"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "roles"
path = ["roles"]
aliases = [["session", "roles"]]
summary = "list"
execution = "provider_exec"
expose_in_cli = true

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        let commands =
            PluginCommandRegistry::build(&config_with_enabled("policy.plugin"), &registry)
                .expect("policy command registry should build");

        let clap = commands
            .augment_clap_command(Cli::command())
            .expect("existing session namespace should be extended");
        let matches = clap
            .try_get_matches_from(["bmux", "session", "roles"])
            .expect("plugin session alias should parse under mixed namespace");
        let (path, _) = super::selected_subcommand_path(&matches);
        assert_eq!(path, vec!["session".to_string(), "roles".to_string()]);
    }
}
