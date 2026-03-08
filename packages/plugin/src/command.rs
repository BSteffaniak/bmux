use serde::{Deserialize, Serialize};

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
    pub summary: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PluginCommandArgument>,
    #[serde(default = "default_execution_kind")]
    pub execution: CommandExecutionKind,
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
