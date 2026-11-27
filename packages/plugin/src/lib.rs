#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::result_large_err)]

//! Host-side plugin infrastructure for bmux.
//!
//! This crate provides the registry, loader, discovery, and validation
//! machinery that the bmux runtime uses to manage plugins.
//!
//! **Plugin authors** should depend on [`bmux_plugin_sdk`] instead — it
//! provides the [`Plugin`](bmux_plugin_sdk::author::Plugin) trait, context
//! types, macros, and everything needed to write a plugin without pulling
//! in the host-side dependencies.

/// Neutral host-authenticated stream origin returned by the bound host API.
pub type StreamOrigin = bmux_ipc::StreamProvenance;

pub mod action_dispatch;
mod attach_alerts;
#[cfg(feature = "tui")]
mod attach_companion;
mod attach_presentation_input;
#[cfg(feature = "tui")]
pub mod component_input;
#[cfg(feature = "tui")]
pub mod component_render;
#[cfg(feature = "tui")]
pub mod component_viewport;
mod declaration;
pub mod dependency_version;
mod discovery;
mod event_bus;
mod extension_points;
pub mod filesystem;
mod host_builtins;
mod host_runtime;
pub mod layout;
mod loader;
mod manifest;
mod plugin_state;
pub mod prompt;
mod registry;
pub mod render;
#[cfg(feature = "tui")]
pub mod render_paint;
mod service_location;
mod startup_gate;
mod static_definition_registry;
pub mod stream_admission;
#[cfg(feature = "tui")]
pub mod surface;
pub mod test_support;
mod text_transform;
mod typed_dispatch_bridge;
mod typed_service_caller;

pub use attach_alerts::{
    AttachAlert, AttachAlerts, AttachBellPolicy, AttachNotification, attach_alerts,
};
#[cfg(feature = "tui")]
pub use attach_companion::{
    AttachCompanion, AttachCompanionCallback, AttachCompanionFactory, AttachCompanionRegistry,
    AttachPresentationResources,
};
pub use attach_presentation_input::{
    AttachPresentationCommandHandler, AttachPresentationCommittedHandler,
    AttachPresentationFocusChangedHandler, AttachPresentationFocusLostHandler,
    AttachPresentationInputDispatch, AttachPresentationInputHandler,
    AttachPresentationInputRegistry, AttachPresentationPasteHandler,
};
pub use bmux_plugin_runtime::{
    ConcurrencyGate, EffectiveConcurrencyPolicy, PluginConcurrencyConfig,
};
pub use bmux_plugin_sdk::PluginEventKind;
pub use declaration::{
    NativePlugin, PluginDeclaration, PluginDependency, PluginEntrypoint, PluginExecutionClass,
    PluginId, PluginLifecycle, PluginOwnedPath,
};
pub use discovery::{
    DEFAULT_PLUGIN_MANIFEST_FILE, PluginDiscoveryReport, discover_plugin_candidates_in_roots,
    discover_plugin_manifests, discover_plugin_manifests_in_roots, discover_registered_plugins,
    discover_registered_plugins_in_roots,
};
pub use event_bus::{
    ActivationEventBus, DEFAULT_EVENT_BUS_CAPACITY, DeliveryMode, EventBus, EventBusBytesError,
    EventBusChannelOptions, EventBusError, EventBusResult, JsonPluginEvent, JsonProjectionPolicy,
    activation_binder as event_bus_activation_binder, activation_event_bus,
    remote::subscribe_payloads as subscribe_event_payloads, take_activation_event_bus,
};
pub use extension_points::{Extension, ExtensionInvoker, ExtensionPoints, ExtensionSet};
pub use host_builtins::InvocationHostBuiltins;
pub use host_runtime::{
    HostRuntimeApi, InvocationKernelDispatch, InvocationKernelPort, ServiceCaller,
};
pub use loader::scoped::{
    ActivationBinder, CallbackBinding, CallbackEntryLease, CallbackReentry, DefinitionArtifact,
    LifecycleCallbackDispatch, LifecycleCallbackFactory, LoadedExecution, LoaderCallbackHost,
    OperationSignal, VerifiedArtifact, WeakCallbackEntryLease,
};
pub use loader::{
    LoadedPlugin, NativePluginLoader, executable_sha256, load_process_plugin, retain_executable,
    retain_executable_snapshot,
};
pub use manifest::{
    PluginManifest, PluginManifestCompatibility, PluginManifestExtension,
    PluginManifestKeybindings, PluginRuntime,
};
pub use plugin_state::{
    ActivationRegistrationPort, ActivationRegistrationRegistry, ActivationStateRegistry,
    AdmittedRegistration, PluginStateRegistry, RegistrationError, RegistrationSnapshot,
    RegistrationSubscription, RegistrationUpdate, RegistrationUpdateBatch,
    activation_binder as state_registry_activation_binder, activation_state_registry,
    take_activation_state_registry,
};
pub use registry::{
    CapabilityProvider, PluginCandidate, PluginCandidateInventory, PluginCandidatePreference,
    PluginCandidateSource, PluginCompatibilityReport, PluginRegistry, PluginSelectionRequest,
    RegisteredPlugin, SelectedPluginCandidate, ServiceProvider,
};
pub use render::{
    AttachInputCodec, AttachInputEndpoint, AttachInputEvent, AttachInputHook,
    AttachInputHookFilter, AttachInputKeyModes, AttachInputModifiers, AttachInputMouseModes,
    AttachInputMouseTracking, AttachInputPaneContext, AttachInputPaneModes, AttachInputResult,
    AttachInputServiceInvocation, AttachInputTarget, AttachRenderExtension, AttachVisualAdapter,
    AttachVisualAdapterOutput, AttachVisualAdapterRequest, AttachVisualCellRef,
    AttachVisualFrameView, AttachVisualProjectionResult, AttachVisualProjectionUpdate,
    AttachVisualSurfaceView, BorderGlyphs, ExtensionRect, RenderCell, RenderColor, RenderDamage,
    RenderExtensionContext, RenderExtensionLayer, RenderExtensionRegistry, RenderLayerItem,
    RenderLayerScene, RenderLayerSceneBuilder, RenderNamedColor, RenderOp, RenderSceneItem,
    RenderSceneItemKey, RenderStyle, RenderSurfaceChrome, RenderSurfaceRole, RenderTextSpan,
    RenderUnderCell, TerminalGraphicFill, TerminalGraphicOverlay, TerminalRenderCapabilities,
    TerminalRgba, VisualAdapterRegistry, clip_render_text_run_to_rect,
    render_char_display_width_u16, render_single_display_cell_char, render_text_width_u16,
};
pub use service_location::{
    ServiceEndpoint, ServiceEndpointError, ServiceLocation, ServiceLocationMap,
    global_service_locations,
};
pub use startup_gate::{StartupReadyGate, StartupReadyGateRegistry};
pub use text_transform::{
    PaneTextAction, PaneTextMatch, PaneTextPattern, PaneTextRule, PaneTextRules,
};
pub use typed_dispatch_bridge::{
    ASYNC_SERVICE_ROUTE_V1, AsyncCommandRouteGuard, AsyncServiceClient, AsyncServiceRequest,
    ServiceCallerDispatchClient, block_on_typed_dispatch, capture_async_command_route,
    enter_async_command_route,
};
pub use typed_service_caller::TypedServiceCaller;
