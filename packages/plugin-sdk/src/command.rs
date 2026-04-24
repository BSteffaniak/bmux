use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionKind {
    ProviderExec,
    BackgroundTask,
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
