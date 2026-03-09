use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionKind {
    HostCallback,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandArgument {
    pub name: String,
    pub kind: PluginCommandArgumentKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginCommandArgumentKind {
    String,
    Integer,
    Boolean,
    Path,
    Choice { values: Vec<String> },
}

const fn default_execution_kind() -> CommandExecutionKind {
    CommandExecutionKind::HostCallback
}

const fn default_expose_in_cli() -> bool {
    true
}

impl PluginCommand {
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
