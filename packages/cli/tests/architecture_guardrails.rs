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

/// Verify that the performance-plugin crates exist and that core does
/// not define the `PerformanceCaptureSettings` or
/// `PerformanceEventRateLimiter` types. Both were relocated from
/// `packages/server/src/lib.rs` to
/// `plugins/performance-plugin-api/src/lib.rs`; server imports them
/// via `use bmux_performance_plugin_api::...` without depending on
/// the plugin impl crate.
#[test]
fn performance_plugin_exists() {
    let api_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/performance-plugin-api");
    let plugin_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/performance-plugin");
    assert!(
        api_dir.join("Cargo.toml").exists(),
        "plugins/performance-plugin-api/Cargo.toml must exist",
    );
    assert!(
        plugin_dir.join("Cargo.toml").exists(),
        "plugins/performance-plugin/Cargo.toml must exist",
    );

    let plugin_api_source = include_str!("../../../plugins/performance-plugin-api/src/lib.rs");
    let state_source = include_str!("../../../packages/performance-state/src/lib.rs");
    assert!(
        state_source.contains("pub struct PerformanceCaptureSettings"),
        "packages/performance-state/src/lib.rs must export the \
         canonical `PerformanceCaptureSettings` struct",
    );
    assert!(
        plugin_api_source.contains("pub struct PerformanceEventRateLimiter"),
        "plugins/performance-plugin-api/src/lib.rs must export the \
         canonical `PerformanceEventRateLimiter` struct",
    );
    assert!(
        state_source.contains("pub struct PerformanceSettingsHandle"),
        "packages/performance-state/src/lib.rs must export the \
         `PerformanceSettingsHandle` registry wrapper",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    assert!(
        !server_source.contains("struct PerformanceCaptureSettings {"),
        "packages/server/src/lib.rs must not define \
         `PerformanceCaptureSettings`; the type lives in \
         bmux_performance_plugin_api",
    );
    assert!(
        !server_source.contains("struct PerformanceEventRateLimiter {"),
        "packages/server/src/lib.rs must not define \
         `PerformanceEventRateLimiter`; the type lives in \
         bmux_performance_plugin_api",
    );
    assert!(
        server_source.contains("use bmux_performance_plugin_api::"),
        "packages/server/src/lib.rs must import performance types via \
         `use bmux_performance_plugin_api::...`",
    );
    assert!(
        server_source.contains("register::<PerformanceSettingsHandle>"),
        "packages/server/src/lib.rs must register a \
         `PerformanceSettingsHandle` into the plugin state registry so \
         the performance plugin can reach the active settings",
    );
    assert!(
        !server_source.contains("fn spawn_performance_events_bridge"),
        "packages/server/src/lib.rs must not define \
         `spawn_performance_events_bridge`; the performance plugin \
         now publishes `Event::PerformanceSettingsUpdated` directly \
         through the registered `WireEventSinkHandle`",
    );
}

/// Verify that `Request::{PerformanceStatus, PerformanceSet}` and
/// `ResponsePayload::{PerformanceStatus, PerformanceUpdated}` have
/// been deleted from `bmux_ipc`. Performance settings queries and
/// mutations go through the `bmux.performance` plugin's typed
/// `performance-commands::dispatch` service.
#[test]
fn performance_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "Request::PerformanceStatus",
        "Request::PerformanceSet",
        "ResponsePayload::PerformanceStatus",
        "ResponsePayload::PerformanceUpdated",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             performance settings go through \
             `performance-commands::dispatch` typed dispatch provided \
             by the `bmux.performance` plugin",
        );
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

/// Verify that `FollowState` is defined in `bmux_clients_plugin_api`
/// and not in `packages/server`. The clients plugin owns the type; it
/// lives in the plugin-api crate so server imports it via
/// `use bmux_clients_plugin_api::FollowState` without depending on the
/// plugin impl crate. The plugin registers a canonical handle into
/// [`bmux_plugin::PluginStateRegistry`] on `activate` for other
/// observers.
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
             FollowState lives in bmux_clients_plugin_api",
        );
    }

    // Server must reach follow-state through the domain-agnostic
    // `FollowStateHandle` from `bmux_client_state`, not through the
    // concrete plugin-api type. Core must not depend on the plugin
    // impl crate.
    assert!(
        server_source.contains("FollowStateHandle"),
        "packages/server/src/lib.rs must reach follow state through \
         `bmux_client_state::FollowStateHandle`",
    );

    // Clients plugin-api hosts the canonical type.
    let plugin_source = include_str!("../../../plugins/clients-plugin-api/src/follow_state.rs");
    assert!(
        plugin_source.contains("pub struct FollowState"),
        "plugins/clients-plugin-api/src/follow_state.rs must export \
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

/// Verify that `ContextState` is defined in `bmux_contexts_plugin_api`
/// and not in `packages/server`. The contexts plugin owns the type; it
/// lives in the plugin-api crate so server imports it via
/// `use bmux_contexts_plugin_api::ContextState` without depending on
/// the plugin impl crate.
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
             ContextState lives in bmux_contexts_plugin_api",
        );
    }

    // Server must reach context-state through the domain-agnostic
    // `ContextStateHandle` from `bmux_context_state`, not through the
    // concrete plugin-api type. Core must not depend on the plugin
    // impl crate.
    assert!(
        server_source.contains("ContextStateHandle"),
        "packages/server/src/lib.rs must reach context state through \
         `bmux_context_state::ContextStateHandle`",
    );

    // Contexts plugin-api hosts the canonical type.
    let plugin_source = include_str!("../../../plugins/contexts-plugin-api/src/context_state.rs");
    assert!(
        plugin_source.contains("pub struct ContextState"),
        "plugins/contexts-plugin-api/src/context_state.rs must export \
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

/// Verify that `SessionManager` is defined in `bmux_sessions_plugin_api`
/// and not in `packages/session` or `packages/server`. The sessions
/// plugin owns the type; it lives in the plugin-api crate so server
/// imports it via `use bmux_sessions_plugin_api::SessionManager`
/// without depending on the plugin impl crate.
#[test]
fn session_manager_is_owned_by_sessions_plugin() {
    // `packages/session` is absent; SessionManager lives in
    // `bmux_sessions_plugin`.
    let session_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../session");
    assert!(
        !session_dir.join("Cargo.toml").exists(),
        "packages/session/Cargo.toml must be absent (only \
         packages/session/models survives as bmux_session_models)",
    );
    assert!(
        !session_dir.join("src/lib.rs").exists(),
        "packages/session/src/lib.rs must be absent; SessionManager \
         lives in bmux_sessions_plugin",
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
             SessionManager lives in bmux_sessions_plugin_api",
        );
    }

    // Server must reach session-manager state through the
    // domain-agnostic `SessionManagerHandle` from `bmux_session_state`,
    // not through the concrete plugin-api type. Core must not depend
    // on the plugin impl crate.
    assert!(
        server_source.contains("SessionManagerHandle"),
        "packages/server/src/lib.rs must reach session-manager state \
         through `bmux_session_state::SessionManagerHandle`",
    );

    // Sessions plugin-api hosts the canonical type.
    let plugin_source = include_str!("../../../plugins/sessions-plugin-api/src/session_manager.rs");
    assert!(
        plugin_source.contains("pub struct SessionManager"),
        "plugins/sessions-plugin-api/src/session_manager.rs must export \
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

// Verify that the `bmux_plugin_domain_compat` crate has been fully
// eliminated. Domain helpers now live inside each plugin's private
// `domain_ipc` module, or are reached through typed BPDL services.
#[test]
fn domain_compat_crate_is_absent() {
    let compat_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin-domain-compat");
    assert!(
        !compat_dir.exists(),
        "packages/plugin-domain-compat/ must be absent; domain \
         helpers live in each plugin's private `domain_ipc` module \
         or are reached through typed BPDL services",
    );

    let workspace_toml = include_str!("../../../Cargo.toml");
    assert!(
        !workspace_toml.contains("packages/plugin-domain-compat"),
        "workspace Cargo.toml must not reference packages/plugin-domain-compat",
    );
    assert!(
        !workspace_toml.contains("bmux_plugin_domain_compat"),
        "workspace Cargo.toml must not declare bmux_plugin_domain_compat",
    );
}

// No crate anywhere in the workspace may depend on the deleted
// `bmux_plugin_domain_compat` crate, as a production or dev
// dependency, or in source code.
#[test]
fn no_crate_uses_domain_compat() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crate_roots = [
        workspace_root.join("packages"),
        workspace_root.join("plugins"),
        workspace_root.join("examples"),
    ];

    fn walk(
        dir: &std::path::Path,
        needle_toml: &str,
        needle_src: &str,
        offenders: &mut Vec<String>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if path.is_dir() {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                walk(&path, needle_toml, needle_src, offenders);
            } else if name == "Cargo.toml" {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && text.contains(needle_toml)
                {
                    offenders.push(path.display().to_string());
                }
            } else if name.ends_with(".rs")
                && name != "architecture_guardrails.rs"
                && let Ok(text) = std::fs::read_to_string(&path)
                && text.contains(needle_src)
            {
                offenders.push(path.display().to_string());
            }
        }
    }

    let mut offenders = Vec::new();
    for root in &crate_roots {
        walk(
            root,
            "bmux_plugin_domain_compat",
            "bmux_plugin_domain_compat",
            &mut offenders,
        );
    }

    assert!(
        offenders.is_empty(),
        "no crate may reference bmux_plugin_domain_compat; offenders: \
         {offenders:#?}",
    );
}

// Core architecture crates must not depend on any plugin crate.
// Plugins → core is allowed; core → plugins is forbidden.
#[test]
fn core_architecture_does_not_depend_on_plugins() {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let core_crates = [
        "packages/ipc",
        "packages/client",
        "packages/server",
        "packages/session/models",
        "packages/event/models",
        "packages/plugin-sdk",
        "packages/plugin-schema",
        "packages/plugin-schema-macros",
    ];

    let mut offenders = Vec::new();
    for crate_path in core_crates {
        let cargo_toml = workspace_root.join(crate_path).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&cargo_toml) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            // Heuristic: any dep name starting with `bmux_` and ending
            // with `_plugin` (the canonical plugin-crate suffix) is a
            // violation when declared as a dependency of a core crate.
            // Plugin-api crates (`bmux_*_plugin_api`) are acceptable —
            // they are neutral typed-dispatch surfaces.
            //
            // Exceptions: `bmux_plugin` (core plugin infrastructure),
            // `bmux_plugin_sdk` (core plugin SDK), and
            // `bmux_plugin_schema*` (core BPDL codegen) are core
            // primitives, not plugin impls.
            if let Some((name, _)) = trimmed.split_once('=')
                && let name = name.trim()
                && name.starts_with("bmux_")
                && name.ends_with("_plugin")
                && name != "bmux_plugin"
            {
                offenders.push(format!("{}: {name}", crate_path));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "core architecture crates must not depend on plugin crates; \
         offenders: {offenders:#?}",
    );
}

/// Verify that the client-domain IPC variants
/// (`Request::WhoAmI`, `Request::ListClients`) have been deleted from
/// `bmux_ipc` and replaced with typed dispatch through the clients
/// plugin's `clients-state::current-client` / `list-clients` surface.
#[test]
fn client_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "    WhoAmI,",
        "    ListClients,",
        "ResponsePayload::ClientIdentity",
        "ResponsePayload::ClientList {",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             client identity and list operations go through typed \
             `clients-state` dispatch",
        );
    }
}

/// Verify that `Request::ControlCatalogSnapshot` has been deleted from
/// `bmux_ipc` and that catalog snapshots are served by the new
/// `bmux.control_catalog` plugin via typed dispatch.
#[test]
fn control_catalog_snapshot_ipc_variant_is_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    assert!(
        !ipc_source.contains("    ControlCatalogSnapshot {\n        /// Optional"),
        "packages/ipc/src/lib.rs must not reintroduce `Request::ControlCatalogSnapshot`; \
         catalog snapshots go through `control-catalog-state::snapshot` typed dispatch",
    );
    assert!(
        !ipc_source.contains("ResponsePayload::ControlCatalogSnapshot"),
        "packages/ipc/src/lib.rs must not reintroduce \
         `ResponsePayload::ControlCatalogSnapshot`",
    );
}

/// Verify that the `bmux.control_catalog` plugin crate exists and
/// owns the catalog revision counter. The counter used to live in
/// `ServerState.control_catalog_revision`; after the migration the
/// plugin owns it and server only bridges the typed `CatalogEvent`
/// into `Event::ControlCatalogChanged` for cross-process subscribers.
#[test]
fn control_catalog_plugin_exists() {
    let api_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/control-catalog-plugin-api");
    let plugin_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/control-catalog-plugin");
    assert!(
        api_dir.join("Cargo.toml").exists(),
        "plugins/control-catalog-plugin-api/Cargo.toml must exist",
    );
    assert!(
        plugin_dir.join("Cargo.toml").exists(),
        "plugins/control-catalog-plugin/Cargo.toml must exist",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    assert!(
        !server_source.contains("control_catalog_revision: AtomicU64"),
        "packages/server/src/lib.rs must not own the control-catalog \
         revision counter; that state lives in the control-catalog plugin",
    );
    assert!(
        !server_source.contains("fn emit_control_catalog_changed("),
        "packages/server/src/lib.rs must not define \
         `emit_control_catalog_changed`; the control-catalog plugin \
         now publishes `Event::ControlCatalogChanged` directly \
         through the registered `WireEventSinkHandle`",
    );
    assert!(
        server_source.contains("register_wire_event_sink"),
        "packages/server/src/lib.rs must register a \
         `WireEventSinkHandle` into the plugin state registry so \
         plugins can publish wire events directly (replacing the \
         former per-plugin event bridges)",
    );
}

/// Verify that the follow-client IPC variants
/// (`Request::FollowClient`, `Request::Unfollow`) and their response
/// payloads (`ResponsePayload::FollowStarted`,
/// `ResponsePayload::FollowStopped`) have been deleted. Follow
/// orchestration lives in `clients-plugin`'s typed
/// `clients-commands::set-following` handler; the server bridges the
/// plugin's typed `ClientEvent::{FollowStarted, FollowStopped,
/// FollowTargetChanged}` into the legacy wire
/// `Event::{FollowStarted, FollowStopped, FollowTargetChanged}`.
#[test]
fn follow_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "    FollowClient {",
        "    Unfollow,",
        "ResponsePayload::FollowStarted",
        "ResponsePayload::FollowStopped",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             follow orchestration goes through typed \
             `clients-commands::set-following` dispatch",
        );
    }

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    assert!(
        !server_source.contains("fn spawn_client_events_bridge"),
        "packages/server/src/lib.rs must not define \
         `spawn_client_events_bridge`; the clients plugin now \
         publishes `Event::{{FollowStarted, FollowStopped, \
         FollowTargetChanged}}` directly through the registered \
         `WireEventSinkHandle`",
    );
}

/// Verify that the recording-plugin crates exist and that core does
/// not define the `RecordingRuntime` type. The type was relocated from
/// `packages/server/src/recording.rs` to
/// `plugins/recording-plugin-api/src/recording_runtime.rs` in Slice 10;
/// server imports it via `use bmux_recording_plugin_api::RecordingRuntime`
/// without depending on the plugin impl crate.
#[test]
fn recording_plugin_exists() {
    let api_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/recording-plugin-api");
    let plugin_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/recording-plugin");
    assert!(
        api_dir.join("Cargo.toml").exists(),
        "plugins/recording-plugin-api/Cargo.toml must exist",
    );
    assert!(
        plugin_dir.join("Cargo.toml").exists(),
        "plugins/recording-plugin/Cargo.toml must exist",
    );

    let plugin_api_source =
        include_str!("../../../plugins/recording-plugin-api/src/recording_runtime.rs");
    assert!(
        plugin_api_source.contains("pub struct RecordingRuntime"),
        "plugins/recording-plugin-api/src/recording_runtime.rs must \
         export the canonical `RecordingRuntime` struct",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    assert!(
        !server_source.contains("pub struct RecordingRuntime"),
        "packages/server/src/lib.rs must not define `RecordingRuntime`; \
         the type lives in bmux_recording_plugin_api",
    );
    assert!(
        server_source.contains("use bmux_recording_plugin_api::"),
        "packages/server/src/lib.rs must import recording types via \
         `use bmux_recording_plugin_api::...`",
    );
    assert!(
        server_source.contains("register::<RecordingSinkHandle>"),
        "packages/server/src/lib.rs must register a `RecordingSinkHandle` \
         into the plugin state registry so the recording plugin can \
         reach the active recording runtimes",
    );
}

/// Verify that `Request::Recording*` (15 variants) and
/// `ResponsePayload::Recording*` variants have been deleted from
/// `bmux_ipc`. Recording lifecycle operations are served by the
/// `bmux.recording` plugin's typed `recording-commands::dispatch`
/// service, which takes a `RecordingRequest` and returns a
/// `RecordingResponse` (both defined in
/// `bmux_recording_plugin_api`).
#[test]
fn recording_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    // These patterns match only `Request` / `ResponsePayload` variant
    // definitions. We intentionally do NOT match `Event::RecordingStarted`
    // or `Event::RecordingStopped` — those remain on the `Event` enum
    // because attach clients still consume them to coordinate display-
    // track writes.
    let denied = [
        // Request variants (the `Request` enum uses indent = 4 spaces
        // and always has at least one named field record shape).
        "Request::RecordingStart",
        "Request::RecordingStop",
        "Request::RecordingStatus",
        "Request::RecordingList",
        "Request::RecordingDelete",
        "Request::RecordingWriteCustomEvent",
        "Request::RecordingDeleteAll",
        "Request::RecordingCut",
        "Request::RecordingRollingStart",
        "Request::RecordingRollingStop",
        "Request::RecordingRollingStatus",
        "Request::RecordingRollingClear",
        "Request::RecordingCaptureTargets",
        "Request::RecordingPrune",
        // ResponsePayload variants.
        "ResponsePayload::RecordingStarted",
        "ResponsePayload::RecordingStopped",
        "ResponsePayload::RecordingStatus",
        "ResponsePayload::RecordingList",
        "ResponsePayload::RecordingDeleted",
        "ResponsePayload::RecordingCustomEventWritten",
        "ResponsePayload::RecordingDeleteAll",
        "ResponsePayload::RecordingCut",
        "ResponsePayload::RecordingCaptureTargets",
        "ResponsePayload::RecordingRollingStatus",
        "ResponsePayload::RecordingRollingCleared",
        "ResponsePayload::RecordingPruned",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             recording lifecycle operations go through \
             `recording-commands::dispatch` typed dispatch provided \
             by the `bmux.recording` plugin",
        );
    }
}
