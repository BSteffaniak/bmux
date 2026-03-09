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
pub enum CoreCommandClass {
    CoreNative,
    PluginBackedLater,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct BuiltInCliCommand {
    pub handler: BuiltInHandlerId,
    pub canonical_path: Vec<String>,
    pub aliases: Vec<Vec<String>>,
    pub summary: &'static str,
    pub class: CoreCommandClass,
    pub domain: Option<PluginDomain>,
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
            domain: None,
        }
    }

    fn with_domain(mut self, domain: PluginDomain) -> Self {
        self.domain = Some(domain);
        self
    }

    pub fn all_paths(&self) -> impl Iterator<Item = &Vec<String>> {
        std::iter::once(&self.canonical_path).chain(self.aliases.iter())
    }
}

pub fn built_in_cli_commands() -> Vec<BuiltInCliCommand> {
    let _ = all_plugin_domains();
    vec![
        BuiltInCliCommand::new(
            BuiltInHandlerId::NewSession,
            &["new-session"],
            "Create a new session",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ListSessions,
            &["list-sessions"],
            "List active sessions",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::ListClients,
            &["list-clients"],
            "List connected clients",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Follow),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillSession,
            &["kill-session"],
            "Kill a session by name or UUID",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::KillAllSessions,
            &["kill-all-sessions"],
            "Kill all sessions",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
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
            BuiltInHandlerId::Follow,
            &["follow"],
            "Follow another client's active target",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Follow),
        BuiltInCliCommand::new(
            BuiltInHandlerId::Unfollow,
            &["unfollow"],
            "Stop following a client",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Follow),
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
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionList,
            &["session", "list"],
            "List active sessions",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionClients,
            &["session", "clients"],
            "List connected clients",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Follow),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionKill,
            &["session", "kill"],
            "Kill a session by name or UUID",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionKillAll,
            &["session", "kill-all"],
            "Kill all sessions",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Sessions),
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
        )
        .with_domain(PluginDomain::Follow),
        BuiltInCliCommand::new(
            BuiltInHandlerId::SessionUnfollow,
            &["session", "unfollow"],
            "Stop following a client",
            CoreCommandClass::PluginBackedLater,
        )
        .with_domain(PluginDomain::Follow),
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
        )
        .with_domain(PluginDomain::Diagnostics),
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
        )
        .with_domain(PluginDomain::Diagnostics),
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
        .filter(|command| command.class == CoreCommandClass::CoreNative)
        .filter(|command| !matches!(command.handler, BuiltInHandlerId::Session))
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
        BuiltInHandlerId, CoreCommandClass, PluginDomain, all_plugin_domains,
        built_in_cli_commands, built_in_command_by_handler, reserved_built_in_paths,
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
            built_in_command_by_handler(BuiltInHandlerId::SessionList).domain,
            Some(PluginDomain::Sessions)
        );
        assert_eq!(
            built_in_command_by_handler(BuiltInHandlerId::Follow).domain,
            Some(PluginDomain::Follow)
        );
    }

    #[test]
    fn migrated_plugin_owned_commands_are_not_in_core_execution_table() {
        let paths = built_in_cli_commands()
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
    fn server_status_is_marked_core_native() {
        let command = built_in_command_by_handler(BuiltInHandlerId::ServerStatus);
        assert_eq!(command.class, CoreCommandClass::CoreNative);
    }
}
