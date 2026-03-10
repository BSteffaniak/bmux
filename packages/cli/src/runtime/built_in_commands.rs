use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInHandlerId {
    NewSession,
    ListSessions,
    ListClients,
    KillSession,
    KillAllSessions,
    Attach,
    Detach,
    Follow,
    Unfollow,
    Session,
    SessionNew,
    SessionList,
    SessionClients,
    SessionKill,
    SessionKillAll,
    SessionAttach,
    SessionDetach,
    SessionFollow,
    SessionUnfollow,
    Server,
    ServerStart,
    ServerStatus,
    ServerWhoamiPrincipal,
    ServerSave,
    ServerRestore,
    ServerStop,
    Keymap,
    KeymapDoctor,
    Terminal,
    TerminalDoctor,
    TerminalInstallTerminfo,
    Plugin,
    PluginList,
    PluginRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PluginDomain {
    Permissions,
    Sessions,
    Windows,
    Panes,
    Follow,
    Persistence,
    Diagnostics,
}

#[must_use]
#[allow(dead_code)]
pub fn all_plugin_domains() -> &'static [PluginDomain] {
    &[
        PluginDomain::Permissions,
        PluginDomain::Sessions,
        PluginDomain::Windows,
        PluginDomain::Panes,
        PluginDomain::Follow,
        PluginDomain::Persistence,
        PluginDomain::Diagnostics,
    ]
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

    pub fn all_paths(&self) -> impl Iterator<Item = &Vec<String>> {
        std::iter::once(&self.canonical_path).chain(self.aliases.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct PlannedPluginCommand {
    pub handler: BuiltInHandlerId,
    pub domain: PluginDomain,
}

pub fn built_in_execution_commands() -> Vec<BuiltInExecutionCommand> {
    vec![
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::NewSession,
            &["new-session"],
            "Create a new session",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ListSessions,
            &["list-sessions"],
            "List active sessions",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ListClients,
            &["list-clients"],
            "List connected clients",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::KillSession,
            &["kill-session"],
            "Kill a session by name or UUID",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::KillAllSessions,
            &["kill-all-sessions"],
            "Kill all sessions",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Attach,
            &["attach"],
            "Attach to a session by name or UUID",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Detach,
            &["detach"],
            "Detach from the current session",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Follow,
            &["follow"],
            "Follow another client's active target",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Unfollow,
            &["unfollow"],
            "Stop following a client",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Session,
            &["session"],
            "Session management commands",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionNew,
            &["session", "new"],
            "Create a new session",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionList,
            &["session", "list"],
            "List active sessions",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionClients,
            &["session", "clients"],
            "List connected clients",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionKill,
            &["session", "kill"],
            "Kill a session by name or UUID",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionKillAll,
            &["session", "kill-all"],
            "Kill all sessions",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionAttach,
            &["session", "attach"],
            "Attach to a session by name or UUID",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionDetach,
            &["session", "detach"],
            "Detach from the current session",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionFollow,
            &["session", "follow"],
            "Follow another client's active target",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SessionUnfollow,
            &["session", "unfollow"],
            "Stop following a client",
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
            BuiltInHandlerId::ServerSave,
            &["server", "save"],
            "Save server state",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRestore,
            &["server", "restore"],
            "Restore server state",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerStop,
            &["server", "stop"],
            "Stop the server",
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
            BuiltInHandlerId::Plugin,
            &["plugin"],
            "Plugin discovery and execution tools",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PluginList,
            &["plugin", "list"],
            "List discovered plugins",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PluginRun,
            &["plugin", "run"],
            "Run a plugin command explicitly",
        ),
    ]
}

#[allow(dead_code)]
pub fn planned_plugin_commands() -> &'static [PlannedPluginCommand] {
    &[
        PlannedPluginCommand {
            handler: BuiltInHandlerId::NewSession,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::ListSessions,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::ListClients,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::KillSession,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::KillAllSessions,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::Follow,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::Unfollow,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionNew,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionList,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionClients,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionKill,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionKillAll,
            domain: PluginDomain::Sessions,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionFollow,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::SessionUnfollow,
            domain: PluginDomain::Follow,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::KeymapDoctor,
            domain: PluginDomain::Diagnostics,
        },
        PlannedPluginCommand {
            handler: BuiltInHandlerId::TerminalDoctor,
            domain: PluginDomain::Diagnostics,
        },
    ]
}

#[allow(dead_code)]
pub fn planned_plugin_domain(handler: BuiltInHandlerId) -> Option<PluginDomain> {
    planned_plugin_commands()
        .iter()
        .find(|command| command.handler == handler)
        .map(|command| command.domain)
}

pub fn reserved_built_in_paths() -> BTreeSet<Vec<String>> {
    built_in_execution_commands()
        .into_iter()
        .filter(|command| !matches!(command.handler, BuiltInHandlerId::Session))
        .flat_map(|command| command.all_paths().cloned().collect::<Vec<_>>())
        .collect()
}

pub fn built_in_command_by_handler(handler: BuiltInHandlerId) -> BuiltInExecutionCommand {
    built_in_execution_commands()
        .into_iter()
        .find(|command| command.handler == handler)
        .expect("built-in command handler should be registered")
}

#[cfg(test)]
mod tests {
    use super::{
        BuiltInHandlerId, PluginDomain, all_plugin_domains, built_in_command_by_handler,
        built_in_execution_commands, planned_plugin_domain, reserved_built_in_paths,
    };

    #[test]
    fn reserved_paths_include_current_static_commands() {
        let paths = reserved_built_in_paths();
        assert!(paths.contains(&vec!["new-session".to_string()]));
        assert!(paths.contains(&vec!["session".to_string(), "new".to_string()]));
        assert!(paths.contains(&vec!["plugin".to_string(), "run".to_string()]));
    }

    #[test]
    fn reserved_paths_leave_session_root_extensible() {
        let paths = reserved_built_in_paths();
        assert!(!paths.contains(&vec!["session".to_string()]));
    }

    #[test]
    fn built_in_table_contains_expected_handler() {
        let commands = built_in_execution_commands();
        assert!(commands.iter().any(|command| {
            command.handler == BuiltInHandlerId::PluginRun
                && command.canonical_path == vec!["plugin".to_string(), "run".to_string()]
        }));
    }

    #[test]
    fn command_lookup_by_handler_returns_descriptor() {
        let command = built_in_command_by_handler(BuiltInHandlerId::PluginRun);
        assert_eq!(
            command.canonical_path,
            vec!["plugin".to_string(), "run".to_string()]
        );
    }

    #[test]
    fn future_plugin_domains_are_classified_in_code() {
        assert!(all_plugin_domains().contains(&PluginDomain::Panes));
        assert!(all_plugin_domains().contains(&PluginDomain::Persistence));
        assert_eq!(
            planned_plugin_domain(BuiltInHandlerId::SessionList),
            Some(PluginDomain::Sessions)
        );
        assert_eq!(
            planned_plugin_domain(BuiltInHandlerId::Follow),
            Some(PluginDomain::Follow)
        );
    }

    #[test]
    fn migrated_plugin_owned_commands_are_not_in_core_execution_table() {
        let paths = built_in_execution_commands()
            .into_iter()
            .map(|command| command.canonical_path)
            .collect::<Vec<_>>();
        for removed in [
            vec!["permissions".to_string()],
            vec!["grant".to_string()],
            vec!["revoke".to_string()],
            vec!["session".to_string(), "permissions".to_string()],
            vec!["session".to_string(), "grant".to_string()],
            vec!["session".to_string(), "revoke".to_string()],
            vec!["new-window".to_string()],
            vec!["list-windows".to_string()],
            vec!["kill-window".to_string()],
            vec!["kill-all-windows".to_string()],
            vec!["switch-window".to_string()],
            vec!["window".to_string()],
            vec!["window".to_string(), "new".to_string()],
            vec!["window".to_string(), "list".to_string()],
            vec!["window".to_string(), "kill".to_string()],
            vec!["window".to_string(), "kill-all".to_string()],
            vec!["window".to_string(), "switch".to_string()],
        ] {
            assert!(
                !paths.contains(&removed),
                "migrated plugin-owned command path {:?} should not remain in core command table",
                removed
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
        assert_eq!(planned_plugin_domain(BuiltInHandlerId::ServerStatus), None);
    }
}
