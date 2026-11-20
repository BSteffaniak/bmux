#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInHandlerId {
    Attach,
    Server,
    ServerStart,
    ServerStatus,
    ServerWhoamiPrincipal,
    ServerStop,
    ServerAutostartInstall,
    ServerAutostartUninstall,
    ServerAutostartStatus,
    ServerAutostartPrint,
    ServerGateway,
    ServerBridge,
    Logs,
    LogsPath,
    LogsLevel,
    Config,
    ConfigPath,
    ConfigShow,
    ConfigGet,
    ConfigSet,
    ConfigProfilesList,
    ConfigProfilesShow,
    ConfigProfilesResolve,
    ConfigProfilesExplain,
    ConfigScopeExplain,
    ConfigProfilesSwitch,
    ConfigProfilesDiff,
    ConfigProfilesLint,
    ConfigProfilesEvaluate,
    Doctor,
    Keymap,
    KeymapDoctor,
    KeymapExplain,
    Terminal,
    TerminalDoctor,
    TerminalInstallTerminfo,
    Sandbox,
    SandboxDev,
    SandboxAttach,
    SandboxStop,
    SandboxRun,
    SandboxList,
    SandboxStatus,
    SandboxInspect,
    SandboxTail,
    SandboxOpen,
    SandboxRerun,
    SandboxTriage,
    SandboxDoctor,
    SandboxBundle,
    SandboxVerifyBundle,
    SandboxCleanup,
    SandboxClean,
    SandboxRebuildIndex,
    SlotList,
    SlotShow,
    SlotPaths,
    SlotDoctor,
    SlotInstall,
    SlotUninstall,
    SlotShell,
    SlotExec,
    SlotPrint,
    DeviceSealBroker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInExecutionCommand {
    pub handler: BuiltInHandlerId,
    pub canonical_path: Vec<String>,
    pub aliases: Vec<Vec<String>>,
    pub summary: &'static str,
}

impl BuiltInExecutionCommand {
    fn new(handler: BuiltInHandlerId, path: &[&str], summary: &'static str) -> Self {
        Self {
            handler,
            canonical_path: path.iter().map(|segment| (*segment).to_string()).collect(),
            aliases: Vec::new(),
            summary,
        }
    }

    /// Attach an alias path that dispatches to the same handler.
    fn with_alias(mut self, path: &[&str]) -> Self {
        self.aliases
            .push(path.iter().map(|s| (*s).to_string()).collect());
        self
    }
}

#[allow(clippy::too_many_lines)]
pub fn built_in_execution_commands() -> Vec<BuiltInExecutionCommand> {
    vec![
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Attach,
            &["attach"],
            "Attach to a session by name or UUID",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Server,
            &["server"],
            "Server lifecycle and status tools",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerStart,
            &["server", "start"],
            "Start the server",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerStatus,
            &["server", "status"],
            "Show server status",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerWhoamiPrincipal,
            &["server", "whoami-principal"],
            "Show active principal id",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerStop,
            &["server", "stop"],
            "Stop the server",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerAutostartInstall,
            &["server", "autostart", "install"],
            "Install native per-user server login autostart",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerAutostartUninstall,
            &["server", "autostart", "uninstall"],
            "Remove bmux-managed server login autostart",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerAutostartStatus,
            &["server", "autostart", "status"],
            "Show native server autostart status",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerAutostartPrint,
            &["server", "autostart", "print"],
            "Print the native server autostart declaration",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerGateway,
            &["server", "gateway"],
            "Run TLS gateway for remote clients",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerBridge,
            &["server", "bridge"],
            "Internal stdio bridge used by SSH transport",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Logs,
            &["logs"],
            "Logging diagnostics and utilities",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsPath,
            &["logs", "path"],
            "Show effective log file path",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsLevel,
            &["logs", "level"],
            "Show effective runtime log level",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Config,
            &["config"],
            "Configuration management and inspection",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigPath,
            &["config", "path"],
            "Print the config file path",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigShow,
            &["config", "show"],
            "Print the effective configuration",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigGet,
            &["config", "get"],
            "Get a configuration value by key",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigSet,
            &["config", "set"],
            "Set a configuration value",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesList,
            &["config", "profiles", "list"],
            "List known composition profiles",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesShow,
            &["config", "profiles", "show"],
            "Show one resolved composition profile",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesResolve,
            &["config", "profiles", "resolve"],
            "Show active profile resolution metadata",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesExplain,
            &["config", "profiles", "explain"],
            "Explain layer-by-layer composition changes",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesSwitch,
            &["config", "profiles", "switch"],
            "Set active composition profile",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesDiff,
            &["config", "profiles", "diff"],
            "Compare two resolved profile configurations",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesLint,
            &["config", "profiles", "lint"],
            "Validate profile composition graph",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigProfilesEvaluate,
            &["config", "profiles", "evaluate"],
            "Evaluate auto-select profile rules",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ConfigScopeExplain,
            &["config", "scope", "explain"],
            "Explain scoped config source resolution",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Doctor,
            &["doctor"],
            "Run system-wide health checks",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Keymap,
            &["keymap"],
            "Keymap tools and diagnostics",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::KeymapDoctor,
            &["keymap", "doctor"],
            "Inspect keymap configuration",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::KeymapExplain,
            &["keymap", "explain"],
            "Explain effective keybinding resolution",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Terminal,
            &["terminal"],
            "Terminal capability tools and diagnostics",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::TerminalDoctor,
            &["terminal", "doctor"],
            "Run terminal diagnostics",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::TerminalInstallTerminfo,
            &["terminal", "install-terminfo"],
            "Install terminfo entry",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Sandbox,
            &["sandbox"],
            "Run bmux commands in isolated sandboxes",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxRun,
            &["sandbox", "run"],
            "Run a bmux command in an ephemeral sandbox",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxDev,
            &["sandbox", "dev"],
            "Run a bmux command in a dev sandbox",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxAttach,
            &["sandbox", "attach"],
            "Attach to an isolated development server",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxStop,
            &["sandbox", "stop"],
            "Stop an isolated development server",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxList,
            &["sandbox", "list"],
            "List known bmux sandbox runs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxStatus,
            &["sandbox", "status"],
            "Show sandbox runtime and index summary",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxInspect,
            &["sandbox", "inspect"],
            "Inspect bmux sandbox metadata and logs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxTail,
            &["sandbox", "tail"],
            "Tail logs from a sandbox quickly",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxOpen,
            &["sandbox", "open"],
            "Show sandbox paths and repro command",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxRerun,
            &["sandbox", "rerun"],
            "Rerun command from sandbox manifest",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxTriage,
            &["sandbox", "triage"],
            "One-shot summary for sandbox failure triage",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxDoctor,
            &["sandbox", "doctor"],
            "Run health checks for sandbox workflows",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxBundle,
            &["sandbox", "bundle"],
            "Bundle sandbox diagnostics and logs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxVerifyBundle,
            &["sandbox", "verify-bundle"],
            "Verify bundle artifacts against metadata",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxCleanup,
            &["sandbox", "cleanup"],
            "Clean sandbox run temp directories",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxClean,
            &["sandbox", "clean"],
            "Clean failed sandboxes with opinionated defaults",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxRebuildIndex,
            &["sandbox", "rebuild-index"],
            "Rebuild sandbox index from manifests",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotList,
            &["slot", "list"],
            "List declared bmux slots",
        )
        .with_alias(&["env", "list"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotShow,
            &["slot", "show"],
            "Show a single slot's resolved detail",
        )
        .with_alias(&["env", "show"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotPaths,
            &["slot", "paths"],
            "Print a slot's resolved config/runtime/data/state/log paths",
        )
        .with_alias(&["env", "paths"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotDoctor,
            &["slot", "doctor"],
            "Validate the slot manifest",
        )
        .with_alias(&["env", "doctor"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotInstall,
            &["slot", "install"],
            "Register a new bmux slot (writes manifest; refuses read-only manifests)",
        )
        .with_alias(&["env", "install"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotUninstall,
            &["slot", "uninstall"],
            "Remove a bmux slot from the manifest and delete its binary link",
        )
        .with_alias(&["env", "uninstall"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotShell,
            &["slot", "shell"],
            "Print shell code to prepend $BMUX_SLOTS_BIN_DIR to PATH",
        )
        .with_alias(&["env", "shell"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotExec,
            &["slot", "exec"],
            "Run a command with a slot's env applied",
        )
        .with_alias(&["env", "exec"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SlotPrint,
            &["slot", "print"],
            "Print the resolved env-var set as structured data",
        )
        .with_alias(&["env", "print"]),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::DeviceSealBroker,
            &["device-seal-broker"],
            "Internal sshenv device-seal broker",
        ),
    ]
}

pub fn built_in_command_by_handler(handler: BuiltInHandlerId) -> BuiltInExecutionCommand {
    built_in_execution_commands()
        .into_iter()
        .find(|command| command.handler == handler)
        .expect("built-in command handler should be registered")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        BuiltInExecutionCommand, BuiltInHandlerId, built_in_command_by_handler,
        built_in_execution_commands,
    };

    impl BuiltInExecutionCommand {
        fn all_paths(&self) -> impl Iterator<Item = &Vec<String>> {
            std::iter::once(&self.canonical_path).chain(self.aliases.iter())
        }
    }

    fn reserved_built_in_paths() -> BTreeSet<Vec<String>> {
        built_in_execution_commands()
            .into_iter()
            .flat_map(|command| command.all_paths().cloned().collect::<Vec<_>>())
            .collect()
    }

    #[test]
    fn reserved_paths_include_current_static_commands() {
        let paths = reserved_built_in_paths();
        assert!(!paths.contains(&vec!["new-session".to_string()]));
        assert!(!paths.contains(&vec!["session".to_string(), "new".to_string()]));
        assert!(paths.contains(&vec!["terminal".to_string(), "doctor".to_string()]));
    }

    #[test]
    fn reserved_paths_leave_session_root_extensible() {
        let paths = reserved_built_in_paths();
        assert!(!paths.contains(&vec!["session".to_string()]));
        assert!(!paths.contains(&vec!["plugin".to_string()]));
    }

    #[test]
    fn built_in_table_contains_expected_handler() {
        let commands = built_in_execution_commands();
        assert!(commands.iter().any(|command| {
            command.handler == BuiltInHandlerId::TerminalDoctor
                && command.canonical_path == vec!["terminal".to_string(), "doctor".to_string()]
        }));
    }

    #[test]
    fn command_lookup_by_handler_returns_descriptor() {
        let command = built_in_command_by_handler(BuiltInHandlerId::TerminalDoctor);
        assert_eq!(
            command.canonical_path,
            vec!["terminal".to_string(), "doctor".to_string()]
        );
    }

    #[test]
    fn command_lookup_includes_new_config_profiles_explain_handler() {
        let command = built_in_command_by_handler(BuiltInHandlerId::ConfigProfilesExplain);
        assert_eq!(
            command.canonical_path,
            vec![
                "config".to_string(),
                "profiles".to_string(),
                "explain".to_string()
            ]
        );
    }

    #[test]
    fn command_lookup_includes_new_keymap_explain_handler() {
        let command = built_in_command_by_handler(BuiltInHandlerId::KeymapExplain);
        assert_eq!(
            command.canonical_path,
            vec!["keymap".to_string(), "explain".to_string()]
        );
    }

    #[test]
    fn autostart_handlers_have_distinct_registered_paths() {
        for (handler, path) in [
            (
                BuiltInHandlerId::ServerAutostartInstall,
                vec!["server", "autostart", "install"],
            ),
            (
                BuiltInHandlerId::ServerAutostartUninstall,
                vec!["server", "autostart", "uninstall"],
            ),
            (
                BuiltInHandlerId::ServerAutostartStatus,
                vec!["server", "autostart", "status"],
            ),
            (
                BuiltInHandlerId::ServerAutostartPrint,
                vec!["server", "autostart", "print"],
            ),
        ] {
            let command = built_in_command_by_handler(handler);
            assert_eq!(command.canonical_path, path);
        }
    }

    #[test]
    fn migrated_plugin_owned_commands_are_not_in_core_execution_table() {
        let paths = built_in_execution_commands()
            .into_iter()
            .map(|command| command.canonical_path)
            .collect::<Vec<_>>();
        for removed in [
            vec!["roles".to_string()],
            vec!["assign".to_string()],
            vec!["unassign".to_string()],
            vec!["session".to_string(), "roles".to_string()],
            vec!["session".to_string(), "assign".to_string()],
            vec!["session".to_string(), "unassign".to_string()],
            vec!["tool-open".to_string()],
            vec!["tool-list".to_string()],
            vec!["tool-close".to_string()],
            vec!["tool-close-all".to_string()],
            vec!["tool-focus".to_string()],
            vec!["tool".to_string()],
            vec!["tool".to_string(), "open".to_string()],
            vec!["tool".to_string(), "list".to_string()],
            vec!["tool".to_string(), "close".to_string()],
            vec!["tool".to_string(), "close-all".to_string()],
            vec!["tool".to_string(), "focus".to_string()],
            vec!["recording".to_string()],
            vec!["recording".to_string(), "start".to_string()],
            vec!["recording".to_string(), "stop".to_string()],
            vec!["recording".to_string(), "status".to_string()],
            vec!["recording".to_string(), "path".to_string()],
            vec!["recording".to_string(), "list".to_string()],
            vec!["recording".to_string(), "delete".to_string()],
            vec!["recording".to_string(), "delete-all".to_string()],
            vec!["recording".to_string(), "cut".to_string()],
            vec!["recording".to_string(), "inspect".to_string()],
            vec!["recording".to_string(), "analyze".to_string()],
            vec!["recording".to_string(), "replay".to_string()],
            vec!["recording".to_string(), "verify-smoke".to_string()],
            vec!["recording".to_string(), "export".to_string()],
            vec!["recording".to_string(), "prune".to_string()],
            vec!["server".to_string(), "recording".to_string()],
            vec!["playbook".to_string(), "from-recording".to_string()],
        ] {
            assert!(
                !paths.contains(&removed),
                "migrated plugin-owned command path {removed:?} should not remain in core command table"
            );
        }
    }

    #[test]
    fn server_status_remains_in_core_execution_table() {
        let command = built_in_command_by_handler(BuiltInHandlerId::ServerStatus);
        assert_eq!(
            command.canonical_path,
            vec!["server".to_string(), "status".to_string()]
        );
    }
}
