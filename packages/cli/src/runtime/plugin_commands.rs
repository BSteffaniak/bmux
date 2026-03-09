use super::built_in_commands::reserved_built_in_paths;
use anyhow::{Result, bail};
use bmux_config::BmuxConfig;
use bmux_plugin::PluginRegistry;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct ResolvedPluginCommand {
    pub plugin_id: String,
    pub command_name: String,
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RegisteredPluginCommand {
    pub plugin_id: String,
    pub command_name: String,
    pub canonical_path: Vec<String>,
    pub aliases: Vec<Vec<String>>,
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
            })
    }
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
}
