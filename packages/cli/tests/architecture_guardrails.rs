/// Strip the `mod tests { ... }` block from a source file so
/// downstream assertions only consider production code.
///
/// Splits on `\nmod tests {` (the unique test-module header shape
/// used across this crate's production files) rather than any
/// `#[cfg(test)]` attribute, because those attributes can legitimately
/// appear on individual test helpers that live in module scope.
fn production_section(source: &str) -> &str {
    source.split("\nmod tests {").next().unwrap_or(source)
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
    // Legitimate typed dispatch uses `call_service(...)` on
    // `ServiceCaller`. The raw-coupling check focuses on markers
    // that indicate bypassing the typed surface (direct IPC enum
    // references, host-kernel bridge handles, and the legacy
    // `/v1` interface ids that were replaced by typed BPDL
    // services).
    let denied = [
        "bmux_ipc::",
        "HostKernelBridge",
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
/// `PerformanceEventRateLimiter` types. Both live in the neutral
/// `packages/performance-state` crate so server does not import the
/// performance plugin API crate for runtime support types.
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

    let state_source = include_str!("../../../packages/performance-state/src/lib.rs");
    assert!(
        state_source.contains("pub struct PerformanceCaptureSettings"),
        "packages/performance-state/src/lib.rs must export the \
         canonical `PerformanceCaptureSettings` struct",
    );
    assert!(
        state_source.contains("pub struct PerformanceEventRateLimiter"),
        "packages/performance-state/src/lib.rs must export the \
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
          bmux_performance_state",
    );
    assert!(
        !server_source.contains("struct PerformanceEventRateLimiter {"),
        "packages/server/src/lib.rs must not define \
         `PerformanceEventRateLimiter`; the type lives in \
          bmux_performance_state",
    );
    assert!(
        !server_source.contains("bmux_performance_plugin_api"),
        "packages/server/src/lib.rs must not import \
          `bmux_performance_plugin_api`; runtime support types live in \
          neutral packages",
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
         `spawn_performance_events_bridge`; performance settings updates \
         flow through the generated performance BPDL event and generic \
         plugin-bus forwarder",
    );
}

/// Server is core runtime architecture and must not import plugin API
/// crates for domain support types. Plugin API crates are allowed in
/// CLI/plugin consumers, but not in `packages/server`.
#[test]
fn server_does_not_import_plugin_api_crates() {
    let server_source = production_section(include_str!("../../server/src/lib.rs"));
    assert!(
        !server_source.contains("bmux_performance_plugin_api"),
        "packages/server/src/lib.rs must not import bmux_performance_plugin_api",
    );
    assert!(
        !server_source.contains("bmux_recording_plugin_api"),
        "packages/server/src/lib.rs must not import bmux_recording_plugin_api",
    );

    let server_cargo = include_str!("../../server/Cargo.toml");
    assert!(
        !server_cargo.contains("bmux_performance_plugin_api"),
        "packages/server/Cargo.toml must not depend on bmux_performance_plugin_api",
    );
    assert!(
        !server_cargo.contains("bmux_recording_plugin_api"),
        "packages/server/Cargo.toml must not depend on bmux_recording_plugin_api",
    );
}

/// The host-side plugin runtime is core architecture. It may expose
/// generic recording primitives, but it must not construct generated
/// request/response types from the recording plugin API crate.
#[test]
fn plugin_host_crate_does_not_import_plugin_api_crates() {
    let plugin_source = production_section(include_str!("../../plugin/src/host_runtime.rs"));
    let plugin_cargo = include_str!("../../plugin/Cargo.toml");
    for marker in [
        "bmux_recording_plugin_api",
        "bmux_performance_plugin_api",
        "bmux_sessions_plugin_api",
        "bmux_contexts_plugin_api",
        "bmux_clients_plugin_api",
        "bmux_windows_plugin_api",
        "bmux_permissions_plugin_api",
        "bmux_pane_runtime_plugin_api",
    ] {
        assert!(
            !plugin_source.contains(marker),
            "packages/plugin/src/host_runtime.rs must not import `{marker}`; core host runtime stays domain-agnostic",
        );
        assert!(
            !plugin_cargo.contains(marker),
            "packages/plugin/Cargo.toml must not depend on `{marker}`; core host runtime stays domain-agnostic",
        );
    }
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

/// Verify that `FollowState` is defined in the clients plugin impl
/// crate and not in `packages/server` or the plugin API crate. The
/// clients plugin owns the concrete type; server observes it through a
/// neutral `bmux_client_state::FollowStateHandle` registered into
/// [`bmux_plugin::PluginStateRegistry`] on `activate`.
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
             FollowState lives in plugins/clients-plugin/src/follow_state.rs",
        );
    }

    // Server must reach follow-state through the domain-agnostic
    // `FollowStateHandle` from `bmux_client_state`, not through the
    // concrete plugin-owned type. Core must not depend on the plugin
    // impl crate.
    assert!(
        server_source.contains("FollowStateHandle"),
        "packages/server/src/lib.rs must reach follow state through \
         `bmux_client_state::FollowStateHandle`",
    );

    // Clients plugin impl crate hosts the canonical `FollowState` type.
    let plugin_source = include_str!("../../../plugins/clients-plugin/src/follow_state.rs");
    assert!(
        plugin_source.contains("pub struct FollowState"),
        "plugins/clients-plugin/src/follow_state.rs must export \
         the canonical `FollowState` struct",
    );

    // Clients-plugin-api crate must NOT define the concrete
    // `FollowState` (that would violate the one-way rule — plugin-api
    // crates host stable wire contracts, plugin impl crates host
    // concrete state).
    let plugin_api_source = include_str!("../../../plugins/clients-plugin-api/src/lib.rs");
    assert!(
        !plugin_api_source.contains("pub struct FollowState"),
        "plugins/clients-plugin-api/src/lib.rs must not define \
         `FollowState`; the concrete type lives in \
         `plugins/clients-plugin/src/follow_state.rs`",
    );
}

/// Verify that `ContextState` is defined in the contexts plugin impl
/// crate and not in `packages/server` or the plugin API crate. Server
/// observes it through a neutral `bmux_context_state::ContextStateHandle`.
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
             ContextState lives in plugins/contexts-plugin/src/context_state.rs",
        );
    }

    // Server must reach context-state through the domain-agnostic
    // `ContextStateHandle` from `bmux_context_state`, not through the
    // concrete plugin-owned type. Core must not depend on the plugin
    // impl crate.
    assert!(
        server_source.contains("ContextStateHandle"),
        "packages/server/src/lib.rs must reach context state through \
         `bmux_context_state::ContextStateHandle`",
    );

    // Contexts plugin impl crate hosts the canonical `ContextState` type.
    let plugin_source = include_str!("../../../plugins/contexts-plugin/src/context_state.rs");
    assert!(
        plugin_source.contains("pub struct ContextState"),
        "plugins/contexts-plugin/src/context_state.rs must export \
         the canonical `ContextState` struct",
    );

    // Contexts-plugin-api crate must NOT define the concrete
    // `ContextState`.
    let plugin_api_source = include_str!("../../../plugins/contexts-plugin-api/src/lib.rs");
    assert!(
        !plugin_api_source.contains("pub struct ContextState"),
        "plugins/contexts-plugin-api/src/lib.rs must not define \
         `ContextState`; the concrete type lives in \
         `plugins/contexts-plugin/src/context_state.rs`",
    );
}

/// Verify that `SessionManager` is defined in the sessions plugin impl
/// crate and not in `packages/session`, `packages/server`, or the plugin
/// API crate. Server observes it through a neutral
/// `bmux_session_state::SessionManagerHandle`.
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
             SessionManager lives in plugins/sessions-plugin/src/session_manager.rs",
        );
    }

    // Server must reach session-manager state through the
    // domain-agnostic `SessionManagerHandle` from `bmux_session_state`,
    // not through the concrete plugin-owned type. Core must not depend
    // on the plugin impl crate.
    assert!(
        server_source.contains("SessionManagerHandle"),
        "packages/server/src/lib.rs must reach session-manager state \
         through `bmux_session_state::SessionManagerHandle`",
    );

    // Sessions plugin impl crate hosts the canonical `SessionManager` type.
    let plugin_source = include_str!("../../../plugins/sessions-plugin/src/session_manager.rs");
    assert!(
        plugin_source.contains("pub struct SessionManager"),
        "plugins/sessions-plugin/src/session_manager.rs must export \
         the canonical `SessionManager` struct",
    );

    // Sessions-plugin-api crate must NOT define the concrete
    // `SessionManager`.
    let plugin_api_source = include_str!("../../../plugins/sessions-plugin-api/src/lib.rs");
    assert!(
        !plugin_api_source.contains("pub struct SessionManager"),
        "plugins/sessions-plugin-api/src/lib.rs must not define \
         `SessionManager`; the concrete type lives in \
         `plugins/sessions-plugin/src/session_manager.rs`",
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
// eliminated. Domain workflows should use BPDL-generated clients and
// small purpose-named local helpers, not broad compatibility crates.
#[test]
fn domain_compat_crate_is_absent() {
    let compat_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin-domain-compat");
    assert!(
        !compat_dir.exists(),
        "packages/plugin-domain-compat/ must be absent; domain \
         workflows should use BPDL-generated clients and small \
         purpose-named local helpers",
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
            // Plugin-api crates are checked by stricter boundary tests
            // because current transitional deps need explicit tracking.
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
    let denied_models = [
        "pub struct ContextSessionBindingSummary",
        "pub struct ControlCatalogSnapshot",
    ];
    for marker in denied_models {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             catalog snapshots go through `control-catalog-state::snapshot` typed dispatch",
        );
    }
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
/// plugin owns it and streaming clients receive generated
/// `control-catalog-events` through the generic plugin-bus forwarder.
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
         emits generated `control-catalog-events` forwarded through \
         the generic plugin-bus bridge",
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
/// `ResponsePayload::FollowStopped`) and legacy follow `Event` variants
/// have been deleted. Follow orchestration lives in `clients-plugin`'s
/// typed `clients-commands::set-following` handler; streaming clients
/// receive generated `clients-events` through the generic plugin-bus
/// forwarder.
#[test]
fn follow_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "    FollowClient {",
        "    Unfollow,",
        "ResponsePayload::FollowStarted",
        "ResponsePayload::FollowStopped",
        "Event::FollowStarted",
        "Event::FollowStopped",
        "Event::FollowTargetGone",
        "Event::FollowTargetChanged",
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
         `spawn_client_events_bridge`; the clients plugin emits \
         generated `clients-events` forwarded through the generic \
         plugin-bus bridge",
    );
}

/// Verify that the recording-plugin crates exist and that core does
/// not define the `RecordingRuntime` type. The concrete runtime lives
/// in the recording plugin implementation crate; core reaches recording
/// through neutral runtime handles and generic host primitives.
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

    let plugin_source = include_str!("../../../plugins/recording-plugin/src/recording_runtime.rs");
    assert!(
        plugin_source.contains("pub struct RecordingRuntime"),
        "plugins/recording-plugin/src/recording_runtime.rs must \
         export the canonical `RecordingRuntime` struct",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    assert!(
        !server_source.contains("pub struct RecordingRuntime"),
        "packages/server/src/lib.rs must not define `RecordingRuntime`",
    );
    assert!(
        !server_source.contains("RecordingRuntime::new"),
        "packages/server/src/lib.rs must not construct \
         `RecordingRuntime`; the recording plugin owns construction \
         during its `activate` callback",
    );
    assert!(
        !server_source.contains("manual_recording_runtime:"),
        "packages/server/src/lib.rs must not hold a \
         `manual_recording_runtime` field on `ServerState`; the \
         recording plugin owns runtime instances",
    );

    // Plugin impl crate must register the sink + runtime handles
    // on `activate`.
    let plugin_lib = include_str!("../../../plugins/recording-plugin/src/lib.rs");
    assert!(
        plugin_lib.contains("register::<RecordingSinkHandle>"),
        "plugins/recording-plugin/src/lib.rs must register a \
         `RecordingSinkHandle` into the plugin state registry on \
         `activate`",
    );
    assert!(
        plugin_lib.contains("register::<ManualRecordingRuntimeHandle>"),
        "plugins/recording-plugin/src/lib.rs must register a \
         `ManualRecordingRuntimeHandle` on `activate`",
    );
    assert!(
        plugin_lib.contains("register::<RollingRecordingRuntimeHandle>"),
        "plugins/recording-plugin/src/lib.rs must register a \
         `RollingRecordingRuntimeHandle` on `activate`",
    );
}

/// Verify that `Request::Recording*` (15 variants) and
/// `ResponsePayload::Recording*` variants have been deleted from
/// `bmux_ipc`. Recording lifecycle operations are served by the
/// `bmux.recording` plugin's BPDL-generated `recording-state` and
/// `recording-commands` service operations.
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
             BPDL-generated typed dispatch provided \
             by the `bmux.recording` plugin",
        );
    }
}

/// Verify `bmux_client` stays close to protocol primitives. Most plugin
/// API dependencies are forbidden here; typed-domain workflows belong in
/// generated clients or consuming-crate private helpers.
#[test]
fn bmux_client_is_pure_protocol() {
    let cargo_toml = include_str!("../../client/Cargo.toml");
    let denied_patterns = [
        "bmux_clients_plugin_api",
        "bmux_contexts_plugin_api",
        "bmux_sessions_plugin_api",
        "bmux_recording_plugin_api",
        "bmux_performance_plugin_api",
        "bmux_control_catalog_plugin_api",
        "bmux_windows_plugin_api",
        "bmux_decoration_plugin_api",
        "bmux_pane_runtime_plugin_api",
    ];
    for pattern in denied_patterns {
        assert!(
            !cargo_toml.contains(pattern),
            "packages/client/Cargo.toml must not depend on `{pattern}`; \
             typed-domain workflows belong in generated clients or \
             consuming-crate private helpers, not in `bmux_client`",
        );
    }
}

/// Source-like backup files under active package/plugin trees preserve
/// stale architecture in grep results and can confuse boundary audits.
#[test]
fn source_tree_has_no_backup_rust_files() {
    fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                visit(&path, offenders);
            } else if name.ends_with(".rs.bak") || name.ends_with(".bak.rs") {
                offenders.push(
                    path.strip_prefix(repo_root())
                        .unwrap_or(path.as_path())
                        .display()
                        .to_string(),
                );
            }
        }
    }

    let mut offenders = Vec::new();
    for root in [repo_root().join("packages"), repo_root().join("plugins")] {
        visit(&root, &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "source backup files must not live under packages/ or plugins/: {offenders:#?}",
    );
}

/// Verify that `ServerState` doesn't hold concrete plugin-owned state
/// types as fields. Server reaches domain state exclusively through
/// the domain-agnostic `*Handle` trait objects registered in the
/// plugin state registry.
#[test]
fn server_state_holds_no_concrete_domain_state() {
    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);
    let denied = [
        "follow_state: Arc<std::sync::RwLock<FollowState>>",
        "context_state: Arc<std::sync::RwLock<ContextState>>",
        "session_manager: Arc<std::sync::RwLock<SessionManager>>",
        "manual_recording_runtime: Arc<Mutex<RecordingRuntime>>",
        "rolling_recording_runtime: Arc<Mutex<Option<RecordingRuntime>>>",
    ];
    for pattern in denied {
        assert!(
            !server_source.contains(pattern),
            "packages/server/src/lib.rs must not hold `{pattern}` on \
             `ServerState`; plugin-owned state is reached through \
             domain-agnostic `*Handle` trait objects from the plugin \
             state registry",
        );
    }
}

/// Verify that plugin-api crates don't define concrete state types.
/// Plugin-api crates host stable wire contracts (BPDL-generated types,
/// generated clients, events, capabilities, and stable models).
/// Concrete state types live in plugin impl crates so the plugin owns
/// construction and the server never names them.
#[test]
fn plugin_api_crates_have_no_concrete_state() {
    let clients_api = include_str!("../../../plugins/clients-plugin-api/src/lib.rs");
    assert!(
        !clients_api.contains("pub struct FollowState"),
        "plugins/clients-plugin-api must not define `FollowState`",
    );

    let contexts_api = include_str!("../../../plugins/contexts-plugin-api/src/lib.rs");
    assert!(
        !contexts_api.contains("pub struct ContextState"),
        "plugins/contexts-plugin-api must not define `ContextState`",
    );

    let sessions_api = include_str!("../../../plugins/sessions-plugin-api/src/lib.rs");
    assert!(
        !sessions_api.contains("pub struct SessionManager"),
        "plugins/sessions-plugin-api must not define `SessionManager`",
    );

    let recording_api = include_str!("../../../plugins/recording-plugin-api/src/lib.rs");
    assert!(
        !recording_api.contains("pub struct RecordingRuntime"),
        "plugins/recording-plugin-api must not define `RecordingRuntime`",
    );
}

/// Public plugin API crates must not host handwritten transport-client
/// modules. Callers should use generated clients or consuming-crate
/// private helpers instead.
#[test]
fn plugin_api_crates_do_not_define_public_typed_clients() {
    for plugin_api_dir in iter_plugin_crate_dirs().into_iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with("-plugin-api"))
    }) {
        let typed_client = plugin_api_dir.join("src/typed_client.rs");
        let relative = typed_client
            .strip_prefix(repo_root())
            .unwrap_or(typed_client.as_path())
            .display();
        assert!(
            !typed_client.exists(),
            "{relative} must not exist; callers should use generated clients or consuming-crate private helpers",
        );
    }
}

/// Recording, performance, and snapshot service transport is generated from BPDL;
/// public API crates must not reintroduce handwritten request/response
/// envelopes for those service calls.
#[test]
fn generated_transport_api_crates_do_not_define_public_envelopes() {
    for (label, source, markers) in [
        (
            "plugins/recording-plugin-api",
            include_str!("../../../plugins/recording-plugin-api/src/lib.rs"),
            ["pub enum RecordingRequest", "pub enum RecordingResponse"].as_slice(),
        ),
        (
            "plugins/performance-plugin-api",
            include_str!("../../../plugins/performance-plugin-api/src/lib.rs"),
            [
                "pub enum PerformanceRequest",
                "pub enum PerformanceResponse",
            ]
            .as_slice(),
        ),
        (
            "plugins/snapshot-plugin-api",
            include_str!("../../../plugins/snapshot-plugin-api/src/lib.rs"),
            ["pub enum SnapshotRequest", "pub enum SnapshotResponse"].as_slice(),
        ),
    ] {
        for marker in markers {
            assert!(
                !source.contains(marker),
                "{label} must not reintroduce `{marker}`; use BPDL-generated operations",
            );
        }
    }
}

/// Plugin impl crates should call typed BPDL clients directly or use
/// small purpose-named local helpers. The old private `domain_ipc`
/// compatibility modules duplicated typed contracts and hid generated
/// clients behind another domain-shaped transport layer.
#[test]
fn plugin_impl_crates_do_not_define_private_domain_ipc_modules() {
    for plugin_dir in iter_plugin_crate_dirs().into_iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| !name.to_string_lossy().ends_with("-plugin-api"))
    }) {
        let relative = plugin_dir
            .strip_prefix(repo_root())
            .unwrap_or(plugin_dir.as_path())
            .display();
        assert!(
            !plugin_dir.join("src/domain_ipc.rs").exists(),
            "{relative}/src/domain_ipc.rs must not exist; use generated BPDL clients directly",
        );
        for source in rust_source_files(&plugin_dir) {
            let text = std::fs::read_to_string(&source).expect("source should be readable");
            let source_relative = source
                .strip_prefix(repo_root())
                .unwrap_or(source.as_path())
                .display();
            assert!(
                !text.contains("mod domain_ipc"),
                "{source_relative} must not reintroduce private domain_ipc modules",
            );
        }
    }
}

/// Foundational plugin cross-calls that have generated BPDL clients
/// should not regress to handwritten service strings or ad-hoc
/// `call_service` payload shims.
#[test]
fn migrated_plugin_cross_calls_use_generated_clients() {
    let clients = production_section(include_str!("../../../plugins/clients-plugin/src/lib.rs"));
    for marker in [
        "\"bmux.contexts.",
        "\"bmux.sessions.",
        "\"contexts-commands\"",
        "\"sessions-commands\"",
    ] {
        assert!(
            !clients.contains(marker),
            "clients-plugin must use generated clients instead of handwritten cross-plugin marker `{marker}`",
        );
    }

    let contexts = production_section(include_str!("../../../plugins/contexts-plugin/src/lib.rs"));
    assert!(
        !contexts.contains(".call_service_raw("),
        "contexts-plugin must use generated clients for sessions-plugin orchestration",
    );

    let permissions = production_section(include_str!(
        "../../../plugins/permissions-plugin/src/lib.rs"
    ));
    assert!(
        !permissions.contains(".call_service("),
        "permissions-plugin must use generated clients for existing typed plugin contracts",
    );
}

/// The monolithic `SnapshotV4` schema plus the `SnapshotManager` +
/// `SnapshotRuntime` machinery and the entire
/// `packages/server/src/persistence.rs` file have been deleted.
/// Persistence flows through the `bmux.snapshot` plugin via
/// `SnapshotOrchestratorHandle` (trait object registered in the
/// plugin state registry); server must not reintroduce any of the
/// legacy schema or functions.
#[test]
fn server_does_not_define_snapshot_schema() {
    let persistence_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../server/src/persistence.rs");
    assert!(
        !persistence_path.exists(),
        "packages/server/src/persistence.rs must remain deleted; the \
         monolithic snapshot schema was replaced by the `bmux.snapshot` \
         plugin",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_source = production_section(server_source);

    // Legacy schema types — must not be declared in server.
    let denied_types = [
        "struct SnapshotV4",
        "struct SnapshotEnvelopeV4",
        "struct SnapshotManager",
        "struct SnapshotRuntime",
        "struct SessionSnapshotV3",
        "struct ContextSnapshotV1",
        "struct FollowEdgeSnapshotV2",
        "struct ClientSelectedSessionSnapshotV2",
        "struct ClientSelectedContextSnapshotV1",
        "struct ContextSessionBindingSnapshotV1",
        "struct PaneSnapshotV2",
        "struct FloatingSurfaceSnapshotV3",
        "enum PaneLayoutNodeSnapshotV2",
        "enum PaneSplitDirectionSnapshotV2",
    ];
    for ty in denied_types {
        assert!(
            !server_source.contains(ty),
            "packages/server/src/lib.rs must not define `{ty}`; the \
             legacy monolithic snapshot schema has been deleted",
        );
    }

    // Legacy pipeline functions — must not be redefined in server.
    let denied_fns = [
        "fn build_snapshot",
        "fn apply_snapshot_state",
        "fn restore_snapshot_replace",
        "fn restore_snapshot_if_present",
        "fn snapshot_status",
        "fn snapshot_layout_from_runtime",
        "fn runtime_layout_from_snapshot",
    ];
    for function in denied_fns {
        assert!(
            !server_source.contains(function),
            "packages/server/src/lib.rs must not define `{function}`; \
             the legacy snapshot pipeline has been replaced by \
             `SnapshotOrchestratorHandle` dispatch",
        );
    }

    // `ServerState` must not hold a `snapshot_runtime` field.
    assert!(
        !server_source.contains("snapshot_runtime: Arc<Mutex<SnapshotRuntime>>"),
        "packages/server/src/lib.rs must not hold `snapshot_runtime` \
         on `ServerState`; the snapshot plugin owns the dirty flag + \
         orchestrator via handles in the plugin state registry",
    );

    // Server MUST reach the orchestrator through the trait handle.
    assert!(
        server_source.contains("SnapshotOrchestratorHandle"),
        "packages/server/src/lib.rs must reference \
         `SnapshotOrchestratorHandle` so IPC handlers + restore hooks \
         delegate through the trait object instead of owning \
         persistence code directly",
    );
    assert!(
        server_source.contains("bmux_snapshot_runtime::"),
        "packages/server/src/lib.rs must import from \
         `bmux_snapshot_runtime` (the neutral primitive crate \
         hosting `SnapshotOrchestratorHandle` + `SnapshotDirtyFlag`)",
    );
}

/// Verify that the snapshot plugin + plugin-api crates exist,
/// export the expected file format + offline utility, and that the
/// plugin impl registers `SnapshotOrchestratorHandle` into the
/// plugin state registry so server can dispatch through it.
#[test]
fn snapshot_plugin_exists() {
    let api_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/snapshot-plugin-api");
    let plugin_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/snapshot-plugin");
    assert!(
        api_dir.join("Cargo.toml").exists(),
        "plugins/snapshot-plugin-api/Cargo.toml must exist",
    );
    assert!(
        plugin_dir.join("Cargo.toml").exists(),
        "plugins/snapshot-plugin/Cargo.toml must exist",
    );

    // Plugin-api crate owns the envelope format + offline utility.
    let api_envelope = include_str!("../../../plugins/snapshot-plugin-api/src/envelope.rs");
    assert!(
        api_envelope.contains("pub struct CombinedSnapshotEnvelope"),
        "plugins/snapshot-plugin-api/src/envelope.rs must export the \
         canonical `CombinedSnapshotEnvelope` struct",
    );
    assert!(
        api_envelope.contains("pub struct SectionV1"),
        "plugins/snapshot-plugin-api/src/envelope.rs must export the \
         canonical `SectionV1` section wrapper",
    );

    let api_offline = include_str!("../../../plugins/snapshot-plugin-api/src/offline_snapshot.rs");
    assert!(
        api_offline.contains("pub fn offline_kill_sessions"),
        "plugins/snapshot-plugin-api/src/offline_snapshot.rs must \
         export the `offline_kill_sessions` utility (CLI fallback \
         when the server is down)",
    );
    assert!(
        api_offline.contains("pub enum OfflineSessionKillTarget"),
        "plugins/snapshot-plugin-api/src/offline_snapshot.rs must \
         export `OfflineSessionKillTarget`",
    );

    // Plugin impl constructs the orchestrator + registers the handle.
    let plugin_lib = include_str!("../../../plugins/snapshot-plugin/src/lib.rs");
    assert!(
        plugin_lib.contains("pub struct SnapshotPlugin"),
        "plugins/snapshot-plugin/src/lib.rs must export a \
         `SnapshotPlugin` type implementing `RustPlugin`",
    );
    assert!(
        plugin_lib.contains("register::<SnapshotOrchestratorHandle>"),
        "plugins/snapshot-plugin/src/lib.rs must register a \
         `SnapshotOrchestratorHandle` into the plugin state registry \
         on `activate` so server + other plugins dispatch through it",
    );

    let orchestrator_src = include_str!("../../../plugins/snapshot-plugin/src/orchestrator.rs");
    assert!(
        orchestrator_src.contains("pub struct BmuxSnapshotOrchestrator"),
        "plugins/snapshot-plugin/src/orchestrator.rs must export the \
         concrete `BmuxSnapshotOrchestrator`",
    );
    assert!(
        orchestrator_src.contains("impl SnapshotOrchestrator for BmuxSnapshotOrchestrator"),
        "plugins/snapshot-plugin/src/orchestrator.rs must implement \
         `SnapshotOrchestrator` for the concrete orchestrator type",
    );

    // CLI bootstrap must register the config before plugin activation.
    let cli_bootstrap = include_str!("../src/runtime/bootstrap.rs");
    assert!(
        cli_bootstrap.contains("register_snapshot_plugin_config"),
        "packages/cli/src/runtime/bootstrap.rs must call \
         `register_snapshot_plugin_config` to install the \
         `SnapshotPluginConfig` before `activate_loaded_plugins` \
         so the snapshot plugin can read its path",
    );
    assert!(
        cli_bootstrap.contains("bmux-snapshot-v1.json"),
        "packages/cli/src/runtime/bootstrap.rs must reference the \
         versioned `bmux-snapshot-v1.json` filename so the new \
         combined-envelope format never silently overwrites a legacy \
         `server-snapshot-v2.json`",
    );
}

/// Verify that each foundational state plugin (clients, contexts,
/// sessions) implements `StatefulPlugin` so the snapshot plugin can
/// iterate them through a registered `StatefulPluginHandle`.
#[test]
fn state_plugins_implement_stateful_plugin() {
    let plugins = [
        (
            "plugins/clients-plugin",
            include_str!("../../../plugins/clients-plugin/src/lib.rs"),
            "impl StatefulPlugin for ClientsStatefulPlugin",
            "bmux.clients/follow-state",
        ),
        (
            "plugins/contexts-plugin",
            include_str!("../../../plugins/contexts-plugin/src/lib.rs"),
            "impl StatefulPlugin for ContextsStatefulPlugin",
            "bmux.contexts/context-state",
        ),
        (
            "plugins/sessions-plugin",
            include_str!("../../../plugins/sessions-plugin/src/lib.rs"),
            "impl StatefulPlugin for SessionsStatefulPlugin",
            "bmux.sessions/session-manager",
        ),
    ];
    for (path, src, impl_marker, id) in plugins {
        assert!(
            src.contains(impl_marker),
            "{path}/src/lib.rs must declare `{impl_marker}` so the \
             plugin participates in the shared `StatefulPluginRegistry`",
        );
        assert!(
            src.contains(id),
            "{path}/src/lib.rs must ground its participant at the \
             well-known id `{id}` so the snapshot orchestrator can \
             route restore payloads back to it",
        );
        assert!(
            src.contains("get_or_init_stateful_registry"),
            "{path}/src/lib.rs must call \
             `bmux_snapshot_runtime::get_or_init_stateful_registry` \
             to push its `StatefulPluginHandle` into the shared \
             registry on `activate`",
        );
    }
}

/// Verify that the pane-runtime plugin owns the `StatefulPlugin`
/// participant for concrete pane runtime state.
#[test]
fn pane_runtime_plugin_implements_stateful() {
    let pane_runtime_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/pane-runtime-plugin/src/snapshot.rs");
    assert!(
        pane_runtime_path.exists(),
        "plugins/pane-runtime-plugin/src/snapshot.rs must exist",
    );

    let module_src = include_str!("../../../plugins/pane-runtime-plugin/src/snapshot.rs");
    assert!(
        module_src.contains("pub struct PaneRuntimeStateful"),
        "plugins/pane-runtime-plugin/src/snapshot.rs must export \
         `PaneRuntimeStateful` — the plugin's participant in \
         the shared `StatefulPluginRegistry`",
    );
    assert!(
        module_src.contains("impl StatefulPlugin for PaneRuntimeStateful"),
        "plugins/pane-runtime-plugin/src/snapshot.rs must implement \
         `StatefulPlugin` for `PaneRuntimeStateful`",
    );
    assert!(
        module_src.contains("bmux.pane_runtime/pane-runtime"),
        "plugins/pane-runtime-plugin/src/snapshot.rs must ground its \
         participant at id `bmux.pane_runtime/pane-runtime` so the snapshot \
         orchestrator can route restore payloads back to it",
    );
    assert!(
        module_src.contains("pub struct PaneRuntimeSnapshotV1"),
        "plugins/pane-runtime-plugin/src/snapshot.rs must define the \
         `PaneRuntimeSnapshotV1` schema for the pane-runtime section",
    );

    let server_source = include_str!("../../server/src/lib.rs");
    let server_prod = production_section(server_source);
    let plugin_source = include_str!("../../../plugins/pane-runtime-plugin/src/lib.rs");
    assert!(
        !server_source.contains("mod pane_runtime_snapshot"),
        "packages/server/src/lib.rs must not declare a pane-runtime snapshot module",
    );
    assert!(
        !server_prod.contains("PaneRuntimeStateful::register"),
        "packages/server/src/lib.rs must not register the pane-runtime snapshot participant",
    );
    for forbidden in [
        "struct SessionRuntimeManager",
        "struct PaneRuntimeHandle",
        "struct OutputFanoutBuffer",
        "process_pane_exit_events",
    ] {
        assert!(
            !server_prod.contains(forbidden),
            "packages/server/src/lib.rs must not define concrete pane runtime item `{forbidden}`",
        );
    }
    assert!(
        plugin_source.contains("runtime::activate_pane_runtime"),
        "pane-runtime plugin activation must construct/register the concrete runtime",
    );
    let server_cargo = include_str!("../../server/Cargo.toml");
    for forbidden_dep in ["portable-pty", "vt100", "bmux_terminal_protocol"] {
        assert!(
            !server_cargo.contains(forbidden_dep),
            "packages/server/Cargo.toml must not depend on pane-runtime-only dependency `{forbidden_dep}`",
        );
    }
}

/// Core may advertise only domain-agnostic host services. Domain
/// services are provided by their owning plugins through BPDL surfaces,
/// not by `bmux.core` convenience interfaces.
#[test]
fn core_service_descriptors_have_no_legacy_domain_host_interfaces() {
    let plugin_kernel = include_str!("../src/runtime/plugin_kernel.rs");
    let plugin_kernel_prod = production_section(plugin_kernel);
    let denied = [
        "client-query/v1",
        "context-query/v1",
        "context-command/v1",
        "session-query/v1",
        "session-command/v1",
        "pane-query/v1",
        "pane-command/v1",
    ];

    for marker in denied {
        assert!(
            !plugin_kernel_prod.contains(marker),
            "packages/cli/src/runtime/plugin_kernel.rs must not advertise legacy domain host interface `{marker}`; use plugin-owned BPDL services instead",
        );
    }
}

/// `Request::NewSession` / `Request::KillSession` /
/// `Request::ListSessions` / `Request::ListPanes` are absent from
/// `bmux_ipc`. Session lifecycle and listing flow through
/// typed-dispatch services owned by the sessions-plugin and the
/// pane-runtime-plugin; streaming lifecycle updates flow through
/// generated `sessions-events` and the generic plugin-bus forwarder.
#[test]
fn session_lifecycle_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "    NewSession {",
        "    KillSession {",
        "    ListSessions,",
        "    ListPanes {",
        "ResponsePayload::SessionCreated {",
        "ResponsePayload::SessionKilled {",
        "ResponsePayload::SessionList {",
        "ResponsePayload::PaneList {",
        "pub struct SessionSummary",
        "Event::SessionCreated",
        "Event::SessionRemoved",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             session lifecycle + listing go through typed dispatch \
             (sessions-commands / pane-runtime-commands)",
        );
    }
}

/// Context selector/summary DTOs are owned by the neutral
/// `bmux_context_state` primitive crate and exposed publicly through
/// generated contexts-plugin API types, not core IPC.
#[test]
fn context_state_dtos_are_absent_from_ipc() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = ["pub enum ContextSelector", "pub struct ContextSummary"];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; context DTOs belong in bmux_context_state / contexts-plugin-api",
        );
    }
}

/// The 8 pane-mutation IPC variants are absent. Every pane mutation
/// is a typed `pane-runtime-commands` invocation whose handler lives
/// in the pane-runtime-plugin.
#[test]
fn pane_mutation_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied = [
        "    SplitPane {",
        "    LaunchPane {",
        "    FocusPane {",
        "    ResizePane {",
        "    ClosePane {",
        "    RestartPane {",
        "    ZoomPane {",
        "    PaneDirectInput {",
    ];
    for marker in denied {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             pane mutations go through `pane-runtime-commands` typed \
             dispatch (plugin owns permission checks + event emission)",
        );
    }
}

/// The 13 attach-family IPC variants are absent. Attach lifecycle
/// (grant/open/input/output/viewport/detach/policy/layout/snapshot/
/// pane-snapshot/pane-output-batch/pane-images) flows through typed
/// `attach-runtime-commands` and `attach-runtime-state` on the
/// pane-runtime-plugin.
#[test]
fn attach_ipc_variants_are_absent() {
    let ipc_source = include_str!("../../ipc/src/lib.rs");
    let denied_requests = [
        "    Attach {",
        "    AttachContext {",
        "    AttachOpen {",
        "    AttachInput {",
        "    AttachOutput {",
        "    AttachSetViewport {",
        "    AttachLayout {",
        "    AttachSnapshot {",
        "    AttachPaneSnapshot {",
        "    AttachPaneOutputBatch {",
        "    AttachPaneImages {",
        "    Detach,",
        "    SetClientAttachPolicy {",
    ];
    for marker in denied_requests {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}; \
             attach lifecycle lives in `attach-runtime-commands` / \
             `attach-runtime-state` on the pane-runtime-plugin",
        );
    }
    let denied_responses = [
        "ResponsePayload::Attached {",
        "ResponsePayload::AttachReady {",
        "ResponsePayload::AttachInputAccepted {",
        "ResponsePayload::AttachViewportSet {",
        "ResponsePayload::AttachOutput {",
        "ResponsePayload::AttachLayout {",
        "ResponsePayload::AttachSnapshot {",
        "ResponsePayload::AttachPaneSnapshot {",
        "ResponsePayload::AttachPaneOutputBatch {",
        "ResponsePayload::AttachPaneImages {",
        "ResponsePayload::ClientAttachPolicySet {",
        "ResponsePayload::Detached",
    ];
    for marker in denied_responses {
        assert!(
            !ipc_source.contains(marker),
            "packages/ipc/src/lib.rs must not reintroduce {marker}",
        );
    }
}

/// The pane-runtime-plugin is the sole owner of session/pane/attach
/// orchestration. Concretely:
///
/// - Its handlers own the runtime manager, attach-token manager, and
///   follow-state writers for every mutation and attach lifecycle
///   event. The server no longer has code paths that decode
///   `Request::{Split,Launch,Focus,Resize,Close,Restart,Zoom,PaneDirectInput,
///   Attach,AttachContext,AttachOpen,AttachInput,AttachOutput,
///   AttachSetViewport,AttachLayout,AttachSnapshot,AttachPaneSnapshot,
///   AttachPaneOutputBatch,AttachPaneImages,SetClientAttachPolicy,
///   Detach,NewSession,KillSession,ListSessions,ListPanes}`.
/// - The server's `ServiceInvokeContext` no longer carries a
///   per-invocation `selection` tuple — plugins read selection state
///   through `FollowStateHandle` when they need it.
#[test]
fn pane_runtime_plugin_owns_orchestration() {
    let server_source = include_str!("../../server/src/lib.rs");
    let server_prod = production_section(server_source);

    // ServiceInvokeContext::selection has been removed.
    assert!(
        !server_prod.contains("selection: Arc<AsyncMutex<(Option<SessionId>"),
        "packages/server/src/lib.rs must not reintroduce a per-invocation \
         `selection` tuple on `ServiceInvokeContext`; selection state \
         lives in `FollowStateHandle` owned by the clients-plugin",
    );

    // handle_connection no longer maintains a per-connection
    // ConnectionAttachPolicy — detach policy is on FollowState.
    assert!(
        !server_prod.contains("struct ConnectionAttachPolicy"),
        "packages/server/src/lib.rs must not reintroduce \
         `ConnectionAttachPolicy`; attach-detach policy is per-client \
         state owned by `FollowState` in the clients-plugin",
    );

    // Pane-runtime-plugin typed handlers exist for all former
    // IPC-handled operations.
    let attach_commands_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/pane-runtime-plugin/src/handlers/attach_commands.rs");
    assert!(
        attach_commands_path.exists(),
        "plugins/pane-runtime-plugin/src/handlers/attach_commands.rs must exist",
    );
    let attach_src = std::fs::read_to_string(&attach_commands_path)
        .expect("attach_commands.rs should be readable");
    for handler in [
        "pub fn attach_session(",
        "pub fn attach_context(",
        "pub fn attach_open(",
        "pub fn attach_input(",
        "pub fn attach_output(",
        "pub fn attach_set_viewport(",
        "pub fn set_client_attach_policy(",
        "pub fn detach(",
    ] {
        assert!(
            attach_src.contains(handler),
            "plugins/pane-runtime-plugin/src/handlers/attach_commands.rs \
             must define {handler} — the plugin owns attach orchestration",
        );
    }

    let pane_commands_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/pane-runtime-plugin/src/handlers/pane_commands.rs");
    assert!(
        pane_commands_path.exists(),
        "plugins/pane-runtime-plugin/src/handlers/pane_commands.rs must exist",
    );
    let pane_src =
        std::fs::read_to_string(&pane_commands_path).expect("pane_commands.rs should be readable");
    for handler in [
        "pub fn split_pane(",
        "pub fn launch_pane(",
        "pub fn focus_pane(",
        "pub fn resize_pane(",
        "pub fn close_pane(",
        "pub fn restart_pane(",
        "pub fn zoom_pane(",
        "pub fn pane_direct_input(",
        "pub fn new_session_with_runtime(",
        "pub fn kill_session_runtime(",
    ] {
        assert!(
            pane_src.contains(handler),
            "plugins/pane-runtime-plugin/src/handlers/pane_commands.rs \
             must define {handler} — the plugin owns pane/session mutation",
        );
    }
}

/// The BPDL contract and plugin handler must agree on the `resize-pane`
/// wire shape. A stale schema here silently breaks cross-process typed
/// dispatch because generated clients encode the BPDL shape.
#[test]
fn pane_runtime_resize_contract_uses_direction_and_cells() {
    let repo = repo_root();
    let bpdl = std::fs::read_to_string(
        repo.join("plugins/pane-runtime-plugin-api/bpdl/pane-runtime-plugin.bpdl"),
    )
    .expect("pane-runtime BPDL should be readable");
    let handler = std::fs::read_to_string(
        repo.join("plugins/pane-runtime-plugin/src/handlers/pane_commands.rs"),
    )
    .expect("pane-runtime pane_commands.rs should be readable");

    assert!(
        bpdl.contains("command resize-pane(")
            && bpdl.contains("direction: string")
            && bpdl.contains("cells: u16")
            && !bpdl.contains("delta_percent"),
        "pane-runtime BPDL resize-pane contract must use direction + cells",
    );
    assert!(
        handler.contains("struct ResizePaneArgs")
            && handler.contains("direction: String")
            && handler.contains("cells: u16")
            && !handler.contains("delta_percent"),
        "pane-runtime resize handler args must match BPDL direction + cells shape",
    );
}

// ── Capability-declaration guardrail ─────────────────────────────────────────
//
// Typed `call_service` / `call_service_raw` invocations are gated on
// the caller plugin's `required_capabilities` (see
// `packages/plugin/src/loader.rs::call_service_raw`). When a plugin
// acquires a new cross-plugin call site without updating its
// `plugin.toml`, the error materializes only at runtime as
// `CapabilityAccessDenied`. Before this guardrail landed, that drift
// shipped undetected (see plugin.toml fixes in this branch).
//
// The test walks every plugin's `src/**/*.rs`, extracts the first
// argument of each `call_service` / `call_service_raw` call (both
// string literals and `*_CAPABILITIES` const references), and
// asserts every captured capability is either declared in that
// plugin's `required_capabilities` or is self-provided through
// `provided_capabilities`. A small map of capability const names →
// literal capability strings is harvested from
// `plugins/*-plugin-api/src/lib.rs::capabilities` modules so the
// check doesn't drift when consts are renamed.

fn repo_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = packages/cli, so walk two levels up.
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root must exist two levels above packages/cli")
        .to_path_buf()
}

fn iter_plugin_dirs() -> Vec<std::path::PathBuf> {
    let plugins_dir = repo_root().join("plugins");
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&plugins_dir)
        .expect("plugins dir should be readable")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("plugin.toml").exists())
        .collect();
    dirs.sort();
    dirs
}

/// All crates under `plugins/`, including `*-plugin-api` ones that
/// don't carry a `plugin.toml`. Used for harvesting capability
/// constants that both host plugins and peer plugins reference.
fn iter_plugin_crate_dirs() -> Vec<std::path::PathBuf> {
    let plugins_dir = repo_root().join("plugins");
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&plugins_dir)
        .expect("plugins dir should be readable")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("Cargo.toml").exists())
        .collect();
    dirs.sort();
    dirs
}

/// Walk `plugin_dir/src` recursively and return every `.rs` file.
fn rust_source_files(plugin_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn visit(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(read) = std::fs::read_dir(root) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    let src = plugin_dir.join("src");
    if src.exists() {
        visit(&src, &mut out);
    }
    out
}

/// Build the map `plugin-api const name` → capability literal by
/// parsing `capabilities` modules in every `*-plugin-api` crate. The
/// regex pattern is:
///
/// `pub const NAME: CapabilityId = CapabilityId::from_static("bmux.X.Y");`
fn build_capability_const_map() -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for plugin_dir in iter_plugin_crate_dirs() {
        let lib_path = plugin_dir.join("src").join("lib.rs");
        if !lib_path.exists() {
            continue;
        }
        // Only `*-plugin-api` crates define capabilities; we scan all
        // plugin dirs but the pattern only matches where relevant.
        let Ok(source) = std::fs::read_to_string(&lib_path) else {
            continue;
        };
        for line in source.lines() {
            // Match either `pub const FOO: CapabilityId = CapabilityId::from_static("bmux.x.y");`
            // or `pub const FOO: HostScope = HostScope::from_static("bmux.x.y");`
            if let Some((const_name, literal)) = extract_capability_const(line) {
                map.insert(const_name.to_string(), literal.to_string());
            }
        }
    }
    map
}

fn extract_capability_const(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub const ")?;
    let (const_name, after_name) = rest.split_once(':')?;
    let const_name = const_name.trim();
    let literal_start = after_name.find("from_static(\"")?;
    let literal_rest = &after_name[literal_start + "from_static(\"".len()..];
    let literal_end = literal_rest.find('"')?;
    let literal = &literal_rest[..literal_end];
    if literal.starts_with("bmux.") {
        Some((const_name, literal))
    } else {
        None
    }
}

/// Scan `source` for capability arguments to `call_service` /
/// `call_service_raw` and return the set of resolved capability
/// strings. Supports both `"bmux.foo.bar"` string literals (via
/// `.as_str()` or bare) and `some_api::capabilities::NAME.as_str()`
/// const references (resolved through `const_map`).
fn extract_used_capabilities(
    source: &str,
    const_map: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    // Process each occurrence of `call_service(` / `call_service_raw(`
    // and grab the first argument token up to the first comma. This is
    // line-oriented to tolerate multi-line calls.
    let mut cursor = 0usize;
    let needles = ["call_service_raw(", "call_service(", "call_service::<"];
    while cursor < source.len() {
        let Some((start_rel, needle)) = needles
            .iter()
            .filter_map(|n| source[cursor..].find(n).map(|i| (i, *n)))
            .min_by_key(|(i, _)| *i)
        else {
            break;
        };
        let start = cursor + start_rel + needle.len();
        // For `call_service::<...>(` we need to skip past the
        // turbofish generic list to find the argument-list `(`. Match
        // nested `<>` depth so `::<Foo<Bar>, Baz>(` is handled too.
        let open_paren = if needle.ends_with('(') {
            start
        } else if needle.ends_with("::<") {
            let bytes = source.as_bytes();
            let mut depth = 1i32;
            let mut idx = start;
            let mut found = None;
            while idx < bytes.len() {
                match bytes[idx] {
                    b'<' => depth += 1,
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            // Next non-whitespace char should be `(`.
                            let mut j = idx + 1;
                            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                                j += 1;
                            }
                            if j < bytes.len() && bytes[j] == b'(' {
                                found = Some(j + 1);
                            }
                            break;
                        }
                    }
                    _ => {}
                }
                idx += 1;
            }
            let Some(pos) = found else {
                cursor = start;
                continue;
            };
            pos
        } else if let Some(offset) = source[start..].find('(') {
            start + offset + 1
        } else {
            cursor = start;
            continue;
        };
        // Find the first argument token. We want the capability
        // expression which ends at the first top-level comma.
        let Some(first_arg) = first_top_level_arg(&source[open_paren..]) else {
            cursor = open_paren;
            continue;
        };
        let arg = first_arg.trim().trim_start_matches('&');

        if let Some(literal) = extract_string_literal(arg) {
            if literal.starts_with("bmux.") {
                out.insert(literal.to_string());
            }
        } else if let Some(cap) = resolve_const_reference(arg, const_map) {
            out.insert(cap);
        }

        cursor = open_paren + first_arg.len();
    }
    out
}

fn first_top_level_arg(body: &str) -> Option<&str> {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b',' if depth == 0 => return Some(&body[..i]),
            _ => {}
        }
        if depth < 0 {
            return Some(&body[..i]);
        }
    }
    None
}

fn extract_string_literal(expr: &str) -> Option<&str> {
    let trimmed = expr.trim();
    let stripped = trimmed.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(&stripped[..end])
}

fn resolve_const_reference(
    expr: &str,
    const_map: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    // Strip common adornments: `.as_str()`, trailing whitespace.
    let mut core = expr.trim();
    for suffix in [".as_str()", ".into()", ".to_string()"] {
        if let Some(stripped) = core.strip_suffix(suffix) {
            core = stripped;
        }
    }
    core = core.trim();
    // Match trailing path segment against the const map.
    let last_segment = core.rsplit("::").next()?;
    // Defensive: avoid matching arbitrary identifiers that happen to
    // collide with a capability const name — require a `::capabilities::`
    // segment somewhere in the path.
    if !core.contains("::capabilities::") {
        return None;
    }
    const_map.get(last_segment).cloned()
}

/// Parse `required_capabilities` + `provided_capabilities` from a
/// plugin.toml. Returns a set of capability strings declared in the
/// manifest.
fn declared_capabilities(plugin_toml_path: &std::path::Path) -> std::collections::BTreeSet<String> {
    let source = std::fs::read_to_string(plugin_toml_path)
        .unwrap_or_else(|err| panic!("failed reading {}: {err}", plugin_toml_path.display()));
    let parsed: toml::Value = toml::from_str(&source).unwrap_or_else(|err| {
        panic!("failed parsing {}: {err}", plugin_toml_path.display());
    });
    let mut out = std::collections::BTreeSet::new();
    for key in ["required_capabilities", "provided_capabilities"] {
        if let Some(array) = parsed.get(key).and_then(|v| v.as_array()) {
            for item in array {
                if let Some(s) = item.as_str() {
                    out.insert(s.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn every_plugin_declares_capabilities_it_calls() {
    let const_map = build_capability_const_map();
    let mut failures: Vec<String> = Vec::new();

    for plugin_dir in iter_plugin_dirs() {
        let plugin_toml = plugin_dir.join("plugin.toml");
        let declared = declared_capabilities(&plugin_toml);

        let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for src in rust_source_files(&plugin_dir) {
            let Ok(source) = std::fs::read_to_string(&src) else {
                continue;
            };
            // Skip test-only modules so test scaffolding doesn't
            // influence capability requirements.
            let prod = production_section(&source);
            for cap in extract_used_capabilities(prod, &const_map) {
                used.insert(cap);
            }
        }

        let missing: Vec<String> = used
            .iter()
            .filter(|cap| !declared.contains(*cap))
            .cloned()
            .collect();
        if !missing.is_empty() {
            failures.push(format!(
                "plugin '{}' calls capabilities {missing:?} but declares only {declared:?} in plugin.toml",
                plugin_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?"),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "capability-declaration drift detected:\n  - {}\n\n\
         Fix: for each plugin listed above, add the missing capability literals to \
         `required_capabilities` in that plugin's plugin.toml.",
        failures.join("\n  - ")
    );
}

#[cfg(test)]
mod capability_guardrail_helpers {
    use super::{
        extract_capability_const, extract_string_literal, extract_used_capabilities,
        first_top_level_arg, resolve_const_reference,
    };

    #[test]
    fn extract_capability_const_parses_from_static_line() {
        let line = "    pub const SESSIONS_WRITE: CapabilityId = CapabilityId::from_static(\"bmux.sessions.write\");";
        let (name, literal) = extract_capability_const(line).expect("should parse");
        assert_eq!(name, "SESSIONS_WRITE");
        assert_eq!(literal, "bmux.sessions.write");
    }

    #[test]
    fn extract_capability_const_ignores_unrelated_const() {
        let line = "pub const EXIT_OK: i32 = 0;";
        assert!(extract_capability_const(line).is_none());
    }

    #[test]
    fn extract_string_literal_returns_inner() {
        assert_eq!(
            extract_string_literal("\"bmux.sessions.write\""),
            Some("bmux.sessions.write")
        );
        assert_eq!(extract_string_literal("foo"), None);
    }

    #[test]
    fn first_top_level_arg_splits_on_top_comma_only() {
        let body = "\"a\", b::c(x, y), 3)";
        assert_eq!(first_top_level_arg(body), Some("\"a\""));
    }

    #[test]
    fn resolve_const_reference_requires_capabilities_segment() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "SESSIONS_WRITE".to_string(),
            "bmux.sessions.write".to_string(),
        );
        assert_eq!(
            resolve_const_reference(
                "bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str()",
                &map,
            )
            .as_deref(),
            Some("bmux.sessions.write"),
        );
        assert!(resolve_const_reference("other::mod::SESSIONS_WRITE", &map).is_none());
    }

    #[test]
    fn extract_used_capabilities_picks_up_literal_and_const_forms() {
        let source = r#"
            caller.call_service_raw("bmux.sessions.write", kind, iface, op, payload);
            caller.call_service(bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(), k, i, o, p);
            let _ = call_service::<(), ()>("bmux.contexts.read", k, i, o, p);
        "#;
        let mut const_map = std::collections::BTreeMap::new();
        const_map.insert("CLIENTS_READ".to_string(), "bmux.clients.read".to_string());
        let out = extract_used_capabilities(source, &const_map);
        assert!(out.contains("bmux.sessions.write"), "got {out:?}");
        assert!(out.contains("bmux.clients.read"), "got {out:?}");
        assert!(out.contains("bmux.contexts.read"), "got {out:?}");
    }
}

// ── RuntimeAction domain-leak guardrail ──────────────────────────────
//
// AGENTS.md forbids domain leaks in core architecture crates.
// `packages/keybind/src/lib.rs::RuntimeAction` historically grew
// domain-named variants (NewWindow, FocusLeft, ZoomPane,
// WindowGoto1..9 etc.) that belong to plugins, not core. The
// migration in `docs/runtime-action-migration.md` tracks removing
// them in favor of `RuntimeAction::PluginCommand { plugin_id, command_name, args }`.
//
// Until that migration completes, this guardrail is a watchlist
// assertion: it allowlists the known-leaking variants so the test
// documents the remaining surface without artificially passing, and
// fails if a NEW domain-named variant is introduced (regression
// prevention). After the migration deletes the variants, the
// allowlist shrinks to empty and the assertion enforces the clean
// boundary.

/// Variant names allowed to appear in `RuntimeAction` *as of today*.
/// The set is ordered alphabetically to make drift reviews clear.
/// When removing a variant from `RuntimeAction`, also remove its
/// entry here.
const RUNTIME_ACTION_ALLOWLIST: &[&str] = &[
    // Core (not domain-scoped)
    "Quit",
    "Detach",
    "ShowHelp",
    "EnterScrollMode",
    "ExitScrollMode",
    "ScrollUpLine",
    "ScrollDownLine",
    "ScrollUpPage",
    "ScrollDownPage",
    "ScrollTop",
    "ScrollBottom",
    "BeginSelection",
    "MoveCursorLeft",
    "MoveCursorRight",
    "MoveCursorUp",
    "MoveCursorDown",
    "CopyScrollback",
    "ConfirmScrollback",
    "EnterMode",
    "ExitMode",
    "SwitchProfile",
    "PluginCommand",
    "ForwardToPane",
    // DOMAIN-leaking variants pending migration. Each should move to
    // a `plugin:bmux.<plugin>:<cmd>` invocation before deletion.
    // See `docs/runtime-action-migration.md` for the plan.
    "FocusNext",
    "FocusPrev",
    "FocusLeft",
    "FocusRight",
    "FocusUp",
    "FocusDown",
    "ToggleSplitDirection",
    "SplitFocusedVertical",
    "SplitFocusedHorizontal",
    "IncreaseSplit",
    "DecreaseSplit",
    "ResizeLeft",
    "ResizeRight",
    "ResizeUp",
    "ResizeDown",
    "RestartFocusedPane",
    "CloseFocusedPane",
    "ZoomPane",
    "EnterWindowMode",
    "WindowPrev",
    "WindowNext",
    "WindowGoto1",
    "WindowGoto2",
    "WindowGoto3",
    "WindowGoto4",
    "WindowGoto5",
    "WindowGoto6",
    "WindowGoto7",
    "WindowGoto8",
    "WindowGoto9",
    "WindowClose",
];

#[test]
fn runtime_action_variants_stay_on_allowlist() {
    let path = repo_root()
        .join("packages")
        .join("keybind")
        .join("src")
        .join("lib.rs");
    let source =
        std::fs::read_to_string(&path).expect("packages/keybind/src/lib.rs should be readable");

    // Find the `pub enum RuntimeAction { ... }` block and extract
    // variant identifiers. Matches any identifier that begins a line
    // inside the enum body up to the closing brace. We keep this
    // parser intentionally small: if it ever over-matches we'd rather
    // see a clear test failure than silently accept a leak.
    let enum_marker = "pub enum RuntimeAction {";
    let Some(start) = source.find(enum_marker) else {
        panic!(
            "could not locate `pub enum RuntimeAction` in {}",
            path.display()
        );
    };
    let body_start = start + enum_marker.len();
    let body_end = source[body_start..]
        .find("\n}\n")
        .expect("RuntimeAction enum body should close with `\\n}\\n`");
    let body = &source[body_start..body_start + body_end];

    let mut found = std::collections::BTreeSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        // Skip comments, attributes, and blank lines.
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        // Variant identifier is the leading word before `,`, `{`, or
        // `(`. Closing `}` lines end up with empty tokens; skip them.
        let terminator = trimmed
            .find(|c: char| c == ',' || c == '{' || c == '(' || c.is_whitespace())
            .unwrap_or(trimmed.len());
        let ident = &trimmed[..terminator];
        if ident.is_empty() || !ident.chars().next().is_some_and(char::is_uppercase) {
            continue;
        }
        found.insert(ident.to_string());
    }

    let allow: std::collections::BTreeSet<String> = RUNTIME_ACTION_ALLOWLIST
        .iter()
        .copied()
        .map(ToString::to_string)
        .collect();

    let unexpected: Vec<String> = found.difference(&allow).cloned().collect();
    let missing: Vec<String> = allow.difference(&found).cloned().collect();

    assert!(
        unexpected.is_empty(),
        "new RuntimeAction variants detected that are not on the allowlist: {unexpected:?}\n\
         Every new action should be routed through \
         `RuntimeAction::PluginCommand {{ plugin_id, command_name, args }}` \
         rather than a domain-named variant. If you are intentionally \
         extending core (not a plugin domain), add the variant to \
         `RUNTIME_ACTION_ALLOWLIST` with a comment explaining why."
    );
    assert!(
        missing.is_empty(),
        "RuntimeAction allowlist is stale — the following entries no longer exist in the enum \
         and should be removed from `RUNTIME_ACTION_ALLOWLIST`: {missing:?}"
    );
}

/// Enforce the boundary rule that core crates carry no
/// decoration-plugin-specific identifiers. The decoration plugin is
/// a domain plugin; everything plumbed between core and it must
/// route through generic primitives (scene-protocol wire types,
/// `AttachRenderExtension`, attach-layout-protocol state channel,
/// typed-client helpers generated into plugin-api interface modules).
///
/// The single permitted exception is the bundled-plugin static
/// registration macro in `plugin_runtime.rs`, which by design names
/// each bundled plugin's impl crate under a feature gate. That site
/// is explicitly excluded from the check.
#[test]
fn core_crates_contain_no_decoration_plugin_references() {
    use std::fs;

    // Core crates that must remain decoration-agnostic. Does NOT
    // include `bmux_attach_pipeline` because that crate transitively
    // imports `bmux_plugin` for the `AttachRenderExtension` trait
    // and render-extension registry — which is generic, not
    // decoration-specific.
    let core_crate_src_dirs = [
        "packages/server/src",
        "packages/session-state/src",
        "packages/context-state/src",
        "packages/pane-runtime-state/src",
        "packages/plugin/src",
        "packages/plugin-sdk/src",
        "packages/ipc/src",
        "packages/client/src",
    ];

    // Symbols that must never appear in core crate sources.
    let decoration_markers = [
        "bmux_decoration_plugin",
        "bmux_decoration_plugin_api",
        "bmux_decoration_plugin_renderer",
        "DecorationPlugin",
        "DecorationSceneCache",
        "push_decoration_pane_geometry",
        "forget_decoration_pane",
        "prime_decoration_scene_cache",
        "DECORATION_READY_TIMEOUT",
        "DECORATION_ANIMATION_HZ",
    ];

    let root = repo_root();
    for rel_dir in core_crate_src_dirs {
        let dir = root.join(rel_dir);
        if !dir.exists() {
            continue;
        }
        walk_rust_files(&dir, &mut |path| {
            // Skip lib.rs of scene-protocol-render-adjacent crates if they
            // legitimately reference decoration in a doc example; the
            // enumerated markers above are symbol-level, not doc-example
            // words, so this is defensive.
            let source = fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            for marker in decoration_markers {
                assert!(
                    !source.contains(marker),
                    "core crate file must not reference decoration-plugin symbol `{marker}`: {}",
                    path.display()
                );
            }
        });
    }

    // Confirm the only place in `bmux_cli/src` that may reference
    // `bmux_decoration_plugin*` is the bundled-plugin macro in
    // `plugin_runtime.rs`. Feature-gated `install()` calls in
    // `attach/runtime.rs` are also allowed. This is a positive
    // allowlist rather than a blanket ban because the CLI bundles
    // plugins by construction.
    let cli_src = root.join("packages/cli/src");
    walk_rust_files(&cli_src, &mut |path| {
        let source = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        if !source.contains("bmux_decoration_plugin") {
            return;
        }
        let allowed = path.ends_with("plugin_runtime.rs")
            || path.ends_with("runtime.rs")
            || path.ends_with("bootstrap.rs");
        assert!(
            allowed,
            "unexpected decoration-plugin reference in bmux_cli source: {}",
            path.display()
        );
    });
}

fn walk_rust_files(dir: &std::path::Path, visitor: &mut impl FnMut(&std::path::Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rust_files(&path, visitor);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            visitor(&path);
        }
    }
}

// ── `new_session_with_runtime` must not create a context ────────────
//
// Regression guard for the "multi-tab per keystroke" bug (see
// `docs/runtime-action-migration.md` et al.). Pre-fix, the
// pane-runtime handler for `new-session-with-runtime` created a
// shadow context with `Uuid::nil()` as the caller client id while
// `contexts-plugin::create_context_local` independently created
// another context for the real caller. The result: one `c`
// keypress produced two contexts, both named `tab-N`, sharing the
// same session.
//
// Context lifecycle is solely owned by `contexts-plugin`. The
// pane-runtime handler is strictly responsible for session +
// runtime allocation. This test enforces the boundary by scanning
// the `new_session_with_runtime` body for any method call that
// looks like context creation.

#[test]
fn pane_runtime_new_session_with_runtime_does_not_create_context() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("packages/")
        .parent()
        .expect("repo root")
        .join("plugins/pane-runtime-plugin/src/handlers/pane_commands.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    // Extract the body of `pub fn new_session_with_runtime(...)` from
    // its signature to the matching closing `}`. We search by brace
    // depth so the walker doesn't get confused by nested blocks.
    let signature = "pub fn new_session_with_runtime(";
    let sig_start = source.find(signature).unwrap_or_else(|| {
        panic!(
            "new_session_with_runtime signature not found in {}",
            path.display()
        )
    });
    let body_open = sig_start
        + source[sig_start..]
            .find('{')
            .expect("function body `{` should follow signature");
    let bytes = source.as_bytes();
    let mut depth: i32 = 0;
    let mut body_end = None;
    for (idx, &byte) in bytes[body_open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = Some(body_open + idx + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let body_end = body_end.expect("new_session_with_runtime body should close");
    let body = &source[body_open..body_end];

    // Disallowed: any call that looks like writing a new context via
    // the ContextStateWriter trait. If this test trips, move the
    // context creation back to `contexts-plugin::create_context_local`
    // (or plumb it through a typed service dispatch from the plugin
    // that owns context lifecycle) — do NOT re-introduce a shadow
    // create with `Uuid::nil()` here.
    let denied_patterns = [
        "context_handle.0.create(",
        "ContextStateHandle",
        "ContextStateWriter",
        ".create(caller_client_id",
    ];
    for pattern in denied_patterns {
        assert!(
            !body.contains(pattern),
            "`new_session_with_runtime` must not call context mutation \
             primitives (`{pattern}`). Context lifecycle is owned by \
             contexts-plugin's `create_context_local`; creating a \
             context here produces shadow tabs (two contexts per \
             keystroke) — see the earlier regression in the \
             multi-tab bug investigation.",
        );
    }
}
