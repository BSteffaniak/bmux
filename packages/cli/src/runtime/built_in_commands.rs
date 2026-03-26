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
    Logs,
    LogsPath,
    LogsLevel,
    LogsTail,
    LogsWatch,
    LogsProfiles,
    LogsProfilesList,
    LogsProfilesShow,
    LogsProfilesDelete,
    LogsProfilesRename,
    Keymap,
    KeymapDoctor,
    Terminal,
    TerminalDoctor,
    TerminalInstallTerminfo,
    Recording,
    RecordingStart,
    RecordingStop,
    RecordingStatus,
    RecordingList,
    RecordingDelete,
    RecordingDeleteAll,
    RecordingInspect,
    RecordingReplay,
    RecordingVerifySmoke,
    RecordingExport,
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
            BuiltInHandlerId::LogsTail,
            &["logs", "tail"],
            "Tail recent log lines",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsWatch,
            &["logs", "watch"],
            "Interactive log viewer with live filters",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsProfiles,
            &["logs", "profiles"],
            "Manage saved log watch profiles",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsProfilesList,
            &["logs", "profiles", "list"],
            "List saved watch profiles",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsProfilesShow,
            &["logs", "profiles", "show"],
            "Show details for one watch profile",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsProfilesDelete,
            &["logs", "profiles", "delete"],
            "Delete a saved watch profile",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::LogsProfilesRename,
            &["logs", "profiles", "rename"],
            "Rename a saved watch profile",
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
            BuiltInHandlerId::Recording,
            &["recording"],
            "Recording and replay controls",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingStart,
            &["recording", "start"],
            "Start explicit recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingStop,
            &["recording", "stop"],
            "Stop active recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingStatus,
            &["recording", "status"],
            "Show recording status",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingList,
            &["recording", "list"],
            "List recordings",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingDelete,
            &["recording", "delete"],
            "Delete one recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingDeleteAll,
            &["recording", "delete-all"],
            "Delete all recordings",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingInspect,
            &["recording", "inspect"],
            "Inspect recording timeline",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingReplay,
            &["recording", "replay"],
            "Replay recording timeline",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingVerifySmoke,
            &["recording", "verify-smoke"],
            "Emit recording verify smoke report",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingExport,
            &["recording", "export"],
            "Export recording media",
        ),
    ]
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
        BuiltInHandlerId, built_in_command_by_handler, built_in_execution_commands,
        reserved_built_in_paths,
    };

    #[test]
    fn reserved_paths_include_current_static_commands() {
        let paths = reserved_built_in_paths();
        assert!(paths.contains(&vec!["new-session".to_string()]));
        assert!(paths.contains(&vec!["session".to_string(), "new".to_string()]));
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
    }
}
