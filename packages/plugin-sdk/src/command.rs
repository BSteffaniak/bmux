use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionKind {
    /// Run the command in the plugin provider process.
    ProviderExec,
    /// Run the command as a provider-owned background task.
    BackgroundTask,
    /// Run the command in the process that invoked it.
    ///
    /// Attach-local commands use this when they need caller-local facilities
    /// such as the active prompt/modal host.
    CallerProcess,
    /// Run the command as a runtime hot-path hook.
    RuntimeHook,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<Vec<String>>,
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PluginCommandArgument>,
    #[serde(default = "default_execution_kind")]
    pub execution: CommandExecutionKind,
    #[serde(default = "default_expose_in_cli")]
    pub expose_in_cli: bool,
    /// Whether this command is safe to fire repeatedly under
    /// keyboard auto-repeat. Navigation and resize commands set this
    /// to `true`; mutating / destructive / one-shot commands leave it
    /// `false`. The attach runtime consults this via the plugin
    /// registry to decide whether to filter `KeyEventKind::Repeat`
    /// events for plugin-command keybindings.
    #[serde(default)]
    pub accepts_repeat: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandArgument {
    pub name: String,
    pub kind: PluginCommandArgumentKind,
    #[serde(default, alias = "values")]
    pub choice_values: Vec<String>,
    /// Hidden input spelling -> canonical entry in `choice_values`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub choice_aliases: BTreeMap<String, String>,
    /// CLI default for a value-taking argument (not caller-owned trailing argv).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// Sibling argument names that cannot be supplied with this argument.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts_with: Vec<String>,
    #[serde(default)]
    pub position: Option<usize>,
    #[serde(default)]
    pub long: Option<String>,
    #[serde(default)]
    pub short: Option<char>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default)]
    pub trailing_var_arg: bool,
    #[serde(default)]
    pub allow_hyphen_values: bool,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub value_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCommandArgumentKind {
    String,
    Integer,
    Boolean,
    Path,
    Choice,
}

const fn default_execution_kind() -> CommandExecutionKind {
    CommandExecutionKind::ProviderExec
}

const fn default_expose_in_cli() -> bool {
    false
}

impl PluginCommand {
    #[must_use]
    pub fn new(name: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: Vec::new(),
            aliases: Vec::new(),
            summary: summary.into(),
            description: None,
            arguments: Vec::new(),
            execution: default_execution_kind(),
            expose_in_cli: default_expose_in_cli(),
            accepts_repeat: false,
        }
    }

    #[must_use]
    pub fn path(mut self, path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.path = path.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn alias(mut self, path: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.aliases
            .push(path.into_iter().map(Into::into).collect());
        self
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn argument(mut self, argument: PluginCommandArgument) -> Self {
        self.arguments.push(argument);
        self
    }

    #[must_use]
    pub const fn execution(mut self, execution: CommandExecutionKind) -> Self {
        self.execution = execution;
        self
    }

    #[must_use]
    pub const fn expose_in_cli(mut self, expose_in_cli: bool) -> Self {
        self.expose_in_cli = expose_in_cli;
        self
    }

    #[must_use]
    pub const fn accepts_repeat(mut self, accepts_repeat: bool) -> Self {
        self.accepts_repeat = accepts_repeat;
        self
    }

    #[must_use]
    pub fn canonical_path(&self) -> Vec<String> {
        if self.path.is_empty() {
            vec![self.name.clone()]
        } else {
            self.path.clone()
        }
    }

    #[must_use]
    pub fn cli_paths(&self) -> Vec<Vec<String>> {
        let canonical = self.canonical_path();
        let mut seen = BTreeSet::new();
        let mut paths = Vec::new();

        if seen.insert(canonical.clone()) {
            paths.push(canonical);
        }

        for alias in &self.aliases {
            if seen.insert(alias.clone()) {
                paths.push(alias.clone());
            }
        }

        paths
    }

    /// Normalize inline option values (`--long=value`, `-s=value`) against
    /// this command's declared argument schema into the separated form
    /// (`--long value`) that every plugin command receives.
    ///
    /// Only options declared with a value (non-boolean kinds) are split, so
    /// the value keeps any further `=` characters. Undeclared options,
    /// boolean flags, values consumed by a preceding option, and everything
    /// after a `--` separator are forwarded unchanged; when the command has a
    /// trailing variadic argument, normalization also stops at the first
    /// positional token so caller-owned trailing argv is never rewritten.
    /// Returns `None` when no argument needed normalization.
    #[must_use]
    pub fn normalize_inline_option_values(&self, arguments: &[String]) -> Option<Vec<String>> {
        let has_trailing = self
            .arguments
            .iter()
            .any(|argument| argument.trailing_var_arg);
        let mut normalized: Option<Vec<String>> = None;
        let mut expects_value = false;
        let mut rest = arguments.len();
        for (index, token) in arguments.iter().enumerate() {
            if expects_value {
                expects_value = false;
            } else if token == "--" || (has_trailing && !token.starts_with('-')) {
                rest = index;
                break;
            } else if let Some((option, value)) = token.split_once('=')
                && self.value_option(option).is_some()
            {
                let out = normalized.get_or_insert_with(|| arguments[..index].to_vec());
                out.push(option.to_string());
                out.push(value.to_string());
                continue;
            } else {
                expects_value = self.value_option(token).is_some();
            }
            if let Some(out) = normalized.as_mut() {
                out.push(token.clone());
            }
        }
        if let Some(out) = normalized.as_mut() {
            out.extend(arguments[rest..].iter().cloned());
        }
        normalized
    }

    /// Declared option taking a value that `token` names exactly
    /// (`--long` or `-s`).
    fn value_option(&self, token: &str) -> Option<&PluginCommandArgument> {
        self.arguments.iter().find(|argument| {
            if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
                return false;
            }
            if let Some(long) = token.strip_prefix("--") {
                return argument.long.as_deref() == Some(long);
            }
            let mut chars = token.strip_prefix('-').unwrap_or_default().chars();
            matches!((chars.next(), chars.next()), (Some(short), None) if argument.short == Some(short))
        })
    }
}

impl PluginCommandArgument {
    #[must_use]
    pub fn option(name: impl Into<String>, kind: PluginCommandArgumentKind) -> Self {
        let name = name.into();
        Self {
            value_name: Some(name.replace('-', "_").to_uppercase()),
            long: Some(name.clone()),
            name,
            kind,
            choice_values: Vec::new(),
            choice_aliases: BTreeMap::new(),
            default_value: None,
            conflicts_with: Vec::new(),
            position: None,
            short: None,
            required: false,
            multiple: false,
            trailing_var_arg: false,
            allow_hyphen_values: false,
            summary: None,
        }
    }

    #[must_use]
    pub fn flag(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            kind: PluginCommandArgumentKind::Boolean,
            choice_values: Vec::new(),
            choice_aliases: BTreeMap::new(),
            default_value: None,
            conflicts_with: Vec::new(),
            position: None,
            long: Some(name),
            short: None,
            required: false,
            multiple: false,
            trailing_var_arg: false,
            allow_hyphen_values: false,
            summary: None,
            value_name: None,
        }
    }

    #[must_use]
    pub fn positional(name: impl Into<String>, kind: PluginCommandArgumentKind) -> Self {
        let name = name.into();
        Self {
            value_name: Some(name.to_uppercase()),
            name,
            kind,
            choice_values: Vec::new(),
            choice_aliases: BTreeMap::new(),
            default_value: None,
            conflicts_with: Vec::new(),
            position: Some(0),
            long: None,
            short: None,
            required: false,
            multiple: false,
            trailing_var_arg: false,
            allow_hyphen_values: false,
            summary: None,
        }
    }

    #[must_use]
    pub const fn short(mut self, short: char) -> Self {
        self.short = Some(short);
        self
    }

    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    #[must_use]
    pub const fn multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }

    #[must_use]
    pub const fn trailing_var_arg(mut self, trailing_var_arg: bool) -> Self {
        self.trailing_var_arg = trailing_var_arg;
        self
    }

    #[must_use]
    pub const fn allow_hyphen_values(mut self, allow_hyphen_values: bool) -> Self {
        self.allow_hyphen_values = allow_hyphen_values;
        self
    }

    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    #[must_use]
    pub fn value_name(mut self, value_name: impl Into<String>) -> Self {
        self.value_name = Some(value_name.into());
        self
    }

    #[must_use]
    pub fn choice_values(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.choice_values = values.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub const fn position(mut self, position: usize) -> Self {
        self.position = Some(position);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandExecutionKind, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn new_tab() -> PluginCommand {
        PluginCommand::new("new-tab", "Create a tab")
            .argument(
                PluginCommandArgument::option("name", PluginCommandArgumentKind::String).short('n'),
            )
            .argument(PluginCommandArgument::option(
                "count",
                PluginCommandArgumentKind::Integer,
            ))
            .argument(PluginCommandArgument::flag("force"))
            .argument(PluginCommandArgument::positional(
                "target",
                PluginCommandArgumentKind::String,
            ))
    }

    #[test]
    fn inline_option_value_is_split_into_separated_form() {
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--name=deps"])),
            Some(args(&["--name", "deps"]))
        );
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["-n=deps", "--count=2", "x"])),
            Some(args(&["-n", "deps", "--count", "2", "x"]))
        );
    }

    #[test]
    fn separated_option_value_is_left_unchanged() {
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--name", "deps", "--force"])),
            None
        );
    }

    #[test]
    fn inline_option_value_keeps_embedded_equals_signs() {
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--name=a=b=c"])),
            Some(args(&["--name", "a=b=c"]))
        );
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--name="])),
            Some(args(&["--name", ""]))
        );
    }

    #[test]
    fn unknown_options_and_boolean_flags_are_forwarded_verbatim() {
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&[
                "--unknown=value",
                "--force=true",
                "--names=x",
            ])),
            None
        );
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--unknown=value", "--name=deps"])),
            Some(args(&["--unknown=value", "--name", "deps"]))
        );
    }

    #[test]
    fn option_values_and_separator_tail_are_never_split() {
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--name", "--count=2"])),
            None
        );
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--", "--name=deps"])),
            None
        );
        assert_eq!(
            new_tab().normalize_inline_option_values(&args(&["--count=1", "--", "--name=deps"])),
            Some(args(&["--count", "1", "--", "--name=deps"]))
        );
    }

    #[test]
    fn trailing_variadic_argv_after_first_positional_is_caller_owned() {
        let command = PluginCommand::new("run", "Run")
            .argument(PluginCommandArgument::option(
                "name",
                PluginCommandArgumentKind::String,
            ))
            .argument(
                PluginCommandArgument::positional("argv", PluginCommandArgumentKind::String)
                    .multiple(true)
                    .trailing_var_arg(true),
            );
        assert_eq!(
            command.normalize_inline_option_values(&args(&["--name=a", "cmd", "--name=b"])),
            Some(args(&["--name", "a", "cmd", "--name=b"]))
        );
    }

    #[test]
    fn caller_process_execution_deserializes_from_manifest_value() {
        let command: PluginCommand = toml::from_str(
            r#"
name = "pick-theme"
summary = "Open picker"
execution = "caller_process"
"#,
        )
        .expect("caller_process command execution should parse");

        assert_eq!(command.execution, CommandExecutionKind::CallerProcess);
    }
}
