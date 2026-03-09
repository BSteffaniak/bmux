use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInHandlerId {
    NewSession,
    ListSessions,
    ListClients,
    Permissions,
    Grant,
    Revoke,
    KillSession,
    KillAllSessions,
    Attach,
    Detach,
    NewWindow,
    ListWindows,
    KillWindow,
    KillAllWindows,
    SwitchWindow,
    Follow,
    Unfollow,
    Session,
    SessionNew,
    SessionList,
    SessionClients,
    SessionPermissions,
    SessionGrant,
    SessionRevoke,
    SessionKill,
    SessionKillAll,
    SessionAttach,
    SessionDetach,
    SessionFollow,
    SessionUnfollow,
    Window,
    WindowNew,
    WindowList,
    WindowKill,
    WindowKillAll,
    WindowSwitch,
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
pub enum CoreCommandClass {
    CoreNative,
    PluginBackedShipped,
    PluginBackedLater,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInCliCommand {
    pub handler: BuiltInHandlerId,
    pub canonical_path: Vec<String>,
    pub aliases: Vec<Vec<String>>,
    pub summary: &'static str,
    pub class: CoreCommandClass,
}

impl BuiltInCliCommand {
    fn new(
        handler: BuiltInHandlerId,
        path: &[&str],
        summary: &'static str,
        class: CoreCommandClass,
    ) -> Self {
        Self {
            handler,
            canonical_path: path.iter().map(|segment| (*segment).to_string()).collect(),
            aliases: Vec::new(),
            summary,
            class,
        }
    }

    pub fn all_paths(&self) -> impl Iterator<Item = &Vec<String>> {
        std::iter::once(&self.canonical_path).chain(self.aliases.iter())
    }
}

pub fn built_in_cli_commands() -> Vec<BuiltInCliCommand> {
    vec![
        BuiltInCliCommand::new(
            BuiltInHandlerId::NewSession,
            &["new-session"],
            "Create a new session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ListSessions,
            &["list-sessions"],
            "List active sessions",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ListClients,
            &["list-clients"],
            "List connected clients",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Permissions,
            &["permissions"],
            "List explicit role assignments for a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Grant,
            &["grant"],
            "Grant a role to a client in a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Revoke,
            &["revoke"],
            "Revoke explicit role from a client in a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillSession,
            &["kill-session"],
            "Kill a session by name or UUID",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillAllSessions,
            &["kill-all-sessions"],
            "Kill all sessions",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Attach,
            &["attach"],
            "Attach to a session by name or UUID",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Detach,
            &["detach"],
            "Detach from the current session",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::NewWindow,
            &["new-window"],
            "Create a new window in a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ListWindows,
            &["list-windows"],
            "List windows for a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillWindow,
            &["kill-window"],
            "Kill a window by name, UUID, or active",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillAllWindows,
            &["kill-all-windows"],
            "Kill all windows in a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SwitchWindow,
            &["switch-window"],
            "Switch active window by name, UUID, or active",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Follow,
            &["follow"],
            "Follow another client's active target",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Unfollow,
            &["unfollow"],
            "Stop following a client",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Session,
            &["session"],
            "Session management commands",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionNew,
            &["session", "new"],
            "Create a new session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionList,
            &["session", "list"],
            "List active sessions",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionClients,
            &["session", "clients"],
            "List connected clients",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionPermissions,
            &["session", "permissions"],
            "List explicit role assignments for a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionGrant,
            &["session", "grant"],
            "Grant a role to a client in a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionRevoke,
            &["session", "revoke"],
            "Revoke explicit role from a client in a session",
            CoreCommandClass::PluginBackedShipped,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionKill,
            &["session", "kill"],
            "Kill a session by name or UUID",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionKillAll,
            &["session", "kill-all"],
            "Kill all sessions",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionAttach,
            &["session", "attach"],
            "Attach to a session by name or UUID",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionDetach,
            &["session", "detach"],
            "Detach from the current session",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionFollow,
            &["session", "follow"],
            "Follow another client's active target",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionUnfollow,
            &["session", "unfollow"],
            "Stop following a client",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Window,
            &["window"],
            "Window management commands",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::WindowNew,
            &["window", "new"],
            "Create a new window in a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::WindowList,
            &["window", "list"],
            "List windows for a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::WindowKill,
            &["window", "kill"],
            "Kill a window by name, UUID, or active",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::WindowKillAll,
            &["window", "kill-all"],
            "Kill all windows in a session",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::WindowSwitch,
            &["window", "switch"],
            "Switch active window by name, UUID, or active",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Server,
            &["server"],
            "Server lifecycle and status tools",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerStart,
            &["server", "start"],
            "Start the server",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerStatus,
            &["server", "status"],
            "Show server status",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerWhoamiPrincipal,
            &["server", "whoami-principal"],
            "Show active principal id",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerSave,
            &["server", "save"],
            "Save server state",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerRestore,
            &["server", "restore"],
            "Restore server state",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ServerStop,
            &["server", "stop"],
            "Stop the server",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Keymap,
            &["keymap"],
            "Keymap tools and diagnostics",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KeymapDoctor,
            &["keymap", "doctor"],
            "Inspect keymap configuration",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Terminal,
            &["terminal"],
            "Terminal capability tools and diagnostics",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::TerminalDoctor,
            &["terminal", "doctor"],
            "Run terminal diagnostics",
            CoreCommandClass::PluginBackedLater,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::TerminalInstallTerminfo,
            &["terminal", "install-terminfo"],
            "Install terminfo entry",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Plugin,
            &["plugin"],
            "Plugin discovery and execution tools",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::PluginList,
            &["plugin", "list"],
            "List discovered plugins",
            CoreCommandClass::CoreNative,
        ),
        BuiltInCliCommand::new(
            BuiltInHandlerId::PluginRun,
            &["plugin", "run"],
            "Run a plugin command explicitly",
            CoreCommandClass::CoreNative,
        ),
    ]
}

pub fn reserved_built_in_paths() -> BTreeSet<Vec<String>> {
    built_in_cli_commands()
        .into_iter()
        .flat_map(|command| command.all_paths().cloned().collect::<Vec<_>>())
        .collect()
}

pub fn built_in_command_by_handler(handler: BuiltInHandlerId) -> BuiltInCliCommand {
    built_in_cli_commands()
        .into_iter()
        .find(|command| command.handler == handler)
        .expect("built-in command handler should be registered")
}

#[cfg(test)]
mod tests {
    use super::{
        built_in_cli_commands, built_in_command_by_handler, reserved_built_in_paths,
        BuiltInHandlerId, CoreCommandClass,
    };

    #[test]
    fn reserved_paths_include_nested_plugin_run_path() {
        let paths = reserved_built_in_paths();
        assert!(paths.contains(&vec!["plugin".to_string(), "run".to_string()]));
    }

    #[test]
    fn built_in_table_contains_expected_handler() {
        let commands = built_in_cli_commands();
        assert!(commands.iter().any(|command| {
            command.handler == BuiltInHandlerId::Permissions
                && command.canonical_path == vec!["permissions".to_string()]
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
    fn permissions_is_marked_plugin_backed_shipped() {
        let command = built_in_command_by_handler(BuiltInHandlerId::Permissions);
        assert_eq!(command.class, CoreCommandClass::PluginBackedShipped);
    }

    #[test]
    fn grant_and_revoke_variants_are_marked_plugin_backed_shipped() {
        for handler in [
            BuiltInHandlerId::Grant,
            BuiltInHandlerId::Revoke,
            BuiltInHandlerId::SessionPermissions,
            BuiltInHandlerId::SessionGrant,
            BuiltInHandlerId::SessionRevoke,
        ] {
            let command = built_in_command_by_handler(handler);
            assert_eq!(command.class, CoreCommandClass::PluginBackedShipped);
        }
    }

    #[test]
    fn server_status_is_marked_core_native() {
        let command = built_in_command_by_handler(BuiltInHandlerId::ServerStatus);
        assert_eq!(command.class, CoreCommandClass::CoreNative);
    }
}
