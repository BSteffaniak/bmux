#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInHandlerId {
    Setup,
    Host,
    Join,
    Hosts,
    Auth,
    AuthLogin,
    AuthStatus,
    AuthLogout,
    Access,
    Share,
    Unshare,
    Connect,
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
    Remote,
    RemoteList,
    RemoteTest,
    RemoteDoctor,
    RemoteInit,
    RemoteInstallServer,
    RemoteUpgrade,
    RemoteComplete,
    RemoteCompleteTargets,
    RemoteCompleteSessions,
    Server,
    ServerStart,
    ServerStatus,
    ServerWhoamiPrincipal,
    ServerSave,
    ServerRestore,
    ServerStop,
    ServerRecording,
    ServerRecordingStart,
    ServerRecordingStop,
    ServerRecordingStatus,
    ServerRecordingPath,
    ServerRecordingClear,
    ServerGateway,
    ServerBridge,
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
    Config,
    ConfigPath,
    ConfigShow,
    ConfigGet,
    ConfigSet,
    ConfigProfilesList,
    ConfigProfilesShow,
    ConfigProfilesResolve,
    ConfigProfilesSwitch,
    ConfigProfilesDiff,
    ConfigProfilesLint,
    ConfigProfilesEvaluate,
    Perf,
    PerfStatus,
    PerfOn,
    PerfOff,
    Doctor,
    Keymap,
    KeymapDoctor,
    Terminal,
    TerminalDoctor,
    TerminalInstallTerminfo,
    Recording,
    RecordingStart,
    RecordingStop,
    RecordingStatus,
    RecordingPath,
    RecordingList,
    RecordingDelete,
    RecordingDeleteAll,
    RecordingCut,
    RecordingInspect,
    RecordingAnalyze,
    RecordingReplay,
    RecordingVerifySmoke,
    RecordingExport,
    RecordingPrune,
    Playbook,
    PlaybookRun,
    PlaybookValidate,
    PlaybookInteractive,
    PlaybookFromRecording,
    PlaybookDryRun,
    PlaybookDiff,
    PlaybookCleanup,
    Sandbox,
    SandboxRun,
    SandboxList,
    SandboxInspect,
    SandboxDoctor,
    SandboxCleanup,
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
}

#[allow(clippy::too_many_lines)]
pub fn built_in_execution_commands() -> Vec<BuiltInExecutionCommand> {
    vec![
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Setup,
            &["setup"],
            "Run first-time hosted setup",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Host,
            &["host"],
            "Start hosted mode using iroh",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Join,
            &["join"],
            "Join a hosted link quickly",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Hosts,
            &["hosts"],
            "List known hosts and targets",
        ),
        BuiltInExecutionCommand::new(BuiltInHandlerId::Auth, &["auth"], "Authentication helpers"),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::AuthLogin,
            &["auth", "login"],
            "Login/register local device",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::AuthStatus,
            &["auth", "status"],
            "Show current login status",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::AuthLogout,
            &["auth", "logout"],
            "Remove local login credentials",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Access,
            &["access"],
            "Manage iroh SSH access keys",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Share,
            &["share"],
            "Create a shareable bmux:// link",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Unshare,
            &["unshare"],
            "Remove a share link",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Connect,
            &["connect"],
            "Connect to a target and attach to a session",
        ),
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
            BuiltInHandlerId::Remote,
            &["remote"],
            "Remote target utilities",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteList,
            &["remote", "list"],
            "List configured connection targets",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteTest,
            &["remote", "test"],
            "Verify connectivity to a target",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteDoctor,
            &["remote", "doctor"],
            "Diagnose remote connectivity and bmux readiness",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteInit,
            &["remote", "init"],
            "Create and validate remote target profile",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteInstallServer,
            &["remote", "install-server"],
            "Install or validate remote bmux server runtime",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteUpgrade,
            &["remote", "upgrade"],
            "Upgrade remote bmux runtime on targets",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteComplete,
            &["remote", "complete"],
            "Shell completion helpers",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteCompleteTargets,
            &["remote", "complete", "targets"],
            "Print target completions",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RemoteCompleteSessions,
            &["remote", "complete", "sessions"],
            "Print session completions",
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
            BuiltInHandlerId::ServerRecording,
            &["server", "recording"],
            "Control hidden rolling recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRecordingStart,
            &["server", "recording", "start"],
            "Start hidden rolling recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRecordingStop,
            &["server", "recording", "stop"],
            "Stop hidden rolling recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRecordingStatus,
            &["server", "recording", "status"],
            "Show hidden rolling recording status",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRecordingPath,
            &["server", "recording", "path"],
            "Show hidden rolling recording path",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::ServerRecordingClear,
            &["server", "recording", "clear"],
            "Clear hidden rolling recording data",
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
            BuiltInHandlerId::Perf,
            &["perf"],
            "Runtime performance telemetry controls",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PerfStatus,
            &["perf", "status"],
            "Show runtime performance telemetry settings",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PerfOn,
            &["perf", "on"],
            "Enable runtime performance telemetry",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PerfOff,
            &["perf", "off"],
            "Disable runtime performance telemetry",
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
            BuiltInHandlerId::RecordingPath,
            &["recording", "path"],
            "Print recordings storage path",
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
            BuiltInHandlerId::RecordingCut,
            &["recording", "cut"],
            "Snapshot active rolling recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingInspect,
            &["recording", "inspect"],
            "Inspect recording timeline",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingAnalyze,
            &["recording", "analyze"],
            "Analyze recording diagnostics and bottlenecks",
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
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::RecordingPrune,
            &["recording", "prune"],
            "Delete completed recordings older than the retention period",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::Playbook,
            &["playbook"],
            "Headless playbook execution and testing",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookRun,
            &["playbook", "run"],
            "Run a playbook",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookValidate,
            &["playbook", "validate"],
            "Validate a playbook without executing",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookInteractive,
            &["playbook", "interactive"],
            "Start an interactive playbook session",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookFromRecording,
            &["playbook", "from-recording"],
            "Generate a playbook from a recording",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookDryRun,
            &["playbook", "dry-run"],
            "Dry-run a playbook: parse, validate, and print the execution plan",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookDiff,
            &["playbook", "diff"],
            "Compare results from two playbook runs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::PlaybookCleanup,
            &["playbook", "cleanup"],
            "Remove orphaned sandbox temp directories",
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
            BuiltInHandlerId::SandboxList,
            &["sandbox", "list"],
            "List known bmux sandbox runs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxInspect,
            &["sandbox", "inspect"],
            "Inspect bmux sandbox metadata and logs",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxDoctor,
            &["sandbox", "doctor"],
            "Run health checks for sandbox workflows",
        ),
        BuiltInExecutionCommand::new(
            BuiltInHandlerId::SandboxCleanup,
            &["sandbox", "cleanup"],
            "Remove orphaned sandbox run temp directories",
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
            .filter(|command| !matches!(command.handler, BuiltInHandlerId::Session))
            .flat_map(|command| command.all_paths().cloned().collect::<Vec<_>>())
            .collect()
    }

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
