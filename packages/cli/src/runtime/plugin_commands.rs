use anyhow::{Context, Result, bail};
use bmux_config::BmuxConfig;
use bmux_plugin::{PluginDeclaration, PluginRegistry};
use bmux_plugin_sdk::{PluginCommand, PluginCommandArgument, PluginCommandArgumentKind};
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
    help: BTreeMap<Vec<String>, PathHelp>,
    resolved_by_path: BTreeMap<Vec<String>, ResolvedPathEntry>,
    owned_exact_paths: BTreeMap<Vec<String>, String>,
    owned_namespaces: BTreeMap<String, String>,
}

/// Presentation only. These entries never create commands or claim dispatch paths.
#[derive(Debug, Clone)]
struct PathHelp {
    before: Option<String>,
    summary: Option<String>,
    order: Option<usize>,
}

#[derive(Debug, Clone)]
struct ResolvedPathEntry {
    plugin_id: String,
    command_name: String,
    schema: PluginCommand,
}

impl PluginCommandRegistry {
    pub fn build(config: &BmuxConfig, plugins: &PluginRegistry) -> Result<Self> {
        let mut registry = Self::default();
        let enabled = config.plugins.enabled.iter().collect::<BTreeSet<_>>();
        let mut claimed: BTreeMap<Vec<String>, (String, String)> = BTreeMap::new();

        for plugin in plugins.iter() {
            let plugin_id = plugin.declaration.id.as_str().to_string();
            if !enabled.contains(&plugin_id) {
                continue;
            }
            register_plugin_ownership(
                &mut registry.owned_exact_paths,
                &mut registry.owned_namespaces,
                &plugin.declaration,
            )?;
        }

        for plugin in plugins.iter() {
            let plugin_id = plugin.declaration.id.as_str().to_string();
            if !enabled.contains(&plugin_id) {
                continue;
            }

            for command in plugin
                .declaration
                .commands
                .iter()
                .filter(|command| command.expose_in_cli)
            {
                validate_argument_metadata(command)?;
                let canonical = command.canonical_path();
                validate_plugin_owns_path(&plugin.declaration, &canonical, &command.name)?;
                validate_path_collision(
                    &canonical,
                    &claimed,
                    plugin.declaration.id.as_str(),
                    &command.name,
                )?;
                validate_prefix_collision(
                    &canonical,
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
                    validate_plugin_owns_path(&plugin.declaration, alias, &command.name)?;
                    validate_path_collision(
                        alias,
                        &claimed,
                        plugin.declaration.id.as_str(),
                        &command.name,
                    )?;
                    validate_prefix_collision(
                        alias,
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
                let registered = registry
                    .commands
                    .last()
                    .expect("just-pushed plugin command should exist");
                register_resolved_path_entry(
                    &mut registry.resolved_by_path,
                    &registered.canonical_path,
                    registered,
                );
                for alias in &registered.aliases {
                    register_resolved_path_entry(&mut registry.resolved_by_path, alias, registered);
                }
            }
        }

        registry.collect_help(plugins, &enabled)?;
        Ok(registry)
    }

    fn collect_help(
        &mut self,
        plugins: &PluginRegistry,
        enabled: &BTreeSet<&String>,
    ) -> Result<()> {
        for plugin in plugins
            .iter()
            .filter(|plugin| enabled.contains(&plugin.manifest.id))
        {
            for (path, help) in &plugin.manifest.cli_help {
                let segments: Vec<String> = path.split_whitespace().map(str::to_owned).collect();
                anyhow::ensure!(
                    path == &segments.join(" ")
                        && help.before.as_ref().is_none_or(|name| {
                            help.order.is_some_and(|order| order < 999)
                                && !name.is_empty()
                                && !name.contains(char::is_whitespace)
                                && !name.chars().any(char::is_control)
                                && segments.last() != Some(name)
                        })
                        && (help.order.is_some() || help.summary.is_some())
                        && help.summary.as_ref().is_none_or(|summary| {
                            !summary.trim().is_empty() && !summary.chars().any(char::is_control)
                        }),
                    "plugin '{}' declares malformed CLI help metadata",
                    plugin.manifest.id
                );
                let contributes = !segments.is_empty()
                    && self.resolved_by_path.iter().any(|(leaf, entry)| {
                        leaf.starts_with(&segments) && entry.plugin_id == plugin.manifest.id
                    });
                anyhow::ensure!(
                    contributes,
                    "plugin '{}' declares help for unrelated path '{path}'",
                    plugin.manifest.id
                );
                let prior = self.help.insert(
                    segments,
                    PathHelp {
                        before: help.before.clone(),
                        summary: help.summary.clone(),
                        order: help.order,
                    },
                );
                anyhow::ensure!(
                    prior.is_none(),
                    "conflicting help declarations for '{path}'"
                );
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn owner_for_path(&self, path: &[String]) -> Option<String> {
        if let Some(owner) = self.owned_exact_paths.get(path) {
            return Some(owner.clone());
        }
        path.first()
            .and_then(|namespace| self.owner_for_namespace(namespace))
    }

    #[must_use]
    pub fn owner_for_namespace(&self, namespace: &str) -> Option<String> {
        self.owned_namespaces.get(namespace).cloned()
    }

    pub fn resolve(&self, raw: &[String]) -> Option<ResolvedPluginCommand> {
        for prefix_len in (1..=raw.len()).rev() {
            let candidate = raw[..prefix_len].to_vec();
            let Some(entry) = self.resolved_by_path.get(&candidate) else {
                continue;
            };
            return Some(ResolvedPluginCommand {
                plugin_id: entry.plugin_id.clone(),
                command_name: entry.command_name.clone(),
                arguments: raw[prefix_len..].to_vec(),
                schema: entry.schema.clone(),
            });
        }
        None
    }

    pub fn resolve_exact_path(&self, path: &[String]) -> Option<ResolvedPluginCommand> {
        self.resolved_by_path
            .get(path)
            .map(|entry| ResolvedPluginCommand {
                plugin_id: entry.plugin_id.clone(),
                command_name: entry.command_name.clone(),
                arguments: Vec::new(),
                schema: entry.schema.clone(),
            })
    }

    pub fn validate_arguments(
        command: &PluginCommand,
        arguments: &[String],
    ) -> Result<Vec<String>> {
        let clap_command = build_plugin_leaf_command(&command.name, command);
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

        // Trailing argv is caller-owned. Clap consumes `--`; reconstructing
        // values loses that boundary and can reorder options.
        if command
            .arguments
            .iter()
            .any(|argument| argument.trailing_var_arg)
        {
            return Ok(arguments.to_vec());
        }
        let normalized = Self::normalize_arguments_from_matches(command, &matches);
        Ok(normalized)
    }

    pub fn normalize_arguments_from_matches(
        command: &PluginCommand,
        matches: &ArgMatches,
    ) -> Vec<String> {
        let mut normalized = Vec::new();
        for argument in &command.arguments {
            // Read validated raw values: integer and static-host parsers do not
            // store Strings, and a retained host leaf can lack this argument ID.
            let Ok(Some(values)) = matches.try_get_raw(&argument.name) else {
                continue;
            };
            let flag = argument
                .long
                .as_ref()
                .map(|name| format!("--{name}"))
                .or_else(|| argument.short.map(|name| format!("-{name}")));
            if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
                if matches.try_get_one::<bool>(&argument.name).ok().flatten() == Some(&true) {
                    normalized.extend(flag);
                }
                continue;
            }
            for raw in values {
                normalized.extend(flag.iter().cloned());
                let value = raw.to_string_lossy();
                normalized.push(
                    argument
                        .choice_aliases
                        .get(value.as_ref())
                        .cloned()
                        .unwrap_or_else(|| value.into_owned()),
                );
            }
        }
        normalized
    }

    /// Namespace ownership alone does not replace an existing static clap leaf.
    pub fn installs_leaf(&self, root: &Command, path: &[String], plugin_id: &str) -> bool {
        !clap_path_exists(root, path)
            || self
                .owned_exact_paths
                .get(path)
                .is_some_and(|owner| owner == plugin_id)
    }

    pub fn augment_clap_command(&self, root: Command) -> Result<Command> {
        let retained: BTreeSet<_> = self
            .help
            .keys()
            .filter(|path| clap_path_exists(&root, path))
            .cloned()
            .collect();
        let mut root = root;
        for command in &self.commands {
            for path in std::iter::once(&command.canonical_path).chain(command.aliases.iter()) {
                if clap_path_exists(&root, path) {
                    if self.owned_exact_paths.get(path) == Some(&command.plugin_id) {
                        replace_plugin_path(&mut root, path, &command.schema)?;
                    }
                    continue;
                }
                insert_plugin_path(&mut root, path, &command.schema)?;
            }
        }
        Ok(self.apply_help(root, &mut Vec::new(), &retained))
    }

    fn apply_help(
        &self,
        command: Command,
        path: &mut Vec<String>,
        retained: &BTreeSet<Vec<String>>,
    ) -> Command {
        let mut reserved = BTreeSet::new();
        let mut lower_bounds = BTreeMap::<&str, usize>::new();
        for child in command.get_subcommands() {
            path.push(child.get_name().to_owned());
            if let Some(order) = self.help.get(path).and_then(|help| help.order) {
                reserved.insert(order);
                if let Some(anchor) = self.help.get(path).and_then(|help| help.before.as_deref()) {
                    lower_bounds
                        .entry(anchor)
                        .and_modify(|bound| *bound = (*bound).max(order + 1))
                        .or_insert(order + 1);
                }
            }
            path.pop();
        }
        let mut next = 0;
        command.mut_subcommands(|mut child| {
            path.push(child.get_name().to_owned());
            let help = self.help.get(path);
            if !reserved.is_empty() {
                let order = help.and_then(|help| help.order).unwrap_or_else(|| {
                    if let Some(bound) = lower_bounds.get(child.get_name()) {
                        next = next.max(*bound);
                    }
                    while reserved.contains(&next) {
                        next += 1;
                    }
                    next += 1;
                    next - 1
                });
                child = child.display_order(order);
            }
            if let Some(summary) = help.and_then(|help| help.summary.as_ref()) {
                // An installed alias can have its own short description without changing
                // the canonical leaf, retained host grammar, or dispatch identity.
                if !child.has_subcommands()
                    && self.resolved_by_path.get(path).is_some_and(|entry| {
                        entry.schema.canonical_path() != *path
                            && (!retained.contains(path)
                                || self.owned_exact_paths.get(path) == Some(&entry.plugin_id))
                    })
                {
                    child = child.about(summary.clone());
                } else if !retained.contains(path)
                    && child.get_about().is_none()
                    && child.has_subcommands()
                {
                    let auto_help = child.find_subcommand("help").is_none();
                    child = child
                        .about(summary.clone())
                        .subcommand_required(true)
                        .disable_help_subcommand(!auto_help);
                }
            }
            let child = self.apply_help(child, path, retained);
            path.pop();
            child
        })
    }
}

fn register_resolved_path_entry(
    index: &mut BTreeMap<Vec<String>, ResolvedPathEntry>,
    path: &[String],
    command: &RegisteredPluginCommand,
) {
    let previous = index.insert(
        path.to_vec(),
        ResolvedPathEntry {
            plugin_id: command.plugin_id.clone(),
            command_name: command.command_name.clone(),
            schema: command.schema.clone(),
        },
    );
    debug_assert!(
        previous.is_none(),
        "path collisions should be rejected before indexing"
    );
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
            build_plugin_leaf_command(path.last().expect("leaf exists"), schema),
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
            build_plugin_leaf_command(tail.last().expect("leaf exists"), schema),
        );
        *child = updated;
        return Ok(());
    }

    insert_plugin_path(child, tail, schema)
}

// Only an exact path owner may replace a static leaf schema. Unowned static
// paths retain their built-in grammar; global options remain on the parent.
fn replace_plugin_path(root: &mut Command, path: &[String], schema: &PluginCommand) -> Result<()> {
    let (head, tail) = path.split_first().context("empty plugin command path")?;
    let child = root
        .find_subcommand_mut(head)
        .context("missing owned command path")?;
    if tail.is_empty() {
        *child = build_plugin_leaf_command(head, schema).display_order(child.get_display_order());
        Ok(())
    } else {
        replace_plugin_path(child, tail, schema)
    }
}

fn build_plugin_namespace_command(name: &str) -> Command {
    Command::new(name.to_owned())
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
}

fn build_plugin_leaf_command(name: &str, schema: &PluginCommand) -> Command {
    let mut command = Command::new(name.to_owned())
        .about(schema.summary.clone())
        .disable_help_subcommand(true);
    if let Some(description) = &schema.description {
        command = command.long_about(description.clone());
    }
    if schema.forward_help {
        // The command parses its own arguments and answers help itself.
        command = command.disable_help_flag(true);
    }
    for argument in &schema.arguments {
        command = command.arg(build_clap_arg(argument));
    }
    command
}

fn validate_argument_metadata(command: &PluginCommand) -> Result<()> {
    let has_trailing = command.arguments.iter().any(|arg| arg.trailing_var_arg);
    for arg in &command.arguments {
        for other in &arg.conflicts_with {
            if other == &arg.name
                || !command
                    .arguments
                    .iter()
                    .any(|sibling| &sibling.name == other)
            {
                bail!(
                    "invalid conflict '{other}' for {}:{}",
                    command.name,
                    arg.name
                );
            }
        }
        for (alias, canonical) in &arg.choice_aliases {
            if arg.kind != PluginCommandArgumentKind::Choice
                || alias.is_empty()
                || arg.choice_values.contains(alias)
                || !arg.choice_values.contains(canonical)
            {
                bail!(
                    "invalid choice alias '{alias}' for {}:{}",
                    command.name,
                    arg.name
                );
            }
        }
        if let Some(value) = &arg.default_value {
            let valid = match arg.kind {
                PluginCommandArgumentKind::Integer => value.parse::<i64>().is_ok(),
                PluginCommandArgumentKind::Choice => arg.choice_values.contains(value),
                PluginCommandArgumentKind::Boolean => false,
                PluginCommandArgumentKind::Path | PluginCommandArgumentKind::String => true,
            };
            if !valid {
                bail!("invalid default for {}:{}", command.name, arg.name);
            }
        }
        if has_trailing && (arg.default_value.is_some() || !arg.choice_aliases.is_empty()) {
            bail!(
                "{} cannot normalize defaults or aliases with caller-owned trailing argv",
                command.name
            );
        }
    }
    if command.forward_help
        && !(command.arguments.len() == 1
            && command.arguments[0].trailing_var_arg
            && command.arguments[0].position.is_some())
    {
        bail!(
            "{} forwards help, so it must take only one trailing positional argument",
            command.name
        );
    }
    Ok(())
}

fn build_clap_arg(argument: &PluginCommandArgument) -> Arg {
    let mut arg = Arg::new(argument.name.clone())
        .required(argument.required)
        .conflicts_with_all(argument.conflicts_with.iter().cloned());
    if let Some(position) = argument.position {
        arg = arg.index(position + 1);
    }
    if let Some(long) = &argument.long {
        arg = arg.long(long.clone());
    }
    if let Some(short) = argument.short {
        arg = arg.short(short);
    }
    if let Some(summary) = &argument.summary {
        arg = arg.help(summary);
    }
    if let Some(value_name) = &argument.value_name {
        arg = arg.value_name(value_name.clone());
    }
    if let Some(value) = &argument.default_value {
        arg = arg.default_value(value.clone());
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
            let choices = argument.choice_values.iter().map(|value| {
                clap::builder::PossibleValue::new(value.clone()).aliases(
                    argument
                        .choice_aliases
                        .iter()
                        .filter(|(_, canonical)| *canonical == value)
                        .map(|(alias, _)| alias.clone()),
                )
            });
            arg = arg.value_parser(clap::builder::PossibleValuesParser::new(choices));
        }
        PluginCommandArgumentKind::String
        | PluginCommandArgumentKind::Boolean
        | PluginCommandArgumentKind::Path => {}
    }
    arg
}

fn validate_path_collision(
    path: &[String],
    claimed: &BTreeMap<Vec<String>, (String, String)>,
    plugin_id: &str,
    command_name: &str,
) -> Result<()> {
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
    claimed: &BTreeMap<Vec<String>, (String, String)>,
    plugin_id: &str,
    command_name: &str,
) -> Result<()> {
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

fn clap_path_exists(root: &Command, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    let mut current = root;
    for segment in path {
        let Some(next) = current.find_subcommand(segment) else {
            return false;
        };
        current = next;
    }
    true
}

#[allow(clippy::suspicious_operation_groupings)] // Intentional: checks two distinct slices aren't equal AND one is a prefix
fn is_prefix_collision(left: &[String], right: &[String]) -> bool {
    left != right && (left.starts_with(right) || right.starts_with(left))
}

fn register_plugin_ownership(
    owned_exact_paths: &mut BTreeMap<Vec<String>, String>,
    owned_namespaces: &mut BTreeMap<String, String>,
    declaration: &PluginDeclaration,
) -> Result<()> {
    let plugin_id = declaration.id.as_str().to_string();
    for namespace in &declaration.owns_namespaces {
        if let Some(existing) = owned_namespaces.get(namespace)
            && existing != &plugin_id
        {
            bail!(
                "plugin '{plugin_id}' ownership conflict: namespace '{namespace}' already owned by plugin '{existing}'"
            );
        }
        owned_namespaces.insert(namespace.clone(), plugin_id.clone());
    }

    for path in &declaration.owns_paths {
        if let Some(existing) = owned_exact_paths.get(&path.0)
            && existing != &plugin_id
        {
            bail!(
                "plugin '{plugin_id}' ownership conflict: path '{}' already owned by plugin '{existing}'",
                path.0.join(" ")
            );
        }
        owned_exact_paths.insert(path.0.clone(), plugin_id.clone());
    }
    Ok(())
}

fn validate_plugin_owns_path(
    declaration: &PluginDeclaration,
    path: &[String],
    command_name: &str,
) -> Result<()> {
    let exact = declaration
        .owns_paths
        .iter()
        .any(|candidate| candidate.matches(path));
    let namespace_owned = path
        .first()
        .is_some_and(|segment| declaration.owns_namespaces.contains(segment));
    if exact || namespace_owned {
        return Ok(());
    }
    bail!(
        "plugin '{}' command '{}' path '{}' is not within declared ownership (owns_paths/owns_namespaces)",
        declaration.id.as_str(),
        command_name,
        path.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::{PluginCommandRegistry, selected_subcommand_path, validate_argument_metadata};
    use bmux_cli_schema::Cli;
    use bmux_config::BmuxConfig;
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use bmux_plugin_sdk::{PluginCommand, PluginCommandArgument, PluginCommandArgumentKind};
    use clap::{Command, CommandFactory};
    use std::path::Path;

    fn config_with_enabled(plugin_id: &str) -> BmuxConfig {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push(plugin_id.to_string());
        config
    }

    #[test]
    fn namespace_help_uses_enabled_owner_metadata_and_reserves_sibling_slots() {
        let mut plugins = PluginRegistry::new();
        plugins
            .register_bundled_manifest(
                r#"
id = "example.fixture"
name = "Fixture"
version = "1.0.0"
owns_namespaces = ["tools"]
[cli_help]
tools = { summary = "Fixture tools", order = 9, before = "existing" }
"tools inspect" = { order = 0 }
"tools view" = { summary = "Alias for inspect" }
[[commands]]
name = "write"
path = ["tools", "write"]
summary = "Write data"
expose_in_cli = true
[[commands]]
name = "inspect"
path = ["tools", "inspect"]
summary = "Inspect data"
aliases = [["tools", "view"]]
expose_in_cli = true
"#,
            )
            .unwrap();
        let root =
            Command::new("fixture").subcommand(Command::new("existing").about("Existing command"));
        let disabled = PluginCommandRegistry::build(&BmuxConfig::default(), &plugins)
            .unwrap()
            .augment_clap_command(root.clone())
            .unwrap();
        assert!(disabled.find_subcommand("tools").is_none());
        let tree = PluginCommandRegistry::build(&config_with_enabled("example.fixture"), &plugins)
            .unwrap()
            .augment_clap_command(root)
            .unwrap();
        let render = |args: &[&str]| {
            tree.clone()
                .try_get_matches_from(args)
                .unwrap_err()
                .to_string()
        };
        let root_help = render(&["fixture", "--help"]);
        assert!(
            root_help.find("Fixture tools").unwrap() < root_help.find("Existing command").unwrap()
        );
        let help = render(&["fixture", "tools", "--help"]);
        assert!(help.starts_with("Fixture tools"));
        assert!(help.contains("Alias for inspect"));
        assert!(render(&["fixture", "help", "tools", "view"]).starts_with("Alias for inspect"));
        assert!(help.contains("tools <COMMAND>"));
        assert!(help.find("Inspect data").unwrap() < help.find("Write data").unwrap());
        assert!(render(&["fixture", "tools", "help", "inspect"]).contains("Inspect data"));
    }

    fn dynamic_help_registry(help: &str) -> anyhow::Result<PluginCommandRegistry> {
        let manifest = PluginManifest::from_toml_str(&format!(
            r#"
id = "example.dynamic"
name = "Example"
version = "1.0.0"
entry = "fixture.dylib"
owns_paths = [["tools", "inspect"]]
[[commands]]
name = "inspect"
path = ["tools", "inspect"]
summary = "Inspect"
expose_in_cli = true
[cli_help]
{help}
"#
        ))?;
        let mut plugins = PluginRegistry::new();
        plugins.register_manifest(Path::new("fixture/plugin.toml"), manifest)?;
        PluginCommandRegistry::build(&config_with_enabled("example.dynamic"), &plugins)
    }

    #[test]
    fn dynamic_help_rejects_malformed_and_unrelated_entries() {
        for (help, expected) in [
            (r#"tools = { summary = " " }"#, "malformed CLI help"),
            (r#"tools = { summary = "one\ntwo" }"#, "malformed CLI help"),
            ("tools = {}", "malformed CLI help"),
            (r#""tools  inspect" = { order = 0 }"#, "malformed CLI help"),
            (r#""tools other" = { order = 0 }"#, "unrelated path"),
            ("tools = { unexpected = 1 }", "unknown field"),
        ] {
            let error = dynamic_help_registry(help).unwrap_err().to_string();
            assert!(error.contains(expected), "{help}: {error}");
        }
    }

    #[test]
    fn dynamic_help_preserves_optional_retained_namespace_grammar() {
        let registry =
            dynamic_help_registry(r#"tools = { summary = "Generated namespace" }"#).unwrap();
        let root = Command::new("host").subcommand(Command::new("tools"));
        let tree = registry.augment_clap_command(root).unwrap();
        assert!(tree.find_subcommand("tools").unwrap().get_about().is_none());
        assert!(tree.try_get_matches_from(["host", "tools"]).is_ok());
    }

    #[test]
    fn exact_owner_schema_replaces_static_leaf_and_preserves_globals() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "example.plugin"
name = "Example"
version = "0.1.0"
entry = "plugin.dylib"
owns_paths = [["existing"]]
[[commands]]
name = "existing"
path = ["existing"]
summary = "owned"
expose_in_cli = true
[[commands.arguments]]
name = "extended"
long = "extended"
kind = "boolean"
required = false
[plugin_api]
minimum = "1.0"
[native_abi]
minimum = "1.0"
"#,
        )
        .unwrap();
        let mut plugins = PluginRegistry::new();
        plugins
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .unwrap();
        let registry =
            PluginCommandRegistry::build(&config_with_enabled("example.plugin"), &plugins).unwrap();
        let root = Command::new("bmux")
            .arg(clap::Arg::new("runtime").long("runtime").global(true))
            .subcommand(Command::new("existing").arg(clap::Arg::new("old").long("old")));
        let extended = registry.augment_clap_command(root.clone()).unwrap();
        let matches = extended
            .try_get_matches_from(["bmux", "--runtime", "test", "existing", "--extended"])
            .unwrap();
        assert_eq!(
            matches.get_one::<String>("runtime").map(String::as_str),
            Some("test")
        );
        assert!(
            matches
                .subcommand_matches("existing")
                .unwrap()
                .get_flag("extended")
        );
        // The core-only parsing route retains its untouched static grammar.
        assert!(
            root.try_get_matches_from(["bmux", "existing", "--extended"])
                .is_err()
        );
    }

    #[test]
    fn resolves_nested_aliases_by_longest_prefix() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "example.plugin"
name = "Example"
version = "0.1.0"
entry = "plugin.dylib"
owns_namespaces = ["acl"]

[[commands]]
name = "roles"
path = ["acl", "list"]
aliases = [["acl", "roles"]]
summary = "list"
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
owns_namespaces = ["roles", "assign", "session"]

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
    fn normalize_arguments_ignores_ids_from_a_different_selected_leaf() {
        let command = bmux_plugin_sdk::PluginCommand::new("first", "first command").argument(
            bmux_plugin_sdk::PluginCommandArgument::positional(
                "args",
                bmux_plugin_sdk::PluginCommandArgumentKind::String,
            )
            .multiple(true),
        );
        let matches = Command::new("root")
            .subcommand(Command::new("second"))
            .try_get_matches_from(["root", "second"])
            .expect("parse second leaf");
        let (_, leaf) = selected_subcommand_path(&matches);

        assert_eq!(
            PluginCommandRegistry::normalize_arguments_from_matches(&command, leaf),
            [] as [std::string::String; 0]
        );
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
owns_namespaces = ["item", "item-open", "item-focus"]

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
    fn plugin_can_claim_current_static_core_command_path_when_owned() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "example.plugin"
name = "Example"
version = "0.1.0"
entry = "plugin.dylib"
required_capabilities = ["bmux.commands"]
owns_namespaces = ["new-session"]

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

        let built = PluginCommandRegistry::build(&config_with_enabled("example.plugin"), &registry)
            .expect("plugin should be allowed to own built-in command path");
        let resolved = built
            .resolve_exact_path(&["new-session".to_string()])
            .expect("plugin command should resolve on owned built-in path");
        assert_eq!(resolved.plugin_id, "example.plugin");
    }

    #[test]
    fn forwarded_help_reaches_the_command_arguments() {
        let mut command = PluginCommand::new("drive", "drive things").forward_help(true);
        command.arguments.push(PluginCommandArgument {
            name: "args".into(),
            kind: PluginCommandArgumentKind::String,
            choice_values: Vec::new(),
            choice_aliases: std::collections::BTreeMap::new(),
            default_value: None,
            conflicts_with: Vec::new(),
            position: Some(0),
            long: None,
            short: None,
            required: false,
            multiple: true,
            trailing_var_arg: true,
            allow_hyphen_values: true,
            summary: None,
            value_name: None,
        });
        validate_argument_metadata(&command).expect("one trailing positional may forward help");
        for help in ["--help", "-h"] {
            let arguments = PluginCommandRegistry::validate_arguments(&command, &[help.to_owned()])
                .expect("help is an argument, not a request to the CLI");
            assert_eq!(arguments, vec![help.to_owned()]);
        }
        let mut flagged = command.clone();
        flagged.arguments[0].trailing_var_arg = false;
        assert!(validate_argument_metadata(&flagged).is_err());
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
owns_namespaces = ["item", "item-open"]

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
owns_namespaces = ["roles", "session"]

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

    #[test]
    fn ownership_exact_path_takes_precedence_over_namespace() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "owner.plugin"
name = "Owner"
version = "0.1.0"
entry = "owner.dylib"
owns_namespaces = ["workspace"]
owns_paths = [["workspace", "doctor"]]
required_capabilities = ["bmux.commands"]

[[commands]]
name = "doctor"
path = ["workspace", "doctor"]
summary = "doctor"
execution = "provider_exec"
expose_in_cli = true

[[commands]]
name = "probe"
path = ["workspace", "probe"]
summary = "probe"
execution = "provider_exec"
expose_in_cli = true
"#,
        )
        .expect("manifest should parse");
        let mut plugins = PluginRegistry::new();
        plugins
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        let registry = PluginCommandRegistry::build(&config_with_enabled("owner.plugin"), &plugins)
            .expect("registry should build");
        assert_eq!(
            registry.owner_for_path(&["workspace".to_string(), "doctor".to_string()]),
            Some("owner.plugin".to_string())
        );
        assert_eq!(
            registry.owner_for_path(&["workspace".to_string(), "probe".to_string()]),
            Some("owner.plugin".to_string())
        );
        assert_eq!(
            registry.owner_for_path(&["workspace".to_string(), "doctor".to_string()]),
            Some("owner.plugin".to_string())
        );
    }

    #[test]
    fn build_rejects_command_outside_declared_ownership() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "invalid.plugin"
name = "Invalid"
version = "0.1.0"
entry = "invalid.dylib"
owns_namespaces = ["window"]
required_capabilities = ["bmux.commands"]

[[commands]]
name = "bad"
path = ["logs", "tail"]
summary = "bad"
execution = "provider_exec"
expose_in_cli = true
"#,
        )
        .expect("manifest should parse");
        let mut plugins = PluginRegistry::new();
        plugins
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/plugin.toml"),
                manifest,
            )
            .expect("manifest should register");
        let error = PluginCommandRegistry::build(&config_with_enabled("invalid.plugin"), &plugins)
            .expect_err("registry build should fail for unowned command path");
        assert!(error.to_string().contains("not within declared ownership"));
    }

    #[test]
    fn build_rejects_namespace_ownership_conflicts() {
        let first = PluginManifest::from_toml_str(
            r#"
id = "first.plugin"
name = "First"
version = "0.1.0"
entry = "first.dylib"
owns_namespaces = ["logs"]
required_capabilities = ["bmux.commands"]

[[commands]]
name = "list"
path = ["logs", "list"]
summary = "list"
execution = "provider_exec"
expose_in_cli = true
"#,
        )
        .expect("first manifest should parse");
        let second = PluginManifest::from_toml_str(
            r#"
id = "second.plugin"
name = "Second"
version = "0.1.0"
entry = "second.dylib"
owns_namespaces = ["logs"]
required_capabilities = ["bmux.commands"]

[[commands]]
name = "tail"
path = ["logs", "tail"]
summary = "tail"
execution = "provider_exec"
expose_in_cli = true
"#,
        )
        .expect("second manifest should parse");

        let mut plugins = PluginRegistry::new();
        plugins
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/first.toml"),
                first,
            )
            .expect("first should register");
        plugins
            .register_manifest_from_root(
                Path::new("/plugins"),
                Path::new("/plugins/second.toml"),
                second,
            )
            .expect("second should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled = vec!["first.plugin".to_string(), "second.plugin".to_string()];
        let error = PluginCommandRegistry::build(&config, &plugins)
            .expect_err("registry build should fail for namespace ownership conflict");
        assert!(error.to_string().contains("ownership conflict"));
    }
}
