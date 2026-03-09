use super::built_in_commands::reserved_built_in_paths;
use anyhow::{Context, Result, bail};
use bmux_config::BmuxConfig;
use bmux_plugin::{
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginRegistry,
};
use clap::{Arg, ArgAction, Command};
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
                            normalized.push(value.to_string());
                        }
                    }
                } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                    normalized.push(format!("--{long}"));
                    normalized.push(value.to_string());
                }
                continue;
            }

            if argument.multiple {
                if let Some(values) = matches.get_many::<String>(&argument.name) {
                    normalized.extend(values.cloned());
                }
            } else if let Some(value) = matches.get_one::<String>(&argument.name) {
                normalized.push(value.to_string());
            }
        }

        Ok(normalized)
    }
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
    use bmux_config::BmuxConfig;
    use bmux_plugin::{PluginManifest, PluginRegistry};
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
name = "permissions"
path = ["acl", "list"]
aliases = [["acl", "permissions"]]
summary = "list"
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
            .resolve(&["acl".into(), "permissions".into(), "dev".into()])
            .expect("command should resolve");
        assert_eq!(resolved.command_name, "permissions");
        assert_eq!(resolved.arguments, vec!["dev"]);
    }

    #[test]
    fn shipped_permissions_aliases_build_without_collision() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "bmux.permissions"
name = "Permissions"
version = "0.1.0"
entry = "plugin.dylib"
required_host_scopes = ["bmux.commands"]

[[commands]]
name = "permissions"
path = ["permissions"]
aliases = [["session", "permissions"]]
summary = "list"
execution = "host_callback"
expose_in_cli = true

[[commands]]
name = "grant"
path = ["grant"]
aliases = [["session", "grant"]]
summary = "grant"
execution = "host_callback"
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
        PluginCommandRegistry::build(&config_with_enabled("bmux.permissions"), &registry)
            .expect("permissions command registry should build");
    }

    #[test]
    fn shipped_windows_aliases_build_without_collision() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "bmux.windows"
name = "Windows"
version = "0.1.0"
entry = "plugin.dylib"
required_host_scopes = ["bmux.commands"]

[[commands]]
name = "new-window"
path = ["new-window"]
aliases = [["window", "new"]]
summary = "new"
execution = "host_callback"
expose_in_cli = true

[[commands]]
name = "switch-window"
path = ["switch-window"]
aliases = [["window", "switch"]]
summary = "switch"
execution = "host_callback"
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
        PluginCommandRegistry::build(&config_with_enabled("bmux.windows"), &registry)
            .expect("windows command registry should build");
    }
}
