fn production_section(source: &str) -> &str {
    source.split("\n#[cfg(test)]").next().unwrap_or(source)
}

fn assert_no_domain_markers(source: &str, context: &str) {
    let denied = [
        "bmux.permissions",
        "bmux.windows",
        "permission-query/v1",
        "permission-command/v1",
        "window-query/v1",
        "window-command/v1",
        // Typed interface ids replacing the legacy `window-*/v1` strings.
        // Core and CLI-runtime code must stay domain-agnostic; only the
        // windows plugin references these.
        "windows-state",
        "windows-commands",
        "windows-events",
        "Request::NewWindow",
        "Request::ListWindows",
        "Request::KillWindow",
        "Request::SwitchWindow",
        "Request::ListPermissions",
        "Request::GrantRole",
        "Request::RevokeRole",
        "permissions plugin",
        "permission_denied",
        "session: {session_label} | window:",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "{context} should not contain domain marker {marker}",
        );
    }
}

fn assert_no_raw_host_kernel_coupling(source: &str, context: &str) {
    let denied = [
        "bmux_ipc::",
        "HostKernelBridge",
        "call_service(",
        "\"session-query/v1\"",
        "\"session-command/v1\"",
        "\"pane-query/v1\"",
        "\"pane-command/v1\"",
        "\"client-query/v1\"",
        "\"storage-query/v1\"",
        "\"storage-command/v1\"",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "{context} should not contain raw host coupling marker {marker}",
        );
    }
}

fn runtime_sources() -> [(&'static str, &'static str); 11] {
    [
        (
            "packages/cli/src/runtime/mod.rs",
            include_str!("../src/runtime/mod.rs"),
        ),
        (
            "packages/cli/src/runtime/plugin_commands.rs",
            include_str!("../src/runtime/plugin_commands.rs"),
        ),
        (
            "packages/cli/src/runtime/built_in_commands.rs",
            include_str!("../src/runtime/built_in_commands.rs"),
        ),
        (
            "packages/cli/src/runtime/plugin_host.rs",
            include_str!("../src/runtime/plugin_host.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/mod.rs",
            include_str!("../src/runtime/attach/mod.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/cursor.rs",
            include_str!("../src/runtime/attach/cursor.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/events.rs",
            include_str!("../src/runtime/attach/events.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/layout.rs",
            include_str!("../src/runtime/attach/layout.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/render.rs",
            include_str!("../src/runtime/attach/render.rs"),
        ),
        (
            "packages/cli/src/runtime/attach/state.rs",
            include_str!("../src/runtime/attach/state.rs"),
        ),
        (
            "packages/cli/src/runtime/terminal_protocol.rs",
            include_str!("../src/runtime/terminal_protocol.rs"),
        ),
    ]
}

#[test]
fn runtime_production_code_is_domain_agnostic() {
    for (path, source) in runtime_sources() {
        let source = production_section(source);
        assert_no_domain_markers(source, path);
        assert!(
            !source.contains("bmux_clipboard::"),
            "{path} should not directly reference clipboard backend crate APIs",
        );
        assert!(
            !source.contains("clipboard-command/v1"),
            "{path} should not retain deprecated clipboard service interface clipboard-command/v1",
        );
    }
}

#[test]
fn core_packages_do_not_reference_domain_plugin_markers() {
    let core_sources = [
        (
            "packages/server/src/lib.rs",
            include_str!("../../server/src/lib.rs"),
        ),
        (
            "packages/server/src/persistence.rs",
            include_str!("../../server/src/persistence.rs"),
        ),
        (
            "packages/client/src/lib.rs",
            include_str!("../../client/src/lib.rs"),
        ),
        (
            "packages/ipc/src/lib.rs",
            include_str!("../../ipc/src/lib.rs"),
        ),
        (
            "packages/session/models/src/lib.rs",
            include_str!("../../session/models/src/lib.rs"),
        ),
        (
            "packages/event/models/src/lib.rs",
            include_str!("../../event/models/src/lib.rs"),
        ),
        (
            "packages/plugin-sdk/src/host_services.rs",
            include_str!("../../plugin-sdk/src/host_services.rs"),
        ),
        (
            "packages/plugin-sdk/src/lib.rs",
            include_str!("../../plugin-sdk/src/lib.rs"),
        ),
        (
            "packages/plugin/src/host_runtime.rs",
            include_str!("../../plugin/src/host_runtime.rs"),
        ),
        (
            "packages/plugin/src/lib.rs",
            include_str!("../../plugin/src/lib.rs"),
        ),
    ];

    for (path, source) in core_sources {
        assert_no_domain_markers(production_section(source), path);
    }
}

#[test]
fn plugin_production_code_uses_generic_host_api_only() {
    let plugin_sources = [
        (
            "plugins/windows-plugin/src/lib.rs",
            include_str!("../../../plugins/windows-plugin/src/lib.rs"),
        ),
        (
            "plugins/permissions-plugin/src/lib.rs",
            include_str!("../../../plugins/permissions-plugin/src/lib.rs"),
        ),
        (
            "plugins/plugin-cli-plugin/src/lib.rs",
            include_str!("../../../plugins/plugin-cli-plugin/src/lib.rs"),
        ),
        (
            "plugins/cluster-plugin/src/lib.rs",
            include_str!("../../../plugins/cluster-plugin/src/lib.rs"),
        ),
    ];

    for (path, source) in plugin_sources {
        assert_no_raw_host_kernel_coupling(production_section(source), path);
    }
}

/// Verify that `packages/event/models` is fully domain-agnostic. The
/// former `Session/Pane/Client/Input` event enums and constructors
/// must not silently reappear.
#[test]
fn event_core_crate_has_no_domain_event_types() {
    let sources = [(
        "packages/event/models/src/lib.rs",
        include_str!("../../event/models/src/lib.rs"),
    )];

    let denied = [
        "pub enum SessionEvent",
        "pub enum PaneEvent",
        "pub enum ClientEvent",
        "pub enum InputEvent",
        "pub enum SystemEvent",
        "pub enum Event",
        "fn session_created",
        "fn pane_created",
        "fn client_connected",
        "fn key_input",
        "fn mouse_input",
        "Session(SessionEvent)",
        "Pane(PaneEvent)",
        "Client(ClientEvent)",
        "Input(InputEvent)",
    ];

    for (path, source) in sources {
        let source = production_section(source);
        for marker in denied {
            assert!(
                !source.contains(marker),
                "{path} must stay domain-agnostic; reintroduced marker {marker}",
            );
        }
    }
}

/// Verify that the `bmux` umbrella crate doesn't re-export domain
/// crates. Only domain-agnostic building blocks should be exposed;
/// `session` and `terminal` features are not present.
#[test]
fn bmux_umbrella_has_no_domain_reexports() {
    let lib_source = include_str!("../../bmux/src/lib.rs");
    let manifest_source = include_str!("../../bmux/Cargo.toml");

    let lib_denied = [
        "bmux_session",
        "bmux_terminal",
        "pub use crate::session",
        "pub use crate::terminal",
        "SessionId",
        "SessionInfo",
        "SessionManager",
        "TerminalInstance",
        "TerminalManager",
        "PaneSize",
    ];
    for marker in lib_denied {
        assert!(
            !lib_source.contains(marker),
            "packages/bmux/src/lib.rs must not reference domain marker \
             {marker}",
        );
    }

    let manifest_denied = [
        "bmux_session",
        "bmux_terminal",
        "bmux_session_models",
        "bmux_terminal_models",
    ];
    for marker in manifest_denied {
        assert!(
            !manifest_source.contains(marker),
            "packages/bmux/Cargo.toml must not depend on domain crate \
             {marker}",
        );
    }
}

/// Verify that `packages/cli/src/lib.rs` doesn't re-export domain
/// types. `SessionId` / `SessionInfo` / `SessionManager` /
/// `TerminalInstance` / `TerminalManager` must not be re-exported.
#[test]
fn cli_crate_does_not_reexport_domain_types() {
    let source = include_str!("../src/lib.rs");
    let denied = [
        "pub use bmux_session::",
        "pub use bmux_terminal::",
        "pub use bmux_session_models::",
        "pub use bmux_terminal_models::",
        "SessionId",
        "SessionInfo",
        "SessionManager",
        "TerminalInstance",
        "TerminalManager",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "packages/cli/src/lib.rs must not re-export domain \
             marker {marker}; domain types belong in plugin-api crates",
        );
    }
}

/// Verify that `packages/plugin-sdk` is fully domain-agnostic. Domain
/// types (`Pane*`, `Session*`, `Context*`, `CurrentClient*`) live in
/// `packages/plugin-domain-compat`; they must not sneak back into the
/// SDK.
#[test]
fn plugin_sdk_has_no_domain_types() {
    let source = include_str!("../../plugin-sdk/src/host_services.rs");
    let denied = [
        "pub struct SessionSummary",
        "pub struct ContextSummary",
        "pub struct PaneSummary",
        "pub enum SessionSelector",
        "pub enum ContextSelector",
        "pub enum PaneSelector",
        "pub enum PaneSplitDirection",
        "pub enum PaneFocusDirection",
        "pub struct SessionCreateRequest",
        "pub struct SessionCreateResponse",
        "pub struct SessionListResponse",
        "pub struct SessionSelectRequest",
        "pub struct SessionSelectResponse",
        "pub struct CurrentClientResponse",
        "pub struct ContextCreateRequest",
        "pub struct ContextCreateResponse",
        "pub struct ContextListResponse",
        "pub struct ContextSelectRequest",
        "pub struct ContextSelectResponse",
        "pub struct ContextCloseRequest",
        "pub struct ContextCloseResponse",
        "pub struct ContextCurrentResponse",
        "pub struct PaneListRequest",
        "pub struct PaneListResponse",
        "pub struct PaneSplitRequest",
        "pub struct PaneSplitResponse",
        "pub struct PaneLaunchCommand",
        "pub struct PaneLaunchRequest",
        "pub struct PaneLaunchResponse",
        "pub struct PaneFocusRequest",
        "pub struct PaneFocusResponse",
        "pub struct PaneResizeRequest",
        "pub struct PaneResizeResponse",
        "pub struct PaneCloseRequest",
        "pub struct PaneCloseResponse",
        "pub struct PaneZoomRequest",
        "pub struct PaneZoomResponse",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "packages/plugin-sdk/src/host_services.rs is core \
             plugin infrastructure; domain type {marker} belongs in \
             packages/plugin-domain-compat instead",
        );
    }
}

/// Verify that `packages/session/models` stays minimal. Dead types
/// `LayoutError`, `PaneError`, `ClientError`, `ClientInfo`,
/// `SessionError`, `PaneId` must not be reintroduced.
#[test]
fn session_models_is_minimal() {
    let source = include_str!("../../session/models/src/lib.rs");
    let denied = [
        "pub enum LayoutError",
        "pub enum PaneError",
        "pub enum ClientError",
        "pub enum SessionError",
        "pub struct ClientInfo",
        "pub struct PaneId",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "packages/session/models/src/lib.rs must not reintroduce \
             dead type {marker}; the session-plugin owns these domain \
             types via typed dispatch",
        );
    }
}

/// Verify that `packages/event/models` doesn't depend on
/// session/terminal domain model crates. The Cargo.toml must not
/// silently regrow those deps.
#[test]
fn event_models_crate_has_no_domain_dependencies() {
    let source = include_str!("../../event/models/Cargo.toml");
    let denied = ["bmux_session_models", "bmux_terminal_models"];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "packages/event/models/Cargo.toml must not depend on {marker}; \
             domain event types must not be reintroduced",
        );
    }
}

/// Verify that `FollowState` is defined in `bmux_plugin_domain_compat`
/// and not in `packages/server`. The clients plugin's `activate`
/// callback registers a canonical handle into
/// [`bmux_plugin::PluginStateRegistry`]; server code uses a
/// locally-owned `Arc<RwLock<FollowState>>` constructed fresh per
/// server instance (so multi-server tests don't share state).
#[test]
fn follow_state_is_owned_by_clients_plugin() {
    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);

    let server_denied = [
        "struct FollowState {",
        "impl FollowState {",
        "struct FollowEntry {",
        "struct FollowTargetUpdate {",
        "follow_state: Mutex<FollowState>",
    ];
    for marker in server_denied {
        assert!(
            !server_source.contains(marker),
            "packages/server/src/lib.rs must not define {marker}; \
             FollowState lives in bmux_plugin_domain_compat and is \
             registered by the clients plugin",
        );
    }

    // Server must import FollowState from the neutral crate.
    assert!(
        server_source.contains("use bmux_plugin_domain_compat::"),
        "packages/server/src/lib.rs must import FollowState via \
         `use bmux_plugin_domain_compat::...`",
    );

    // Domain-compat hosts the canonical type.
    let compat_source = include_str!("../../plugin-domain-compat/src/follow_state.rs");
    assert!(
        compat_source.contains("pub struct FollowState"),
        "packages/plugin-domain-compat/src/follow_state.rs must export \
         the canonical `FollowState` struct",
    );

    // Clients plugin must register it on activate.
    let clients_source = include_str!("../../../plugins/clients-plugin/src/lib.rs");
    assert!(
        clients_source.contains("global_plugin_state_registry()"),
        "plugins/clients-plugin/src/lib.rs must register FollowState \
         into the global plugin state registry on activate",
    );
    assert!(
        clients_source.contains("register::<FollowState>"),
        "plugins/clients-plugin/src/lib.rs must call `register::<FollowState>` \
         to install the state handle",
    );
}

/// Verify that `ContextState` is defined in
/// `bmux_plugin_domain_compat` and not in `packages/server`. The
/// contexts plugin's `activate` callback registers a canonical handle
/// into [`bmux_plugin::PluginStateRegistry`]; server code uses a
/// locally-owned `Arc<RwLock<ContextState>>` per server instance.
#[test]
fn context_state_is_owned_by_contexts_plugin() {
    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);

    let server_denied = [
        "struct ContextState {",
        "impl ContextState {",
        "struct RuntimeContext {",
        "context_state: Mutex<ContextState>",
    ];
    for marker in server_denied {
        assert!(
            !server_source.contains(marker),
            "packages/server/src/lib.rs must not define {marker}; \
             ContextState lives in bmux_plugin_domain_compat and is \
             registered by the contexts plugin",
        );
    }

    // Domain-compat hosts the canonical type.
    let compat_source = include_str!("../../plugin-domain-compat/src/context_state.rs");
    assert!(
        compat_source.contains("pub struct ContextState"),
        "packages/plugin-domain-compat/src/context_state.rs must export \
         the canonical `ContextState` struct",
    );

    // Contexts plugin must register it on activate.
    let contexts_source = include_str!("../../../plugins/contexts-plugin/src/lib.rs");
    assert!(
        contexts_source.contains("global_plugin_state_registry()"),
        "plugins/contexts-plugin/src/lib.rs must register ContextState \
         into the global plugin state registry on activate",
    );
    assert!(
        contexts_source.contains("register::<ContextState>"),
        "plugins/contexts-plugin/src/lib.rs must call `register::<ContextState>` \
         to install the state handle",
    );
}

/// Verify that `SessionManager` is defined in
/// `bmux_plugin_domain_compat` and not in `packages/session` or
/// `packages/server`. The sessions plugin's `activate` callback
/// registers a canonical handle into
/// [`bmux_plugin::PluginStateRegistry`].
#[test]
fn session_manager_is_owned_by_sessions_plugin() {
    // `packages/session` is absent; SessionManager lives in
    // `bmux_plugin_domain_compat`.
    let session_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../session");
    assert!(
        !session_dir.join("Cargo.toml").exists(),
        "packages/session/Cargo.toml must be absent (only \
         packages/session/models survives as bmux_session_models)",
    );
    assert!(
        !session_dir.join("src/lib.rs").exists(),
        "packages/session/src/lib.rs must be absent; SessionManager \
         lives in bmux_plugin_domain_compat",
    );

    // Server must not define or Mutex-wrap SessionManager.
    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    let server_denied = [
        "pub struct SessionManager",
        "struct SessionManager {",
        "impl SessionManager {",
        "session_manager: Mutex<SessionManager>",
    ];
    for marker in server_denied {
        assert!(
            !server_source.contains(marker),
            "packages/server/src/lib.rs must not define {marker}; \
             SessionManager lives in bmux_plugin_domain_compat and is \
             registered by the sessions plugin",
        );
    }

    // Domain-compat hosts the canonical type.
    let compat_source = include_str!("../../plugin-domain-compat/src/session_manager.rs");
    assert!(
        compat_source.contains("pub struct SessionManager"),
        "packages/plugin-domain-compat/src/session_manager.rs must export \
         the canonical `SessionManager` struct",
    );

    // Sessions plugin must register it on activate.
    let sessions_source = include_str!("../../../plugins/sessions-plugin/src/lib.rs");
    assert!(
        sessions_source.contains("global_plugin_state_registry()"),
        "plugins/sessions-plugin/src/lib.rs must register SessionManager \
         into the global plugin state registry on activate",
    );
    assert!(
        sessions_source.contains("register::<SessionManager>"),
        "plugins/sessions-plugin/src/lib.rs must call `register::<SessionManager>` \
         to install the state handle",
    );
}

/// Verify that `packages/client` carries no domain convenience
/// methods. All session/context/pane/client operations must route
/// through `BmuxClient::invoke_service_raw` via typed plugin-api
/// dispatch, not through hand-coded IPC request methods.
#[test]
fn client_core_crate_has_no_domain_convenience_methods() {
    let source = include_str!("../../client/src/lib.rs");
    let source = production_section(source);

    let denied = [
        "pub async fn new_session",
        "pub async fn list_sessions",
        "pub async fn kill_session",
        "pub async fn list_clients",
        "pub async fn create_context",
        "pub async fn list_contexts",
        "pub async fn select_context",
        "pub async fn close_context",
        "pub async fn current_context",
        "pub async fn split_pane",
        "pub async fn launch_pane",
        "pub async fn focus_pane",
        "pub async fn resize_pane",
        "pub async fn close_pane",
        "pub async fn restart_pane",
        "pub async fn zoom_pane",
        "pub async fn list_panes",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "packages/client/src/lib.rs must not reintroduce domain \
             convenience method {marker}; route through typed dispatch \
             via invoke_service_raw instead",
        );
    }
}
