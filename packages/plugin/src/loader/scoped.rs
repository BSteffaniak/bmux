//! Scoped definition execution. v1 and process adapters are deliberately separate.
//!
//! Operations remain owned by the instance until explicit acknowledgement. A
//! dropped observer does not drop an author future or unload executable code.
//! Static operations and native guests negotiating `CAP_HOST_WAKE_V1` are driven
//! by the observer's real waker; only legacy native guests without that bit are
//! repolled periodically, as their ABI requires.
#[cfg(all(test, unix))]
#[path = "scoped/activation_binder_tests.rs"]
mod activation_binder_tests;
#[cfg(all(test, unix))]
#[path = "scoped/activation_parity_tests.rs"]
mod activation_parity_tests;
#[path = "scoped/activation_ports.rs"]
mod activation_ports;
#[path = "scoped/callbacks.rs"]
pub mod callbacks;
#[cfg(all(test, unix))]
#[path = "scoped/host_wake_tests.rs"]
mod host_wake_tests;
pub use callbacks::{
    CallbackBinding, CallbackEntryLease, CallbackReentry, LifecycleCallbackDispatch,
    LifecycleCallbackFactory, LoaderCallbackHost, WeakCallbackEntryLease,
};
#[path = "scoped/ports.rs"]
mod ports;
#[path = "scoped/retained.rs"]
mod retained;
#[path = "scoped/wake.rs"]
mod wake;
pub use wake::OperationSignal;

/// Trusted host factory invoked with this execution's actual activation.
pub type RetainedHostFactory = dyn Fn(&ActivationScope) -> Result<Arc<dyn bmux_plugin_sdk::abi::retained::RetainedHostDispatch>>
    + Send
    + Sync;

/// Trusted host binder invoked with this execution's actual activation scope.
///
/// It runs before the component's `activate` hook. It may mint activation-owned
/// in-process values from that scope (and retain their host-side halves) and
/// offers them to the component through `PluginLifecycleContext::capabilities`.
/// The loader holds no capability itself: it only hands the binder the real
/// scope, so everything the binder mints is revoked when that scope closes.
pub type ActivationBinder = dyn Fn(&ActivationScope, &bmux_plugin_sdk::definition::ActivationCapabilities) -> Result<()>
    + Send
    + Sync;

use bmux_plugin_runtime::{
    ActivationScope, AdmissionGuard, InstanceScope, InvocationScope, LifecycleInvocationKind,
    PluginConcurrencyConfig, PluginRuntimeScopeId,
};
use bmux_plugin_sdk::abi::{self, NativeOperation, NativeOutcome, NativePluginDescriptor};
use bmux_plugin_sdk::definition::{
    ActivationId, ComponentRole, LoadedInstanceId, PluginCallContext, PluginCreateContext,
    PluginDefinition, PluginFuture, PluginInstance, PluginLifecycleContext, PluginMetadata,
    RuntimeScopeId,
};
use bmux_plugin_sdk::{
    NativeLifecycleContext, NativeStreamingServiceContext, PluginError, PluginInvocationId, Result,
    ServiceCancellation, ServiceEvent, ServiceEventSink, ServiceEventSinkHandle,
};
use libloading::Library;
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    output
}

fn failure(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}
/// Bounded teardown drain of an activation: stragglers are reported by owner
/// and abandoned (retained, never unloaded) instead of being awaited forever.
async fn drain_activation(
    activation: &bmux_plugin_runtime::ActivationScope,
    deadline: tokio::time::Instant,
) -> Result<()> {
    activation.drain_until(deadline).await.map_err(scope_error)
}
fn scope_error(error: bmux_plugin_runtime::ScopeError) -> PluginError {
    match error {
        bmux_plugin_runtime::ScopeError::Cleanup(details) => {
            failure(format!("scope cleanup failed: {details}"))
        }
        error => failure(error.to_string()),
    }
}
fn start_status(code: i32) -> Result<()> {
    match code {
        abi::OK | abi::PENDING => Ok(()),
        _ => status(code),
    }
}

fn status(status: i32) -> Result<()> {
    if status == abi::OK {
        Ok(())
    } else {
        Err(failure(format!("native v2 status {status}")))
    }
}

/// One loader owner's retained observation of bundled executable bytes.
/// The cache is neither keyed by a mutable path nor shared process-globally.
#[derive(Debug, Default)]
pub(super) struct BundledExecutableCache {
    observed: Mutex<Option<Arc<BundledExecutableArtifact>>>,
}
#[derive(Debug)]
struct BundledExecutableArtifact {
    // Private anonymous snapshot: no caller can mutate these retained bytes.
    _snapshot: std::fs::File,
    identity: String,
    executable_parent: std::path::PathBuf,
}
impl BundledExecutableCache {
    fn observe(&self) -> Result<Arc<BundledExecutableArtifact>> {
        self.observe_with(|| BundledExecutableArtifact::capture(&std::env::current_exe()?))
    }

    fn observe_with(
        &self,
        capture: impl FnOnce() -> Result<BundledExecutableArtifact>,
    ) -> Result<Arc<BundledExecutableArtifact>> {
        let mut observed = self
            .observed
            .lock()
            .map_err(|_| failure("bundled executable observation poisoned"))?;
        if let Some(artifact) = &*observed {
            return Ok(artifact.clone());
        }
        let artifact = Arc::new(capture()?);
        *observed = Some(artifact.clone());
        drop(observed);
        Ok(artifact)
    }
}
impl BundledExecutableArtifact {
    fn capture(path: &Path) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let executable_parent = path
            .canonicalize()?
            .parent()
            .map(std::path::PathBuf::from)
            .ok_or_else(|| failure("canonical host executable has no parent"))?;
        let mut input = std::fs::File::open(path)?;
        let before = input.metadata()?;
        if !before.is_file() {
            return Err(failure("bundled executable is not a regular file"));
        }
        let mut snapshot = tempfile::tempfile()?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 65536];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            snapshot.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
        }
        let after = input.metadata()?;
        if before.len() != after.len()
            || before.modified()? != after.modified()?
            || snapshot.metadata()?.len() != after.len()
        {
            return Err(failure("bundled executable changed during observation"));
        }
        let mut permissions = snapshot.metadata()?.permissions();
        permissions.set_readonly(true);
        snapshot.set_permissions(permissions)?;
        Ok(Self {
            _snapshot: snapshot,
            identity: hex_digest(&hash.finalize()),
            executable_parent,
        })
    }
}

/// Loader-issued provenance for one created native instance, not policy approval.
///
/// The digest covers only the retained private main-library copy, not its
/// dependencies, auxiliary executables, or a whole package. No wire/payload
/// representation can construct this receipt.
#[derive(Debug)]
pub struct VerifiedArtifact {
    sha256: String,
    metadata: PluginMetadata,
    role: ComponentRole,
    native_abi_version: u32,
    native_capabilities: u64,
    runtime_scope: RuntimeScopeId,
    instance: LoadedInstanceId,
}
impl VerifiedArtifact {
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    #[must_use]
    pub const fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }
    #[must_use]
    pub const fn role(&self) -> ComponentRole {
        self.role
    }
    #[must_use]
    pub const fn native_abi_version(&self) -> u32 {
        self.native_abi_version
    }
    /// Validated descriptor capabilities, not a grant of invocation authority.
    #[must_use]
    pub const fn native_capabilities(&self) -> u64 {
        self.native_capabilities
    }
    #[must_use]
    pub const fn runtime_scope(&self) -> RuntimeScopeId {
        self.runtime_scope
    }
    #[must_use]
    pub const fn instance(&self) -> LoadedInstanceId {
        self.instance
    }
}

/// An inert artifact, shared across hosts; it never caches implementation state.
#[derive(Clone)]
pub struct DefinitionArtifact {
    metadata: PluginMetadata,
    backend: ArtifactBackend,
    bundled_executable: Option<Arc<BundledExecutableArtifact>>,
    lifecycle_selection: Option<Arc<LifecycleSelection>>,
    verified_selection: Option<(String, ComponentRole)>,
}
struct LifecycleSelection {
    registered: crate::RegisteredPlugin,
    base: std::result::Result<std::path::PathBuf, String>,
}
#[derive(Clone)]
enum ArtifactBackend {
    Static(PluginDefinition),
    Dynamic(Arc<DynamicArtifact>),
}
struct DynamicArtifact {
    // Never unloaded: once plug-rt is installed the host holds plugin wakers,
    // futures and drop functions, so the library must outlive every reference.
    _library: std::mem::ManuallyDrop<Library>,
    artifact_copy: Option<tempfile::TempDir>,
    descriptor: &'static NativePluginDescriptor,
    // Guests must negotiate `CAP_HOST_WAKE_V1`: operations are driven by real
    // wakers only, never by host repolling.
    read_wake: abi::NativeReadWakeV1,
    // The plug-rt table installed into this library. `None` only for this
    // test process's own in-process descriptors, which share the host's
    // runtime statics.
    plug_rt: Option<&'static bmux_plugin_runtime::rt::host::PlugRtHost>,
    // Declared by plugins that use host facilities plug-rt does not proxy.
    bundled_only: Option<String>,
    // Copied once from this retained library's validated v3 declaration export.
    // Never substitute discovery metadata or another implementation's contracts.
    service_contracts: Vec<bmux_plugin_sdk::definition::DeclaredServiceContract>,
}
impl DefinitionArtifact {
    #[must_use]
    pub fn from_definition(definition: PluginDefinition) -> Self {
        Self {
            metadata: definition.metadata.clone(),
            backend: ArtifactBackend::Static(definition),
            bundled_executable: None,
            lifecycle_selection: None,
            verified_selection: None,
        }
    }
    /// Load trusted native code and inspect its inert, explicitly selected v2 ABI.
    /// No fallback to v1 occurs after selection of this API.
    ///
    /// # Safety
    /// The library must implement the descriptor ABI, including pointer validity,
    /// thread safety and panic containment. Native code is not sandboxed.
    ///
    /// # Errors
    /// Loading, ABI validation, or metadata decoding fails.
    pub unsafe fn load(path: &Path) -> Result<Self> {
        let library = unsafe { Library::new(path) }.map_err(|e| failure(e.to_string()))?;
        let pointer = {
            let entry: libloading::Symbol<
                '_,
                unsafe extern "C" fn() -> *const NativePluginDescriptor,
            > = unsafe { library.get(abi::ENTRY_SYMBOL.as_bytes()) }
                .map_err(|e| failure(e.to_string()))?;
            unsafe { entry() }
        };
        let install = unsafe {
            library.get::<bmux_plugin_runtime::rt::abi::PlugRtInstallV1>(
                bmux_plugin_runtime::rt::abi::INSTALL_SYMBOL.as_bytes(),
            )
        }
        .map(|symbol| *symbol)
        .ok();
        let mut artifact = unsafe { Self::from_descriptor(library, pointer) }?;
        let plugin_id = artifact.metadata.plugin_id.clone();
        let ArtifactBackend::Dynamic(owner) = &mut artifact.backend else {
            return Err(failure("native load produced a static artifact"));
        };
        refuse_bundled_only(&plugin_id, owner.bundled_only.as_deref())?;
        let install = install.ok_or_else(|| PluginError::DynamicRuntimeUnavailable {
            plugin_id: plugin_id.clone(),
            details: format!(
                "library does not export {}; rebuild it with the plugin packager",
                bmux_plugin_runtime::rt::abi::INSTALL_SYMBOL
            ),
        })?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            PluginError::DynamicRuntimeUnavailable {
                plugin_id: plugin_id.clone(),
                details: format!("dynamic plugins load inside the host runtime: {error}"),
            }
        })?;
        let host = bmux_plugin_runtime::rt::host::PlugRtHost::new(runtime, plugin_id.clone());
        // SAFETY: `install` is this library's export and the library is never
        // unloaded (`DynamicArtifact::_library` is never dropped).
        unsafe { host.install(install) }
            .map_err(|details| PluginError::DynamicRuntimeUnavailable { plugin_id, details })?;
        // Asynchronous host calls resolve the plugin's invocation ports.
        host.set_port_services(Arc::new(ports::AsyncHostRoute));
        Arc::get_mut(owner)
            .ok_or_else(|| failure("artifact unexpectedly shared during loading"))?
            .plug_rt = Some(host);
        Ok(artifact)
    }

    /// Adopt this test process's own in-process adapter descriptor as a
    /// dynamic artifact, exercising the exact native ABI path without a
    /// separately built library. The process handle owns the descriptor.
    ///
    /// # Safety
    /// `pointer` must be a static descriptor implemented in this process.
    #[cfg(all(test, unix))]
    pub(super) unsafe fn from_process_descriptor(
        pointer: *const NativePluginDescriptor,
    ) -> Result<Self> {
        let library = Library::from(libloading::os::unix::Library::this());
        unsafe { Self::from_descriptor(library, pointer) }
    }

    unsafe fn from_descriptor(
        library: Library,
        pointer: *const NativePluginDescriptor,
    ) -> Result<Self> {
        if pointer.is_null()
            || !(pointer as usize).is_multiple_of(std::mem::align_of::<NativePluginDescriptor>())
        {
            return Err(failure("invalid native v2 descriptor pointer"));
        }
        // Read only the fixed header until size/version have been validated.
        let size = unsafe { std::ptr::addr_of!((*pointer).size).read() };
        let version = unsafe { std::ptr::addr_of!((*pointer).version).read() };
        if !matches!(version, abi::ABI_VERSION_V2 | abi::ABI_VERSION)
            || (size as usize) < std::mem::size_of::<NativePluginDescriptor>()
        {
            return Err(failure("unsupported native descriptor size/version"));
        }
        // The extended reference never escapes its Library owner. Instances pin
        // the Arc owner, and failed cleanup retains it rather than guessing safety.
        let descriptor = unsafe { abi::validate_descriptor(pointer) }
            .map_err(|code| failure(format!("unsupported native descriptor: {code}")))?;
        let bytes = read_bounded(|out, len| unsafe {
            (descriptor.metadata)(out.as_mut_ptr(), out.len(), len)
        })?
        .ok_or_else(|| failure("metadata discovery cannot be pending"))?;
        let metadata = serde_json::from_slice(&bytes).map_err(|e| failure(e.to_string()))?;
        let read_wake = unsafe { abi::validate_descriptor_host_wake(pointer) }
            .map_err(|code| failure(format!("unsupported native host-wake descriptor: {code}")))?
            .map(|extension| extension.read_wake)
            .ok_or_else(|| {
                failure(
                    "native guest lacks CAP_HOST_WAKE_V1; legacy repolled guests are unsupported",
                )
            })?;
        let mut bundled_only = None;
        let service_contracts = if descriptor.version == abi::ABI_VERSION {
            let extension = unsafe { abi::validate_descriptor_v3(pointer) }
                .map_err(|code| failure(format!("unsupported native v3 descriptor: {code}")))?;
            let declarations = read_bounded(|out, len| unsafe {
                (extension.declarations)(out.as_mut_ptr(), out.len(), len)
            })?
            .ok_or_else(|| failure("declaration discovery cannot be pending"))?;
            let declarations: bmux_plugin_sdk::definition::PluginDeclarations =
                serde_json::from_slice(&declarations)
                    .map_err(|error| failure(error.to_string()))?;
            validate_native_service_contracts(&declarations.modeled_service_contracts)?;
            bundled_only = declarations.bundled_only;
            declarations.modeled_service_contracts
        } else {
            Vec::new()
        };
        Ok(Self {
            metadata,
            backend: ArtifactBackend::Dynamic(Arc::new(DynamicArtifact {
                _library: std::mem::ManuallyDrop::new(library),
                artifact_copy: None,
                descriptor,
                read_wake,
                plug_rt: None,
                bundled_only,
                service_contracts,
            })),
            bundled_executable: None,
            lifecycle_selection: None,
            verified_selection: None,
        })
    }
    /// Load exactly the selected inventory candidate. Dynamic bytes are copied
    /// privately, hashed, pin-checked and then loaded from that retained copy.
    /// Embedded declaration verification happens before scoped creation.
    ///
    /// # Safety
    /// The selected artifact is trusted native code; loading is not a sandbox.
    ///
    /// # Errors
    /// Artifact I/O, identity, declaration, or ABI validation fails.
    pub unsafe fn load_selected(selected: &crate::SelectedPluginCandidate) -> Result<Self> {
        unsafe { Self::load_selected_with_owner(selected, &BundledExecutableCache::default()) }
    }

    pub(super) unsafe fn load_selected_with_owner(
        selected: &crate::SelectedPluginCandidate,
        owner: &BundledExecutableCache,
    ) -> Result<Self> {
        unsafe { Self::load_selected_with_definition(selected, owner, None) }
    }

    pub(super) unsafe fn load_selected_with_definition(
        selected: &crate::SelectedPluginCandidate,
        owner: &BundledExecutableCache,
        definition: Option<PluginDefinition>,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let plugin = &selected.candidate().plugin;
        if definition.is_some() && !plugin.bundled_static {
            return Err(failure(
                "definition override requires a selected bundled artifact",
            ));
        }
        let mut observed_identity = String::new();
        let mut artifact = if plugin.bundled_static {
            let executable = owner.observe()?;
            observed_identity.push_str(&executable.identity);
            let definition = match definition {
                Some(definition) => definition,
                None => crate::static_definition_registry::static_definition(
                    plugin.declaration.id.as_str(),
                )?
                .ok_or_else(|| failure("selected bundled plugin has no unified definition"))?,
            };
            let mut artifact = Self::from_definition(definition);
            artifact.bundled_executable = Some(executable);
            artifact
        } else {
            let base = plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let source = plugin
                .manifest
                .resolve_entry_path(base)
                .ok_or_else(|| failure("selected native artifact has no entry path"))?;
            let mut input = std::fs::File::open(&source)?;
            if !input.metadata()?.is_file() {
                return Err(failure("selected artifact is not a regular file"));
            }
            let directory = tempfile::tempdir()?;
            let path = directory.path().join(
                source
                    .file_name()
                    .ok_or_else(|| failure("artifact filename missing"))?,
            );
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0; 65536];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
                hash.update(&buffer[..count]);
            }
            output.sync_all()?;
            drop(output);
            observed_identity.push_str(&hex_digest(&hash.finalize()));
            selected.validate_artifact_identity(&observed_identity)?;
            let mut permissions = std::fs::metadata(&path)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&path, permissions)?;
            let mut artifact = unsafe { Self::load(&path) }?;
            if let ArtifactBackend::Dynamic(owner) = &mut artifact.backend {
                Arc::get_mut(owner)
                    .ok_or_else(|| failure("artifact unexpectedly shared during loading"))?
                    .artifact_copy = Some(directory);
            }
            artifact
        };
        selected.validate_embedded_metadata(artifact.metadata(), &observed_identity)?;
        // Bare loads and static definitions have no private loaded-library proof.
        // Mint selection evidence only after all selected metadata checks pass.
        if matches!(&artifact.backend, ArtifactBackend::Dynamic(owner) if owner.artifact_copy.is_some())
        {
            artifact.verified_selection = Some((observed_identity, selected.role()));
        }
        let base = artifact.bundled_executable.as_ref().map_or_else(
            || {
                plugin
                    .manifest_path
                    .parent()
                    .filter(|path| path.is_absolute())
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| {
                        "auxiliary package manifest has no absolute authoritative root".into()
                    })
            },
            |executable| Ok(executable.executable_parent.clone()),
        );
        artifact.lifecycle_selection = Some(Arc::new(LifecycleSelection {
            registered: plugin.clone(),
            base,
        }));
        Ok(artifact)
    }

    fn verified_for_context(&self, context: &PluginCreateContext) -> Option<VerifiedArtifact> {
        let (sha256, selected_role) = self.verified_selection.as_ref()?;
        let ArtifactBackend::Dynamic(owner) = &self.backend else {
            return None;
        };
        owner.artifact_copy.as_ref()?;
        Some(VerifiedArtifact {
            sha256: sha256.clone(),
            metadata: self.metadata.clone(),
            role: *selected_role,
            native_abi_version: owner.descriptor.version,
            native_capabilities: owner.descriptor.capabilities,
            runtime_scope: context.runtime_scope,
            instance: context.instance,
        })
    }

    #[must_use]
    pub const fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    /// Allocate fresh implementation state. Capabilities are trusted host data;
    /// metadata is not proof of permission or caller provenance.
    ///
    /// # Errors
    /// Scope allocation, native creation, or author construction fails.
    pub fn create(
        &self,
        runtime: PluginRuntimeScopeId,
        role: ComponentRole,
        available_capabilities: &[String],
    ) -> Result<Arc<LoadedExecution>> {
        self.create_for_command(runtime, role, available_capabilities, None)
    }

    pub(super) fn create_for_command(
        &self,
        runtime: PluginRuntimeScopeId,
        role: ComponentRole,
        available_capabilities: &[String],
        cli_command: Option<&str>,
    ) -> Result<Arc<LoadedExecution>> {
        if self
            .verified_selection
            .as_ref()
            .is_some_and(|(_, selected_role)| *selected_role != role)
            || !self.metadata.roles.contains(&role)
            || self
                .metadata
                .required_capabilities
                .iter()
                .any(|capability| !available_capabilities.contains(capability))
        {
            return Err(failure("component role or required capability unavailable"));
        }
        let scope = InstanceScope::new(runtime);
        let background = bmux_plugin_runtime::background::BackgroundTasks::new();
        // Attribute every holder of this instance to its plugin in diagnostics.
        scope
            .set_label(self.metadata.plugin_id.as_str())
            .map_err(scope_error)?;
        let context = PluginCreateContext {
            cli_command: cli_command.map(str::to_owned),
            runtime_scope: RuntimeScopeId::from_uuid(runtime.as_uuid()),
            instance: LoadedInstanceId::from_uuid(scope.id().as_uuid()),
            role,
        };
        let backend = match &self.backend {
            ArtifactBackend::Static(definition) => {
                let created = catch_unwind(AssertUnwindSafe(|| definition.create(context.clone())));
                match created {
                    Ok(Ok(implementation)) => ExecutionBackend::Static(implementation),
                    Ok(Err(error)) => {
                        scope.close().map_err(scope_error)?;
                        scope.finish().map_err(scope_error)?;
                        return Err(failure(error.message));
                    }
                    Err(_) => {
                        scope
                            .fail_cleanup("factory panicked".into())
                            .map_err(scope_error)?;
                        return Err(failure("factory panicked; cleanup required"));
                    }
                }
            }
            ArtifactBackend::Dynamic(artifact) => {
                create_native(artifact, &context, &scope, &background)?
            }
        };
        let verified_artifact = self.verified_for_context(&context);
        let execution = Arc::new(LoadedExecution {
            activation_binders: Mutex::new(Vec::new()),
            remote_event_bus: Mutex::new(None),
            lifecycle_callback_factory: Mutex::new(None),
            callback_runtime: std::sync::OnceLock::new(),
            callback_runtime_owner: Mutex::new(None),
            callback_runtime_pin: Mutex::new(None),
            callback_owner: Arc::new(callbacks::CallbackOwner::default()),
            retained_factory: Mutex::new(None),
            retained_dispatch: Mutex::new(None),
            verified_artifact,
            background,
            scope,
            context,
            backend,
            _bundled_executable: self.bundled_executable.clone(),
            lifecycle_selection: self.lifecycle_selection.clone(),
            control: AtomicBool::new(false),
            operation_admissions: AtomicUsize::new(0),
            kernel_dispatch: Mutex::new(None),
            host_dispatch: Mutex::new(None),
            host_state: Mutex::new(None),
            state: Mutex::new(ExecutionState {
                activation: None,
                activation_admission_failure: None,
                lifecycle_transport: None,
                registration: None,
                calls: BTreeMap::new(),
                next: 1,
                destroyed: false,
                retained: None,
            }),
        });
        // Intentionally retained until explicit destroy; observer loss cannot
        // authorize unloading or dropping an in-flight author future.
        execution
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .retained = Some(execution.clone());
        Ok(execution)
    }
}

fn create_native(
    artifact: &Arc<DynamicArtifact>,
    context: &PluginCreateContext,
    scope: &InstanceScope,
    background: &bmux_plugin_runtime::background::BackgroundTasks,
) -> Result<ExecutionBackend> {
    let input = encode_bounded(context)?;
    let mut handle = 0;
    let created =
        unsafe { (artifact.descriptor.create)(input.as_ptr(), input.len(), &raw mut handle) };
    if created != abi::OK || handle == 0 {
        // A failed native create can have partially initialized code or
        // foreign cleanup requirements. Retain executable evidence instead of
        // unloading based on an uncertain return status.
        std::mem::forget(artifact.clone());
        scope
            .fail_cleanup(format!("native create failed: {created}"))
            .map_err(scope_error)?;
        return Err(failure(format!(
            "native create failed: {created}; artifact retained"
        )));
    }
    if let Some(plug_rt) = artifact.plug_rt {
        // Work the instance spawns through plug-rt joins the same activation
        // scope as bundled work.
        plug_rt.bind_scope(handle, background.clone());
    }
    Ok(ExecutionBackend::Dynamic {
        artifact: artifact.clone(),
        handle,
    })
}

/// The actual selected reactor owner, shared by an instance and the callback
/// bindings derived from it. Bindings may be retained by host or author state
/// after their scopes retire; they must not keep the reactor alive forever.
/// Revocation happens only after successful instance destruction, which has
/// drained every admission, child, task and resource, so no admitted callback
/// entry (the only user of the selected runtime) can exist or be created.
struct RevocableRuntimePin(Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>);
impl RevocableRuntimePin {
    fn revoke(&self) -> Result<Option<Arc<dyn std::any::Any + Send + Sync>>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| failure("callback runtime pin poisoned"))?
            .take())
    }
}

enum ExecutionBackend {
    Static(Arc<dyn PluginInstance>),
    Dynamic {
        artifact: Arc<DynamicArtifact>,
        handle: u64,
    },
}
/// Host-owned scoped execution. IDs are local lookup keys, never wire credentials.
pub struct LoadedExecution {
    activation_binders: Mutex<Vec<Arc<ActivationBinder>>>,
    /// Host bus whose activation-scoped view this instance reaches: as an
    /// activation capability plus host-service subscriptions (in-process),
    /// or through the serialized plug-rt event port (dynamic).
    remote_event_bus: Mutex<Option<Arc<crate::EventBus>>>,
    lifecycle_callback_factory: Mutex<Option<Arc<callbacks::LifecycleCallbackFactory>>>,
    callback_runtime: std::sync::OnceLock<tokio::runtime::Handle>,
    /// Code-pin view snapshotted by callback bindings. It wraps
    /// `callback_runtime_pin`, so successful destruction revokes the actual
    /// runtime owner for the instance and every binding derived from it.
    callback_runtime_owner: Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    callback_runtime_pin: Mutex<Option<Arc<RevocableRuntimePin>>>,
    callback_owner: Arc<callbacks::CallbackOwner>,
    retained_factory: Mutex<Option<Arc<RetainedHostFactory>>>,
    retained_dispatch: Mutex<Option<Arc<dyn bmux_plugin_sdk::abi::retained::RetainedHostDispatch>>>,
    verified_artifact: Option<VerifiedArtifact>,
    background: bmux_plugin_runtime::background::BackgroundTasks,
    scope: InstanceScope,
    context: PluginCreateContext,
    backend: ExecutionBackend,
    _bundled_executable: Option<Arc<BundledExecutableArtifact>>,
    lifecycle_selection: Option<Arc<LifecycleSelection>>,
    control: AtomicBool,
    operation_admissions: AtomicUsize,
    kernel_dispatch: Mutex<Option<Arc<dyn crate::InvocationKernelDispatch>>>,
    host_dispatch: Mutex<Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>>,
    host_state: Mutex<Option<Arc<crate::PluginStateRegistry>>>,
    state: Mutex<ExecutionState>,
}
struct InvocationDispatch {
    invocation: InvocationScope,
    host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
    kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    callback: Option<callbacks::CallbackBinding>,
}
struct OperationDispatch {
    cancellation: ServiceCancellation,
    ports: InvocationPorts,
}
struct InvocationPorts {
    callback: Option<callbacks::CallbackBinding>,
    host: Option<bmux_plugin_sdk::BoundHostServicePort>,
    kernel: Option<crate::InvocationKernelPort>,
}

struct LifecycleTransport {
    table: ports::InvocationTable,
}
struct ExecutionState {
    lifecycle_transport: Option<LifecycleTransport>,
    activation: Option<ActivationScope>,
    // Retain the primary admission error when no cleanup scope was created.
    activation_admission_failure: Option<String>,
    registration: Option<crate::ActivationRegistrationPort>,
    calls: BTreeMap<u64, Arc<Call>>,
    next: u64,
    destroyed: bool,
    retained: Option<Arc<LoadedExecution>>,
}
struct CallStart {
    future: Option<PluginFuture<'static, NativeOutcome>>,
    cancellation: ServiceCancellation,
    invocation: Option<InvocationScope>,
    lifecycle: Option<bool>,
    admission: Option<AdmissionGuard>,
    operation: Option<NativeOperation>,
    stream_failure: Option<Arc<AtomicBool>>,
    deadline_ms: Option<u64>,
    table: Option<ports::InvocationTable>,
}
struct Call {
    state: Mutex<CallState>,
    cancellation: ServiceCancellation,
    invocation: Option<InvocationScope>,
    lifecycle: Option<bool>,
    finalizing: AtomicBool,
    acknowledging: AtomicBool,
    deadline: Option<std::time::Instant>,
    stream_failure: Option<Arc<AtomicBool>>,
    ports: Option<ports::InvocationTable>,
    native_acknowledged: AtomicBool,
    invocation_retired: Arc<AtomicBool>,
    /// Latest observer waker: woken on host-side progress (cancellation,
    /// deadline expiry) that the operation itself may not signal.
    observer: Mutex<Option<Waker>>,
    /// Observers that found the operation mid-poll; woken once it is released.
    contended: Mutex<Vec<Waker>>,
    deadline_armed: AtomicBool,
}
impl Call {
    fn observe(&self, waker: &Waker) {
        if let Ok(mut observer) = self.observer.lock()
            && !observer
                .as_ref()
                .is_some_and(|known| known.will_wake(waker))
        {
            *observer = Some(waker.clone());
        }
    }
    fn wake_observer(&self) {
        let observer = self.observer.lock().ok().and_then(|slot| slot.clone());
        if let Some(observer) = observer {
            observer.wake();
        }
    }
}
/// Timer target for a call deadline; never extends the call's lifetime.
struct DeadlineWake(std::sync::Weak<Call>);
impl Wake for DeadlineWake {
    fn wake(self: Arc<Self>) {
        if let Some(call) = self.0.upgrade() {
            call.wake_observer();
        }
    }
}
/// Releases observers that raced a poll, whatever path the poll exits by.
struct ReleaseContended<'a>(&'a Call);
impl Drop for ReleaseContended<'_> {
    fn drop(&mut self) {
        let contended = self
            .0
            .contended
            .lock()
            .map(|mut waiting| std::mem::take(&mut *waiting))
            .unwrap_or_default();
        for waker in contended {
            waker.wake();
        }
    }
}
enum CallState {
    Pending {
        future: Option<PluginFuture<'static, NativeOutcome>>,
        admission: Option<AdmissionGuard>,
    },
    Polling,
    Ready(NativeOutcome),
    Failed(String),
}

impl LoadedExecution {
    /// Bind the trusted owner factory once, before activation.
    /// # Errors
    /// Rejects rebinding and binding after activation begins.
    pub fn bind_retained_host_factory(&self, factory: Arc<RetainedHostFactory>) -> Result<()> {
        let _control = self.control()?;
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && (artifact.descriptor.version != abi::ABI_VERSION
                || artifact.descriptor.capabilities & abi::CAP_RETAINED_ASYNC_HOST_V1 == 0)
        {
            return Err(failure("native retained host capability not negotiated"));
        }
        if self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .is_some()
        {
            return Err(failure("retained factory must bind before activation"));
        }
        let mut slot = self
            .retained_factory
            .lock()
            .map_err(|_| failure("retained factory poisoned"))?;
        if slot.is_some() {
            return Err(failure("retained factory already bound"));
        }
        *slot = Some(factory);
        drop(slot);
        Ok(())
    }

    /// Open this execution's actual activation owner, including for static callers.
    /// # Errors
    /// Rejects absent grants, closed activation and unavailable execution ownership.
    pub fn open_retained_host(&self) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let table = self.open_retained_host_table()?;
        // SAFETY: the loader registry backs the transferred independent reference.
        unsafe { bmux_plugin_sdk::abi::retained::RetainedHostPort::from_native_owned(table) }
    }

    /// Open the independently owned lookup-capable retained host contract.
    /// # Errors
    /// Rejects missing selected owner grants or closed live activation admission.
    pub fn open_retained_host_v2(
        &self,
    ) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let table = retained::table_v2(self.open_retained_host_table()?);
        // SAFETY: exactly one independent reference was transferred above.
        unsafe { bmux_plugin_sdk::abi::retained::RetainedHostPort::from_native_owned_v2(table) }
    }

    /// Retry final transport release after the bound runtime has repaired cleanup.
    /// Never replays producer work. False leaves the execution/image pinned.
    /// # Errors
    /// Rejects outstanding multiple observers and failed cleanup.
    pub fn retry_release_retained_host(&self) -> Result<bool> {
        retained::retry_release(self)
    }

    fn open_retained_host_table(
        &self,
    ) -> Result<bmux_plugin_sdk::abi::retained::NativeRetainedHostPortV1> {
        let _control = self.operation_control()?;
        let activation = self.activation()?;
        let _admission = activation.try_admit().map_err(scope_error)?;
        let dispatch = self
            .retained_dispatch
            .lock()
            .map_err(|_| failure("retained dispatch poisoned"))?
            .clone()
            .ok_or_else(|| failure("retained host not granted"))?;
        let execution = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .retained
            .clone()
            .ok_or_else(|| failure("execution no longer retained"))?;
        retained::open(execution, dispatch)
    }

    /// Whether this execution runs inside the host process and can therefore
    /// receive in-process activation capabilities.
    #[must_use]
    pub const fn is_in_process(&self) -> bool {
        matches!(self.backend, ExecutionBackend::Static(_))
    }

    /// Register a trusted host activation binder before activation begins.
    ///
    /// Activation capabilities are live in-process values; they cannot cross
    /// the native ABI. A dynamic artifact is therefore refused here, explicitly,
    /// rather than activated without the capability its host meant to supply.
    ///
    /// # Errors
    /// Rejects dynamic artifacts and binding after activation has begun.
    pub fn bind_activation_binder(&self, binder: Arc<ActivationBinder>) -> Result<()> {
        let _control = self.control()?;
        if matches!(self.backend, ExecutionBackend::Dynamic { .. }) {
            return Err(failure(
                "host activation capabilities are in-process values and cannot cross the native ABI; \
                 a dynamic artifact cannot be bound to this host's activation binder",
            ));
        }
        if self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .is_some()
        {
            return Err(failure("activation binder must bind before activation"));
        }
        self.activation_binders
            .lock()
            .map_err(|_| failure("activation binders poisoned"))?
            .push(binder);
        Ok(())
    }

    /// Give every activation of this instance a view of `bus` scoped to that
    /// activation. In-process instances receive the view as an activation
    /// capability; a dynamic instance reaches the same view through the
    /// serialized plug-rt event port, so its authors use one `EventBus` API.
    ///
    /// # Errors
    /// The instance already activated, or a dynamic artifact was loaded
    /// without plug-rt (an in-process test descriptor).
    pub fn bind_event_bus(&self, bus: &Arc<crate::EventBus>) -> Result<()> {
        match &self.backend {
            ExecutionBackend::Static(_) => {
                self.bind_activation_binder(crate::event_bus_activation_binder(Arc::clone(bus)))?;
                // Host services subscribe through the same activation scope.
                *self
                    .remote_event_bus
                    .lock()
                    .map_err(|_| failure("remote event bus poisoned"))? = Some(Arc::clone(bus));
                Ok(())
            }
            ExecutionBackend::Dynamic { artifact, .. } => {
                let _control = self.control()?;
                if artifact.plug_rt.is_none() {
                    return Err(failure(
                        "dynamic artifact has no plug-rt table to carry its event bus",
                    ));
                }
                if self
                    .state
                    .lock()
                    .map_err(|_| failure("execution state poisoned"))?
                    .activation
                    .is_some()
                {
                    return Err(failure("event bus must bind before activation"));
                }
                *self
                    .remote_event_bus
                    .lock()
                    .map_err(|_| failure("remote event bus poisoned"))? = Some(Arc::clone(bus));
                Ok(())
            }
        }
    }

    fn bind_activation_capabilities(
        &self,
        activation: &ActivationScope,
    ) -> Result<bmux_plugin_sdk::definition::ActivationCapabilities> {
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend
            && let Some(plug_rt) = artifact.plug_rt
            && let Some(bus) = self
                .remote_event_bus
                .lock()
                .map_err(|_| failure("remote event bus poisoned"))?
                .clone()
        {
            // The view is owned by this activation: its registrations are
            // released when the activation closes or the scope is unbound.
            plug_rt.bind_scope_events(
                *handle,
                Arc::new(crate::event_bus::remote::BusScopeEvents(
                    bus.scoped(activation.clone()),
                )),
            );
        }
        let binders = self
            .activation_binders
            .lock()
            .map_err(|_| failure("activation binders poisoned"))?
            .clone();
        let capabilities = bmux_plugin_sdk::definition::ActivationCapabilities::default();
        for binder in binders {
            binder(activation, &capabilities)?;
        }
        Ok(capabilities)
    }

    fn bind_retained_activation(&self, activation: &ActivationScope) -> Result<()> {
        let factory = self
            .retained_factory
            .lock()
            .map_err(|_| failure("retained factory poisoned"))?
            .clone();
        if let Some(factory) = factory {
            let dispatch = factory(activation)?;
            *self
                .retained_dispatch
                .lock()
                .map_err(|_| failure("retained dispatch poisoned"))? = Some(dispatch);
        }
        Ok(())
    }

    fn retained_execution(&self) -> Result<Option<Arc<Self>>> {
        if self
            .retained_dispatch
            .lock()
            .map_err(|_| failure("retained dispatch poisoned"))?
            .is_none()
        {
            return Ok(None);
        }
        Ok(self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .retained
            .clone())
    }

    /// Provenance of the selected retained library and this exact created instance.
    /// Unverified loading paths return `None`; this receipt is never approval.
    #[must_use]
    pub const fn verified_artifact(&self) -> Option<&VerifiedArtifact> {
        self.verified_artifact.as_ref()
    }

    /// Serialized event subscriptions the host still holds for this
    /// instance's library (always 0 for in-process instances). Diagnostics.
    #[must_use]
    pub fn event_port_subscriptions(&self) -> usize {
        match &self.backend {
            ExecutionBackend::Dynamic { artifact, .. } => artifact.plug_rt.map_or(
                0,
                bmux_plugin_runtime::rt::host::PlugRtHost::event_subscription_count,
            ),
            ExecutionBackend::Static(_) => 0,
        }
    }

    /// Work the activation scope still owns: cooperative tasks, raw plug-rt
    /// tasks (bundled or dynamic) and running blocking jobs. Diagnostics only.
    #[must_use]
    pub fn pending_background_work(&self) -> Vec<String> {
        self.background.pending_names()
    }

    /// Close the actual activation task/callback tree and propagate cooperative
    /// cancellation. This is not acknowledgement, destruction, or timeout repair.
    ///
    /// # Errors
    /// Registration withdrawal, scope closure, or cancellation fails.
    pub fn cancel_background_tasks(&self) -> Result<()> {
        self.background.cancel().map_err(failure)?;
        if let Some(port) = self.registration_port()? {
            port.withdraw().map_err(|e| failure(e.to_string()))?;
        }
        let activation = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .clone();
        if let Some(activation) = activation {
            activation.close().map_err(scope_error)?;
        }
        let calls = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in calls {
            self.cancel(id)?;
        }
        Ok(())
    }

    /// Drain host-retained work, not just an observer. Outstanding operation
    /// futures must still be driven by their owning dispatch observer; dropping
    /// this wait never drops them or authorizes unload.
    ///
    /// # Errors
    /// Cancellation or activation draining fails.
    pub async fn drain_background_tasks(&self) -> Result<()> {
        self.cancel_background_tasks()?;
        // Teardown is bounded: stragglers are reported by owner and abandoned
        // (retained, never unloaded) instead of being awaited forever.
        let deadline = bmux_plugin_runtime::teardown_deadline();
        self.background
            .shutdown_until(deadline, None)
            .await
            .map_err(failure)?;
        let activation = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .clone();
        if let Some(activation) = activation {
            drain_activation(&activation, deadline).await?;
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Executable context fields, unsupported native export, or failed registration are encountered.
    pub fn collect_typed_services(
        &self,
        mut context: bmux_plugin_sdk::TypedServiceRegistrationContext<'_>,
    ) -> Result<bmux_plugin_sdk::TypedServiceRegistry> {
        reject_executable_context(context.host_kernel_bridge.is_some())?;
        let ExecutionBackend::Static(implementation) = &self.backend else {
            return Err(failure(
                "native typed-service registry export is not negotiated; portable invocation must be used explicitly",
            ));
        };
        // Strip incoming authority before rebinding to this selected activation.
        // Consume the caller's context without changing the public value API.
        let _ = context.host_service_port.take();
        let activation = self.activation()?;
        let services = self.activation_host_services(&activation, None)?;
        let context = bmux_plugin_sdk::TypedServiceRegistrationContext {
            host_service_port: services.as_ref(),
            ..context
        };
        let invocation = activation.begin_invocation().map_err(scope_error)?;
        let admission = invocation.try_admit().map_err(scope_error)?;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut registry = bmux_plugin_sdk::TypedServiceRegistry::new();
            implementation
                .register_typed_services_with_context(context, &mut registry)
                .map_err(|error| failure(error.message))?;
            Ok(registry)
        }));
        invocation.close().map_err(scope_error)?;
        drop(admission);
        // Synchronous registration hooks cannot launch invocation-owned async
        // work through this interface; finish verifies that assumption rather
        // than returning an apparently successful empty registry.
        invocation.finish().map_err(scope_error)?;
        if let Ok(value) = result {
            value
        } else {
            self.scope
                .fail_cleanup("typed registration hook panicked".into())
                .map_err(scope_error)?;
            Err(failure(
                "typed registration hook panicked; cleanup retained",
            ))
        }
    }

    fn resolved_lifecycle_context(
        &self,
        context: NativeLifecycleContext,
    ) -> Result<NativeLifecycleContext> {
        if let Some(selection) = &self.lifecycle_selection {
            return super::resolve_lifecycle_auxiliaries(
                &selection.registered,
                &selection.base,
                context,
            );
        }
        // Raw test/embedded definitions have no verified package root. Do not
        // accept caller-supplied executable handles as selected artifact metadata.
        let mut context = context;
        context.auxiliary_executables.clear();
        context.auxiliary_executable_errors.clear();
        Ok(context)
    }

    /// Standalone command activation uses this exact implementation and owner.
    /// Observe/acknowledge this operation before begin(Command) on the same self.
    ///
    /// # Errors
    /// Activation admission or command lifecycle startup fails.
    pub fn begin_command_activation(
        &self,
        command: &bmux_plugin_sdk::NativeCommandContext,
        policy: PluginConcurrencyConfig,
    ) -> Result<u64> {
        reject_executable_context(command.host_kernel_bridge.is_some())?;
        self.begin_activation(
            NativeLifecycleContext {
                plugin_id: command.plugin_id.clone(),
                required_capabilities: command.required_capabilities.clone(),
                provided_capabilities: command.provided_capabilities.clone(),
                services: command.services.clone(),
                available_capabilities: command.available_capabilities.clone(),
                enabled_plugins: command.enabled_plugins.clone(),
                plugin_search_roots: command.plugin_search_roots.clone(),
                registered_plugins: Vec::new(),
                host: command.host.clone(),
                connection: command.connection.clone(),
                settings: command.settings.clone(),
                plugin_settings_map: command.plugin_settings_map.clone(),
                host_kernel_bridge: None,
                auxiliary_executables: BTreeMap::new(),
                auxiliary_executable_errors: BTreeMap::new(),
                host_service_port: None,
                capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
            },
            policy,
        )
    }

    /// Select the actual owning host registry; never substitutes global state.
    ///
    /// # Errors
    /// Host state is already bound or its lock is poisoned.
    pub fn bind_host_state(&self, host: Arc<crate::PluginStateRegistry>) -> Result<()> {
        let _control = self.control()?;
        if self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .is_some()
        {
            return Err(failure("host state must be selected before activation"));
        }
        let mut state = self
            .host_state
            .lock()
            .map_err(|_| failure("host state selection poisoned"))?;
        if state.is_some() {
            return Err(failure("host state already selected"));
        }
        *state = Some(host);
        drop(state);
        Ok(())
    }

    /// Trusted host wiring only; requests cannot install or replace this port.
    ///
    /// # Errors
    /// Work has started, dispatch is already bound, or a lock is poisoned.
    pub fn bind_host_dispatch(
        &self,
        dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    ) -> Result<()> {
        let _control = self.control()?;
        let state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        if state.activation.is_some() || state.destroyed || !state.calls.is_empty() {
            return Err(failure("host dispatch must be bound before activation"));
        }
        drop(state);
        let mut bound = self
            .host_dispatch
            .lock()
            .map_err(|_| failure("host dispatch poisoned"))?;
        if bound.is_some() {
            return Err(failure("host dispatch already bound"));
        }
        *bound = Some(dispatch);
        drop(bound);
        Ok(())
    }
    /// Bind trusted kernel dispatch once before activation. Each invocation gets
    /// a fresh scope-bound capability; operation payloads never grant access.
    /// Synchronous execution must be driven from a blocking plugin worker.
    ///
    /// # Errors
    /// Returns an error if execution has started, the grant is already bound,
    /// the native ABI cannot carry kernel requests, or a state lock is poisoned.
    pub fn bind_kernel_dispatch(
        &self,
        dispatch: Arc<dyn crate::InvocationKernelDispatch>,
    ) -> Result<()> {
        let _control = self.control()?;
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && (artifact.descriptor.version != abi::ABI_VERSION
                || artifact.descriptor.capabilities & abi::CAP_KERNEL_REQUESTS == 0)
        {
            return Err(failure("native kernel capability not negotiated"));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        if state.activation.is_some() || state.destroyed || !state.calls.is_empty() {
            return Err(failure("kernel dispatch must be bound before activation"));
        }
        drop(state);
        let mut bound = self
            .kernel_dispatch
            .lock()
            .map_err(|_| failure("kernel dispatch poisoned"))?;
        if bound.is_some() {
            return Err(failure("kernel dispatch already bound"));
        }
        *bound = Some(dispatch);
        drop(bound);
        Ok(())
    }

    #[cfg(test)]
    fn activation_owned_services(
        activation: ActivationScope,
        dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    ) -> bmux_plugin_sdk::PluginHostServices {
        Self::activation_services(activation, dispatch, None)
    }

    fn activation_services(
        activation: ActivationScope,
        dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
        setup: Option<InvocationScope>,
    ) -> bmux_plugin_sdk::PluginHostServices {
        activation_ports::services(activation, dispatch, setup)
    }

    fn activation_host_services(
        &self,
        activation: &ActivationScope,
        setup: Option<&InvocationScope>,
    ) -> Result<Option<bmux_plugin_sdk::PluginHostServices>> {
        let dispatch = self
            .host_dispatch
            .lock()
            .map_err(|_| failure("host dispatch poisoned"))?
            .clone();
        let kernel = self
            .kernel_dispatch
            .lock()
            .map_err(|_| failure("kernel dispatch poisoned"))?
            .clone();
        Ok(Self::activation_host_services_with(
            activation, setup, dispatch, kernel,
        ))
    }

    /// Activation services over an explicit dispatch: calls during setup use
    /// the lifecycle invocation, later calls mint fresh activation invocations,
    /// and nothing is admitted once deactivation closes the activation.
    fn activation_host_services_with(
        activation: &ActivationScope,
        setup: Option<&InvocationScope>,
        dispatch: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    ) -> Option<bmux_plugin_sdk::PluginHostServices> {
        if dispatch.is_none() && kernel.is_none() {
            return None;
        }
        let services = dispatch.map_or_else(
            || {
                bmux_plugin_sdk::PluginHostServices::new(|_| {
                    Err(failure("host services not bound"))
                })
            },
            |dispatch| Self::activation_services(activation.clone(), dispatch, setup.cloned()),
        );
        Some(if let Some(kernel) = kernel {
            let activation = activation.clone();
            let setup = setup.cloned();
            services.with_kernel(move |request| {
                activation_ports::kernel(&activation, setup.as_ref(), kernel.clone(), request)
            })
        } else {
            services
        })
    }

    fn kernel_port(
        &self,
        invocation: &InvocationScope,
    ) -> Result<Option<crate::InvocationKernelPort>> {
        let dispatch = self
            .kernel_dispatch
            .lock()
            .map_err(|_| failure("kernel dispatch poisoned"))?
            .clone();
        Ok(dispatch.map(|dispatch| crate::InvocationKernelPort::new(invocation.clone(), dispatch)))
    }

    fn host_port(
        &self,
        invocation: &InvocationScope,
    ) -> Result<Option<bmux_plugin_sdk::BoundHostServicePort>> {
        let dispatch = self
            .host_dispatch
            .lock()
            .map_err(|_| failure("host dispatch poisoned"))?
            .clone();
        self.bind_host_port(invocation, dispatch)
    }

    fn bind_host_port(
        &self,
        invocation: &InvocationScope,
        dispatch: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
    ) -> Result<Option<bmux_plugin_sdk::BoundHostServicePort>> {
        let host = self
            .host_state
            .lock()
            .map_err(|_| failure("host state selection poisoned"))?
            .clone();
        dispatch
            .map(|dispatch| {
                if let Some(host) = host {
                    host.registrations()
                        .context_for(invocation)
                        .map_err(|e| failure(e.to_string()))?
                        .bind_service(invocation.clone(), dispatch)
                        .map_err(|e| failure(e.to_string()))
                } else {
                    Ok(bmux_plugin_sdk::BoundHostServicePort::new(
                        invocation.clone(),
                        dispatch,
                    ))
                }
            })
            .transpose()
    }
    /// Select the actual host reactor before activation or callback binding.
    /// The caller must retain and drive its runtime owner through settlement;
    /// a handle alone does not keep a Tokio runtime operational.
    ///
    /// # Errors
    /// Rejects replacement or selection after this instance leaves Created.
    pub fn bind_callback_runtime(&self, runtime: tokio::runtime::Handle) -> Result<()> {
        let _control = self.control()?;
        if self.scope.state().map_err(scope_error)? != bmux_plugin_runtime::LifecycleState::Created
        {
            return Err(failure("select callback runtime before activation"));
        }
        self.callback_runtime
            .set(runtime)
            .map_err(|_| failure("callback runtime already selected"))
    }

    /// The host reactor selected for this instance's callbacks, if bound.
    /// Host dispatch uses this explicitly instead of whatever runtime the
    /// polling thread happens to have entered.
    #[must_use]
    pub fn callback_runtime(&self) -> Option<tokio::runtime::Handle> {
        self.callback_runtime.get().cloned()
    }

    /// Retain the actual selected reactor owner independently of observer lifetime.
    /// Existing callback bindings must not precede this selection. Each later
    /// binding shares the instance's pin; successful destruction (which proves
    /// no admitted entry remains) revokes it for the instance and all bindings,
    /// so retained bindings cannot keep the reactor alive past retirement.
    /// The host supplies the actual owner, never an identifier or placeholder.
    ///
    /// # Errors
    /// Rejects late/repeated selection or a handle for a different reactor.
    pub fn bind_callback_runtime_owner(
        &self,
        runtime: tokio::runtime::Handle,
        pin: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Result<()> {
        let _control = self.control()?;
        if self.scope.state().map_err(scope_error)? != bmux_plugin_runtime::LifecycleState::Created
        {
            return Err(failure("select callback runtime owner before activation"));
        }
        let mut owner = self
            .callback_runtime_owner
            .lock()
            .map_err(|_| failure("callback runtime owner poisoned"))?;
        if owner.is_some() {
            return Err(failure("callback runtime owner already selected"));
        }
        if let Some(selected) = self.callback_runtime.get() {
            if selected.id() != runtime.id() {
                return Err(failure("callback runtime owner reactor mismatch"));
            }
        } else {
            self.callback_runtime
                .set(runtime)
                .map_err(|_| failure("callback runtime already selected"))?;
        }
        let revocable = Arc::new(RevocableRuntimePin(Mutex::new(Some(pin))));
        *self
            .callback_runtime_pin
            .lock()
            .map_err(|_| failure("callback runtime pin poisoned"))? = Some(revocable.clone());
        *owner = Some(revocable as Arc<dyn std::any::Any + Send + Sync>);
        drop(owner);
        Ok(())
    }

    pub const fn scope(&self) -> &InstanceScope {
        &self.scope
    }
    pub const fn create_context(&self) -> &PluginCreateContext {
        &self.context
    }

    ///
    /// # Errors
    /// Activation admission, registration binding, or lifecycle startup fails.
    pub fn begin_activation(
        &self,
        native: NativeLifecycleContext,
        policy: PluginConcurrencyConfig,
    ) -> Result<u64> {
        let result = self.begin_activation_inner(native, policy);
        if let Err(error) = &result {
            let mut state = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?;
            if state.activation.is_none() && state.activation_admission_failure.is_none() {
                state.activation_admission_failure = Some(error.to_string());
            }
        }
        result
    }

    fn begin_activation_inner(
        &self,
        native: NativeLifecycleContext,
        policy: PluginConcurrencyConfig,
    ) -> Result<u64> {
        reject_executable_context(native.host_kernel_bridge.is_some())?;
        let native = self.resolved_lifecycle_context(native)?;
        let _control = self.control()?;
        self.ensure_capacity()?;
        self.ensure_activation_port_negotiated()?;
        let activation = self.scope.begin_activation(policy).map_err(scope_error)?;
        let mut context = PluginLifecycleContext {
            callback_execution: None,
            scope: self.context.clone(),
            activation: ActivationId::from_uuid(activation.id().as_uuid()),
            native,
            capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
        };
        let registration = self
            .host_state
            .lock()
            .map_err(|_| failure("host state selection poisoned"))?
            .as_ref()
            .map(|host| {
                host.registrations()
                    .bind(activation.clone())
                    .map_err(|e| failure(e.to_string()))
            })
            .transpose()?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?;
            state.activation = Some(activation.clone());
            state.activation_admission_failure = None;
            state.registration = registration;
        }
        self.bind_retained_activation(&activation)?;
        context.capabilities = self.bind_activation_capabilities(&activation)?;
        let invocation = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
            .map_err(scope_error)?;
        context.native.host_service_port = self
            .activation_host_services(&activation, Some(&invocation))?
            .map(|services| self.with_event_route(services))
            .transpose()?;
        let admission = invocation.try_admit().map_err(scope_error)?;
        self.start(
            NativeOperation::Activate(context),
            Some(invocation),
            Some(true),
            Some(admission),
        )
    }

    /// A dynamic guest receives host services at activation only through the
    /// retained activation port. A guest that did not negotiate it is refused
    /// before any activation scope exists, never degraded to the activate
    /// call's own invocation ports, which close when `activate` returns.
    fn ensure_activation_port_negotiated(&self) -> Result<()> {
        let ExecutionBackend::Dynamic { artifact, .. } = &self.backend else {
            return Ok(());
        };
        if artifact.descriptor.version == abi::ABI_VERSION
            && artifact.descriptor.capabilities & abi::CAP_RETAINED_LIFECYCLE != 0
        {
            return Ok(());
        }
        let grants_host = self
            .host_dispatch
            .lock()
            .map_err(|_| failure("host dispatch poisoned"))?
            .is_some()
            || self
                .kernel_dispatch
                .lock()
                .map_err(|_| failure("kernel dispatch poisoned"))?
                .is_some();
        if grants_host {
            return Err(failure("retained lifecycle host services not negotiated"));
        }
        Ok(())
    }

    fn ensure_previous_lifecycle_settled(&self) -> Result<()> {
        let calls = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for call in calls {
            if call.lifecycle.is_some()
                && !matches!(&*call.state.lock().map_err(|_| failure("operation state poisoned"))?,
                CallState::Ready(outcome) if !matches!(outcome, NativeOutcome::Unit))
            {
                return Err(failure(
                    "acknowledge the previous successful lifecycle outcome before closing",
                ));
            }
        }
        Ok(())
    }

    /// Close admissions before waiting. Cancelling this observer retains state.
    ///
    /// # Errors
    /// Ordinary work has not drained or cleanup startup fails.
    pub async fn begin_deactivation(&self, native: NativeLifecycleContext) -> Result<u64> {
        // Failed admission has no cleanup root. Preserve its primary diagnostic
        // before auxiliary resolution or later cleanup preconditions can mask it.
        self.activation()?;
        reject_executable_context(native.host_kernel_bridge.is_some())?;
        let native = self.resolved_lifecycle_context(native)?;
        let _control = self.control()?;
        self.ensure_previous_lifecycle_settled()?;
        if let Some(port) = self.registration_port()? {
            port.withdraw().map_err(|e| failure(e.to_string()))?;
        }
        let activation = self.activation()?;
        activation.close().map_err(scope_error)?;
        let calls = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for call in calls {
            self.cancel(call)?;
        }
        drain_activation(&activation, bmux_plugin_runtime::teardown_deadline()).await?;
        let calls = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for call in calls {
            if !matches!(&*call.state.lock().map_err(|_| failure("operation state poisoned"))?,
                CallState::Ready(outcome) if call.lifecycle.is_some() && !matches!(outcome, NativeOutcome::Unit))
            {
                return Err(failure(
                    "acknowledge outstanding outcomes before deactivation",
                ));
            }
        }
        // The native adapter requires its terminal activation operation to be
        // acknowledged before cleanup can start. Keep the host-owned failure
        // outcome retained until acknowledge_repaired_cleanup; only dispose the
        // duplicate native transport retention here, never rerun its effects.
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            let calls = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?
                .calls
                .iter()
                .map(|(id, call)| (*id, call.clone()))
                .collect::<Vec<_>>();
            for (id, call) in calls {
                if !call.native_acknowledged.load(Ordering::Acquire) {
                    if let Some(table) = &call.ports
                        && call.lifecycle != Some(true)
                    {
                        ports::HostPorts::retire(table)?;
                    }
                    status(unsafe { (artifact.descriptor.ack)(*handle, id) })?;
                    if call.lifecycle == Some(true)
                        && let Some(table) = &call.ports
                    {
                        self.state
                            .lock()
                            .map_err(|_| failure("execution state poisoned"))?
                            .lifecycle_transport = Some(LifecycleTransport { table: *table });
                    }
                    call.native_acknowledged.store(true, Ordering::Release);
                }
            }
        }
        // Ordinary work drains before minting cleanup authority. Do not drain
        // the activation again while that fresh root is serving the author hook:
        // activation drain closes the subtree and would cancel its callback port.
        // The registration owner is deliberately still bound here; only final
        // cleanup acknowledgement may call withdraw_and_drain and remove it.
        let invocation = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
            .map_err(scope_error)?;
        let admission = invocation.try_admit().map_err(scope_error)?;
        self.start(
            NativeOperation::Deactivate(PluginLifecycleContext {
                callback_execution: None,
                scope: self.context.clone(),
                activation: ActivationId::from_uuid(activation.id().as_uuid()),
                native,
                capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
            }),
            Some(invocation),
            Some(false),
            Some(admission),
        )
    }

    /// The caller supplies host-built operation contexts, never decoded authority.
    /// Checked envelopes are rejected until this artifact advertises their exact
    /// provider contract; ordinary version strings are not contract evidence.
    ///
    /// # Errors
    /// Invocation admission, context validation, stream limits, or operation startup fails.
    pub fn begin(&self, operation: NativeOperation) -> Result<u64> {
        self.begin_with_cancellation(operation, ServiceCancellation::new())
    }

    /// Allocate a host-owned scope for explicit per-operation dispatch binding.
    /// The caller must close/finish an unused scope if operation admission fails.
    /// # Errors
    /// Rejects unavailable/retiring activations or exhausted admission capacity.
    pub fn begin_invocation_scope(&self) -> Result<InvocationScope> {
        let _control = self.operation_control()?;
        self.ensure_capacity()?;
        self.activation()?.begin_invocation().map_err(scope_error)
    }

    /// Start with explicit grants; `None` never falls back to instance dispatch.
    /// Supplied dispatchers must contain trusted per-operation caller context.
    /// # Errors
    /// Rejects foreign runtime/instance/activation scopes, reused scopes,
    /// unsupported native capabilities, invalid contexts and failed admission.
    pub fn begin_with_dispatch(
        &self,
        operation: NativeOperation,
        cancellation: ServiceCancellation,
        invocation: InvocationScope,
        host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    ) -> Result<u64> {
        self.begin_operation(
            operation,
            cancellation,
            Some(InvocationDispatch {
                invocation,
                host,
                kernel,
                callback: None,
            }),
        )
    }

    /// Start with an explicitly selected callback executor and admitted ancestry.
    /// # Errors
    /// Rejects mismatched binding/scope before author code executes.
    pub fn begin_with_callback_dispatch(
        &self,
        operation: NativeOperation,
        binding: callbacks::CallbackBinding,
        host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    ) -> Result<u64> {
        let invocation = binding.invocation().clone();
        if binding
            .execution()
            .is_none_or(|execution| !std::ptr::eq(self, Arc::as_ptr(execution)))
        {
            return Err(failure("callback binding belongs to another execution"));
        }
        self.begin_operation(
            operation,
            binding.cancellation(),
            Some(InvocationDispatch {
                invocation,
                host,
                kernel,
                callback: Some(binding),
            }),
        )
    }

    fn validate_invocation_dispatch(&self, dispatch: &InvocationDispatch) -> Result<()> {
        let invocation = &dispatch.invocation;
        if invocation.runtime_id() != self.scope.runtime_id()
            || invocation.instance_id() != self.scope.id()
            || invocation.activation_id().map_err(scope_error)? != self.activation()?.id()
        {
            return Err(failure(
                "operation scope does not belong to selected runtime/instance/activation",
            ));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        if state.calls.values().any(|call| {
            call.invocation
                .as_ref()
                .is_some_and(|scope| scope.id() == invocation.id())
        }) {
            return Err(failure("invocation already owns an operation"));
        }
        drop(state);
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend {
            if dispatch.host.is_some()
                && artifact.descriptor.capabilities & abi::CAP_HOST_SERVICES == 0
            {
                return Err(failure("host services not negotiated"));
            }
            if dispatch.kernel.is_some()
                && (artifact.descriptor.version != abi::ABI_VERSION
                    || artifact.descriptor.capabilities & abi::CAP_KERNEL_REQUESTS == 0)
            {
                return Err(failure("native kernel capability not negotiated"));
            }
        }
        Ok(())
    }

    fn invocation_ports(
        &self,
        invocation: &InvocationScope,
        dispatch: Option<InvocationDispatch>,
    ) -> Result<InvocationPorts> {
        if let Some(dispatch) = dispatch {
            if let Some(callback) = &dispatch.callback {
                callback.validate()?;
                if callback.invocation().id() != invocation.id() {
                    return Err(failure("callback binding invocation mismatch"));
                }
            }
            Ok(InvocationPorts {
                callback: dispatch.callback,
                host: self.bind_host_port(invocation, dispatch.host)?,
                kernel: dispatch
                    .kernel
                    .map(|kernel| crate::InvocationKernelPort::new(invocation.clone(), kernel)),
            })
        } else {
            Ok(InvocationPorts {
                callback: None,
                host: self.host_port(invocation)?,
                kernel: self.kernel_port(invocation)?,
            })
        }
    }

    /// Nested calls pass their parent's live cancellation handle. Siblings
    /// receive distinct operation identities and instance-default dispatch.
    ///
    /// # Errors
    /// Rejects invalid or cancelled contexts, unavailable activation, exhausted
    /// capacity, failed admission, unsupported capabilities and startup failures.
    pub fn begin_with_cancellation(
        &self,
        operation: NativeOperation,
        cancellation: ServiceCancellation,
    ) -> Result<u64> {
        self.begin_operation(operation, cancellation, None)
    }

    fn begin_operation(
        &self,
        mut operation: NativeOperation,
        cancellation: ServiceCancellation,
        dispatch: Option<InvocationDispatch>,
    ) -> Result<u64> {
        reject_operation_executable_fields(&operation)?;
        let _control = self.operation_control()?;
        self.ensure_capacity()?;
        if cancellation.is_cancelled() {
            return Err(failure("parent invocation already cancelled"));
        }
        if matches!(operation, NativeOperation::Streaming { .. }) {
            return Err(failure(
                "streaming requires begin_streaming and an explicit event sink",
            ));
        }
        if matches!(
            operation,
            NativeOperation::Activate(_) | NativeOperation::Deactivate(_)
        ) {
            return Err(failure("use explicit lifecycle entrypoints"));
        }
        if let NativeOperation::Invoke { context, .. }
        | NativeOperation::Streaming { context, .. } = &operation
        {
            if context.cancellation.is_cancelled() || context.cancellation.deadline_ms == Some(0) {
                return Err(failure(
                    "invocation cancelled or deadline expired before admission",
                ));
            }
            validate_checked_service(&self.backend, context)?;
        }
        // Dynamic input admission must happen before allocating invocation scope
        // or consuming an operation identity, exactly as for the static path.
        validate_operation_limits(&operation)?;
        if let Some(dispatch) = &dispatch {
            self.validate_invocation_dispatch(dispatch)?;
        }
        let invocation = match &dispatch {
            Some(dispatch) => dispatch.invocation.clone(),
            None => self.activation()?.begin_invocation().map_err(scope_error)?,
        };
        let admission = invocation.try_admit().map_err(scope_error)?;
        let ports = self.invocation_ports(&invocation, dispatch)?;
        let id = PluginInvocationId::new();
        match &mut operation {
            NativeOperation::Command { invocation_id, .. }
            | NativeOperation::Extension { invocation_id, .. } => *invocation_id = id,
            NativeOperation::Invoke {
                invocation_id,
                context,
            }
            | NativeOperation::Streaming {
                invocation_id,
                context,
            } => {
                *invocation_id = id.clone();
                context.cancellation.invocation_id = id;
            }
            _ => {}
        }
        self.start_with_ports(
            operation,
            Some(invocation),
            None,
            Some(admission),
            OperationDispatch {
                cancellation,
                ports,
            },
        )
    }

    /// Static streaming remains native Rust dispatch, with an actual event sink.
    /// v2 currently declares no event-port capability; reject before creating work.
    ///
    /// # Errors
    /// Invocation admission, context validation, stream limits, or operation startup fails.
    pub fn begin_streaming(&self, context: NativeStreamingServiceContext) -> Result<u64> {
        self.begin_streaming_with_cancellation(context, ServiceCancellation::new())
    }

    ///
    /// # Errors
    /// Invocation admission, context validation, stream limits, or operation startup fails.
    pub fn begin_streaming_with_cancellation(
        &self,
        context: NativeStreamingServiceContext,
        cancellation: ServiceCancellation,
    ) -> Result<u64> {
        self.begin_streaming_operation(context, cancellation, None)
    }

    /// Streaming counterpart of `begin_with_dispatch`, with no ambient fallback.
    /// # Errors
    /// Rejects foreign/reused scopes, invalid context, unsupported capabilities,
    /// and failed admission. Events and both author grants share this scope.
    pub fn begin_streaming_with_dispatch(
        &self,
        context: NativeStreamingServiceContext,
        cancellation: ServiceCancellation,
        invocation: InvocationScope,
        host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    ) -> Result<u64> {
        self.begin_streaming_operation(
            context,
            cancellation,
            Some(InvocationDispatch {
                invocation,
                host,
                kernel,
                callback: None,
            }),
        )
    }

    /// Streaming dispatch with the same explicit callback authority as unary calls.
    /// # Errors
    /// Rejects foreign callback bindings and unsupported native negotiation.
    pub fn begin_streaming_with_callback_dispatch(
        &self,
        context: NativeStreamingServiceContext,
        binding: callbacks::CallbackBinding,
        host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
    ) -> Result<u64> {
        if binding
            .execution()
            .is_none_or(|execution| !std::ptr::eq(self, Arc::as_ptr(execution)))
        {
            return Err(failure("callback binding belongs to another execution"));
        }
        self.begin_streaming_operation(
            context,
            binding.cancellation(),
            Some(InvocationDispatch {
                invocation: binding.invocation().clone(),
                host,
                kernel,
                callback: Some(binding),
            }),
        )
    }

    fn begin_streaming_operation(
        &self,
        mut context: NativeStreamingServiceContext,
        cancellation: ServiceCancellation,
        dispatch: Option<InvocationDispatch>,
    ) -> Result<u64> {
        reject_executable_context(context.service.host_kernel_bridge.is_some())?;
        if context.service.request.payload.len() > abi::MAX_BUFFER_BYTES
            || context.service.cancellation.deadline_ms == Some(0)
        {
            return Err(failure("stream payload exceeds bound or deadline expired"));
        }
        let _control = self.operation_control()?;
        self.ensure_capacity()?;
        if cancellation.is_cancelled() || context.service.cancellation.is_cancelled() {
            return Err(failure("stream cancelled"));
        }
        let deadline = context.service.cancellation.deadline_ms;
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && artifact.descriptor.capabilities & abi::CAP_STREAM_EVENTS == 0
        {
            return Err(failure("stream event capability not negotiated"));
        }
        validate_checked_service(&self.backend, &context.service)?;
        if let Some(dispatch) = &dispatch {
            self.validate_invocation_dispatch(dispatch)?;
        }
        let invocation = match &dispatch {
            Some(dispatch) => dispatch.invocation.clone(),
            None => self.activation()?.begin_invocation().map_err(scope_error)?,
        };
        let admission = invocation.try_admit().map_err(scope_error)?;
        let InvocationPorts {
            host,
            kernel,
            callback,
        } = self.invocation_ports(&invocation, dispatch)?;
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && host.is_some()
            && artifact.descriptor.capabilities & abi::CAP_HOST_SERVICES == 0
        {
            return Err(failure("host services not negotiated"));
        }
        let call = PluginCallContext {
            callback_execution: callback.as_ref().map(callbacks::CallbackBinding::context),
            invocation_id: PluginInvocationId::new(),
            cancellation: cancellation.clone(),
            host_services: self.decorate_host_services(host.clone(), kernel.clone())?,
        };
        Self::bind_stream_call_context(&mut context.service, &call);
        let stream_failed =
            Self::bind_stream_events(&mut context, &invocation, &call.invocation_id);
        let operation = NativeOperation::Streaming {
            invocation_id: call.invocation_id.clone(),
            context: context.service.clone(),
        };
        if callback.is_some()
            && let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && artifact.descriptor.capabilities & abi::callback_authority::CAP_CALLBACK_AUTHORITY_V1
                == 0
        {
            return Err(failure(
                "native streaming callback authority not negotiated",
            ));
        }
        let table = if matches!(self.backend, ExecutionBackend::Dynamic { .. }) {
            Some(
                ports::HostPorts::new(invocation.clone(), Some(context.events.clone()), host)
                    .with_callback(callback)
                    .with_retained_execution(self.retained_execution()?)
                    .install_for(artifact_version(&self.backend), kernel)?,
            )
        } else {
            None
        };
        let observed_failure = stream_failed.clone();
        let future = if let ExecutionBackend::Static(implementation) = &self.backend {
            let implementation = implementation.clone();
            Some(Box::pin(async move {
                let result = implementation.invoke_streaming(call, context).await;
                if stream_failed.load(Ordering::Acquire) {
                    NativeOutcome::Error {
                        code: 1,
                        message: "stream delivery failed".into(),
                    }
                } else {
                    author_result(result, NativeOutcome::Service)
                }
            }) as PluginFuture<'static, NativeOutcome>)
        } else {
            None
        };
        self.insert_call(CallStart {
            future,
            cancellation,
            invocation: Some(invocation),
            lifecycle: None,
            admission: Some(admission),
            operation: Some(operation),
            stream_failure: Some(observed_failure),
            deadline_ms: deadline,
            table,
        })
    }

    fn bind_stream_call_context(
        context: &mut bmux_plugin_sdk::NativeServiceContext,
        call: &PluginCallContext,
    ) {
        context.host_service_port.clone_from(&call.host_services);
        context
            .cancellation
            .invocation_id
            .clone_from(&call.invocation_id);
    }

    fn bind_stream_events(
        context: &mut NativeStreamingServiceContext,
        invocation: &InvocationScope,
        identity: &PluginInvocationId,
    ) -> Arc<AtomicBool> {
        let failed = Arc::new(AtomicBool::new(false));
        context.events = ServiceEventSinkHandle::new(ScopedEventSink {
            invocation: invocation.clone(),
            downstream: context.events.clone(),
            identity: identity.clone(),
            remaining: Mutex::new((1024, abi::MAX_BUFFER_BYTES)),
            failed: failed.clone(),
        });
        failed
    }

    fn ensure_capacity(&self) -> Result<()> {
        let has_host_dispatch = self
            .host_dispatch
            .lock()
            .map_err(|_| failure("host dispatch poisoned"))?
            .is_some();
        if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
            && has_host_dispatch
            && artifact.descriptor.capabilities & abi::CAP_HOST_SERVICES == 0
        {
            return Err(failure("native host services capability not negotiated"));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        if state.destroyed || state.calls.len() >= abi::MAX_OPERATIONS || state.next == u64::MAX {
            return Err(failure(
                "execution closed or retained operation capacity exhausted",
            ));
        }
        drop(state);
        Ok(())
    }
    fn operation_control(&self) -> Result<OperationControlGuard<'_>> {
        // SeqCst orders the two counters: a lifecycle owner and an ordinary
        // admission can never both observe the other as absent.
        self.operation_admissions
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < abi::MAX_OPERATIONS).then(|| count + 1)
            })
            .map_err(|_| failure("concurrent operation admission capacity exhausted"))?;
        let guard = OperationControlGuard(&self.operation_admissions);
        if self.control.load(Ordering::SeqCst) {
            return Err(failure(
                "lifecycle transition in progress; operation not admitted",
            ));
        }
        Ok(guard)
    }

    fn control(&self) -> Result<ControlGuard<'_>> {
        self.control
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| failure("execution lifecycle control busy; operation not admitted"))?;
        let guard = ControlGuard(&self.control);
        if self.operation_admissions.load(Ordering::SeqCst) != 0 {
            return Err(failure(
                "ordinary operation admission in progress; lifecycle not admitted",
            ));
        }
        Ok(guard)
    }
    fn bound_host_services(
        port: bmux_plugin_sdk::BoundHostServicePort,
    ) -> bmux_plugin_sdk::PluginHostServices {
        let recovery = port.clone();
        bmux_plugin_sdk::PluginHostServices::from_bound(port)
            .with_retained_host_v2(move || recovery.open_retained_host_v2())
    }

    fn decorate_host_services(
        &self,
        port: Option<bmux_plugin_sdk::BoundHostServicePort>,
        kernel: Option<crate::InvocationKernelPort>,
    ) -> Result<Option<bmux_plugin_sdk::definition::PluginHostServices>> {
        let registration = self.registration_port()?;
        if port.is_none() && registration.is_none() && kernel.is_none() {
            return Ok(None);
        }
        // Preserve both synchronous and optional async dispatch on the actual
        // invocation-bound port; decorators must not reconstruct a sync facade.
        let services = port.map_or_else(
            || {
                bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
                    Err(failure("host services not bound"))
                })
            },
            Self::bound_host_services,
        );
        let services = if let Some(registration) = registration {
            services.with_registrations(move |mutation| apply_registration(&registration, mutation))
        } else {
            services
        };
        let services = if let Some(kernel) = kernel {
            kernel.bind_services(services)
        } else {
            services
        };
        Ok(Some(self.with_event_route(services)?))
    }

    /// Give in-process host services the activation's event subscriptions
    /// ([`bmux_plugin_sdk::PluginHostServices::subscribe_event`]) over a view
    /// of the bound bus scoped to the current activation. Dynamic instances
    /// subscribe through the serialized event port in the library instead.
    fn with_event_route(
        &self,
        services: bmux_plugin_sdk::PluginHostServices,
    ) -> Result<bmux_plugin_sdk::PluginHostServices> {
        if !matches!(self.backend, ExecutionBackend::Static(_)) {
            return Ok(services);
        }
        let Some(bus) = self
            .remote_event_bus
            .lock()
            .map_err(|_| failure("remote event bus poisoned"))?
            .clone()
        else {
            return Ok(services);
        };
        let Ok(activation) = self.activation() else {
            return Ok(services);
        };
        let view = bus.scoped(activation);
        let register = Arc::clone(&view);
        let publish = Arc::clone(&view);
        Ok(services
            .with_event_subscriptions(move |kind, state| {
                crate::event_bus::remote::subscribe_payloads(&view, kind, state)
            })
            .with_event_publisher(
                move |kind, state| {
                    crate::event_bus::remote::register_payloads(&register, kind, state)
                },
                move |kind, payload| {
                    crate::event_bus::remote::publish_payload(&publish, kind, payload)
                },
            ))
    }
    fn registration_port(&self) -> Result<Option<crate::ActivationRegistrationPort>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .registration
            .clone())
    }
    fn activation(&self) -> Result<ActivationScope> {
        let state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        state.activation.clone().ok_or_else(|| {
            state.activation_admission_failure.as_ref().map_or_else(
                || failure("instance has no activation"),
                |primary| {
                    failure(format!(
                        "activation admission failed before cleanup scope creation: {primary}"
                    ))
                },
            )
        })
    }
    fn start(
        &self,
        operation: NativeOperation,
        invocation: Option<InvocationScope>,
        lifecycle: Option<bool>,
        admission: Option<AdmissionGuard>,
    ) -> Result<u64> {
        self.start_with_cancellation(
            operation,
            invocation,
            lifecycle,
            admission,
            ServiceCancellation::new(),
        )
    }
    /// Bind the host services a lifecycle hook receives from the selected
    /// lifecycle dispatch.
    ///
    /// Activation services outlive `activate` (background work retains them
    /// until deactivation begins), so they are built over the activation, not
    /// the lifecycle invocation; `deactivate` gets its own invocation's port.
    fn bind_lifecycle_host(
        &self,
        operation: &mut NativeOperation,
        scope: &InvocationScope,
        binding: &callbacks::CallbackBinding,
        host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
        kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
        ports: &mut InvocationPorts,
    ) -> Result<()> {
        let activation_dispatch = if matches!(operation, NativeOperation::Activate(_)) {
            let instance = self
                .host_dispatch
                .lock()
                .map_err(|_| failure("host dispatch poisoned"))?
                .clone();
            host.clone()
                .map(|entered| binding.activation_dispatch(entered, instance))
        } else {
            None
        };
        let activation_kernel = kernel.clone();
        ports.host = self.bind_host_port(scope, host)?;
        ports.kernel = kernel.map(|kernel| crate::InvocationKernelPort::new(scope.clone(), kernel));
        match operation {
            NativeOperation::Activate(context) => {
                let services = Self::activation_host_services_with(
                    &self.activation()?,
                    Some(scope),
                    activation_dispatch,
                    activation_kernel,
                );
                context.native.host_service_port = match self.registration_port()? {
                    Some(registration) => Some(
                        services
                            .unwrap_or_else(|| {
                                bmux_plugin_sdk::PluginHostServices::new(|_| {
                                    Err(failure("host services not bound"))
                                })
                            })
                            .with_registrations(move |mutation| {
                                apply_registration(&registration, mutation)
                            }),
                    ),
                    None => services,
                }
                .map(|services| self.with_event_route(services))
                .transpose()?;
            }
            NativeOperation::Deactivate(context) => {
                context.native.host_service_port =
                    self.decorate_host_services(ports.host.clone(), ports.kernel.clone())?;
            }
            _ => {
                return Err(failure(
                    "lifecycle callback factory received ordinary operation",
                ));
            }
        }
        Ok(())
    }

    fn start_with_cancellation(
        &self,
        mut operation: NativeOperation,
        invocation: Option<InvocationScope>,
        lifecycle: Option<bool>,
        admission: Option<AdmissionGuard>,
        cancellation: ServiceCancellation,
    ) -> Result<u64> {
        let mut ports = if let Some(scope) = &invocation {
            self.invocation_ports(scope, None)?
        } else {
            InvocationPorts {
                callback: None,
                host: None,
                kernel: None,
            }
        };
        if lifecycle.is_some()
            && let Some(scope) = &invocation
        {
            // A lifecycle root comes from this runtime's admitted scope, never
            // from context bytes or a reconstructed caller/origin grant.
            let supports_callback = match &self.backend {
                ExecutionBackend::Static(_) => true,
                ExecutionBackend::Dynamic { artifact, .. } => {
                    artifact.descriptor.capabilities
                        & abi::callback_authority::CAP_CALLBACK_AUTHORITY_V1
                        != 0
                }
            };
            if !supports_callback
                && self
                    .lifecycle_callback_factory
                    .lock()
                    .map_err(|_| failure("lifecycle callback factory poisoned"))?
                    .is_some()
            {
                return Err(failure(
                    "native lifecycle callback authority not negotiated",
                ));
            }
            if supports_callback {
                let execution = self
                    .state
                    .lock()
                    .map_err(|_| failure("execution state poisoned"))?
                    .retained
                    .clone()
                    .ok_or_else(|| failure("lifecycle execution no longer retained"))?;
                let mut binding =
                    execution.bind_root_callback(scope.clone(), cancellation.clone())?;
                let factory = self
                    .lifecycle_callback_factory
                    .lock()
                    .map_err(|_| failure("lifecycle callback factory poisoned"))?
                    .clone();
                if let Some(factory) = factory {
                    let selected = factory(&binding)?;
                    if let Some(nested) = selected.callback {
                        nested.validate()?;
                        if nested.invocation().id() != scope.id()
                            || nested
                                .execution()
                                .is_none_or(|owner| !Arc::ptr_eq(owner, &execution))
                        {
                            return Err(failure(
                                "lifecycle factory returned foreign callback binding",
                            ));
                        }
                        binding = nested;
                    }
                    self.bind_lifecycle_host(
                        &mut operation,
                        scope,
                        &binding,
                        selected.host,
                        selected.kernel,
                        &mut ports,
                    )?;
                }
                ports.callback = Some(binding);
            }
        }
        self.start_with_ports(
            operation,
            invocation,
            lifecycle,
            admission,
            OperationDispatch {
                cancellation,
                ports,
            },
        )
    }

    fn start_with_ports(
        &self,
        operation: NativeOperation,
        invocation: Option<InvocationScope>,
        lifecycle: Option<bool>,
        admission: Option<AdmissionGuard>,
        dispatch: OperationDispatch,
    ) -> Result<u64> {
        let OperationDispatch {
            cancellation,
            ports,
        } = dispatch;
        let InvocationPorts {
            host,
            kernel,
            callback,
        } = ports;
        let deadline = match &operation {
            NativeOperation::Invoke { context, .. } => context.cancellation.deadline_ms,
            _ => None,
        };
        let table = if let (ExecutionBackend::Dynamic { artifact, .. }, Some(invocation)) =
            (&self.backend, &invocation)
        {
            if callback.is_some()
                && artifact.descriptor.capabilities
                    & abi::callback_authority::CAP_CALLBACK_AUTHORITY_V1
                    == 0
            {
                return Err(failure("native callback authority not negotiated"));
            }
            if host.is_some() && artifact.descriptor.capabilities & abi::CAP_HOST_SERVICES == 0 {
                return Err(failure("host services not negotiated"));
            }
            let port = if let NativeOperation::Activate(context) = &operation
                && let Some(host) = &context.native.host_service_port
            {
                if artifact.descriptor.capabilities & abi::CAP_RETAINED_LIFECYCLE == 0 {
                    return Err(failure("retained lifecycle host services not negotiated"));
                }
                ports::HostPorts::for_activation(
                    invocation.clone(),
                    self.activation()?,
                    host.clone(),
                )
            } else {
                ports::HostPorts::new(invocation.clone(), None, host.clone())
            };
            let port = if matches!(&operation, NativeOperation::Activate(_)) {
                if artifact.descriptor.capabilities & abi::CAP_ASYNC_ACTIVATION == 0 {
                    return Err(failure("native async activation not negotiated"));
                }
                port.with_async_activation()
            } else {
                port
            };
            Some(
                port.with_callback(callback.clone())
                    .with_retained_execution(self.retained_execution()?)
                    .install_for(artifact.descriptor.version, kernel.clone())?,
            )
        } else {
            None
        };
        let host_services = self.decorate_host_services(host, kernel)?;
        let future = match &self.backend {
            ExecutionBackend::Static(implementation) => Some(static_operation_with_tasks(
                implementation.clone(),
                operation.clone(),
                cancellation.clone(),
                host_services,
                Some(self.background.clone()),
                callback.map(|binding| binding.context()),
            )),
            ExecutionBackend::Dynamic { .. } => None,
        };
        self.insert_call(CallStart {
            future,
            cancellation,
            invocation,
            lifecycle,
            admission,
            operation: Some(operation),
            stream_failure: None,
            deadline_ms: deadline,
            table,
        })
    }
    fn reserve_call(&self, call: &Arc<Call>) -> Result<u64> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        let admission_error = if state.destroyed || state.calls.len() >= abi::MAX_OPERATIONS {
            Some("execution closed or outcome capacity exhausted")
        } else if call.invocation.as_ref().is_some_and(|invocation| {
            state.calls.values().any(|existing| {
                existing
                    .invocation
                    .as_ref()
                    .is_some_and(|scope| scope.id() == invocation.id())
            })
        }) {
            Some("invocation already owns an operation")
        } else {
            None
        };
        if let Some(error) = admission_error {
            // No native start has run: remove the unpublished callback table
            // rather than leak its admission when concurrent reservation loses.
            if let Some(table) = &call.ports {
                ports::HostPorts::retire(table)?;
            }
            return Err(failure(error));
        }
        let id = state.next;
        state.next = id
            .checked_add(1)
            .ok_or_else(|| failure("operation identity exhausted"))?;
        state.calls.insert(id, call.clone());
        drop(state);
        Ok(id)
    }

    fn insert_call(&self, start: CallStart) -> Result<u64> {
        let CallStart {
            future,
            cancellation,
            invocation,
            lifecycle,
            admission,
            operation,
            stream_failure,
            deadline_ms,
            mut table,
        } = start;
        let registration = self.registration_port()?;
        if let Some(table) = &mut table {
            if registration.is_some() {
                if let ExecutionBackend::Dynamic { artifact, .. } = &self.backend
                    && artifact.descriptor.capabilities & abi::CAP_ACTIVATION_REGISTRATIONS == 0
                {
                    return Err(failure("activation registration capability not negotiated"));
                }
                table.capabilities |= abi::CAP_ACTIVATION_REGISTRATIONS;
            }
            ports::HostPorts::bind_registration(table, registration)?;
        }
        let call = Arc::new(Call {
            state: Mutex::new(CallState::Pending { future, admission }),
            cancellation,
            invocation,
            lifecycle,
            finalizing: AtomicBool::new(false),
            acknowledging: AtomicBool::new(false),
            stream_failure,
            ports: table,
            native_acknowledged: AtomicBool::new(false),
            invocation_retired: Arc::new(AtomicBool::new(false)),
            observer: Mutex::new(None),
            contended: Mutex::new(Vec::new()),
            deadline_armed: AtomicBool::new(false),
            deadline: deadline_ms.and_then(|milliseconds| {
                std::time::Instant::now()
                    .checked_add(std::time::Duration::from_millis(milliseconds))
            }),
        });
        let id = self.reserve_call(&call)?;
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            let result = operation
                .ok_or_else(|| failure("missing native operation"))
                .and_then(|operation| encode_bounded(&operation))
                .and_then(|input| {
                    start_status(unsafe {
                        call.ports.as_ref().map_or_else(
                            || {
                                (artifact.descriptor.start)(
                                    *handle,
                                    id,
                                    input.as_ptr(),
                                    input.len(),
                                )
                            },
                            |table| {
                                (artifact.descriptor.start_scoped)(
                                    *handle,
                                    id,
                                    input.as_ptr(),
                                    input.len(),
                                    table.as_ptr(),
                                )
                            },
                        )
                    })
                });
            if let Err(error) = result {
                // Never retry a possibly admitted mutation with a fresh identity.
                *call
                    .state
                    .lock()
                    .map_err(|_| failure("operation state poisoned"))? =
                    CallState::Failed(error.to_string());
                self.scope
                    .fail_cleanup(error.to_string())
                    .map_err(scope_error)?;
                return Err(error);
            }
        }
        Ok(id)
    }

    /// Polls one admitted operation, without holding a mutex across author code.
    ///
    /// `Pending` always arranges a wake of `context`'s waker: by the operation
    /// itself (static futures, `CAP_HOST_WAKE_V1` guests), by release of a
    /// concurrent poll, by cancellation or an explicit call deadline, or (for
    /// legacy native guests only) by the ABI-mandated periodic repoll.
    ///
    /// # Errors
    /// The operation is unknown, author execution fails, or cleanup cannot complete.
    pub fn poll(&self, id: u64, context: &mut Context<'_>) -> Result<Poll<NativeOutcome>> {
        let call = self.call(id)?;
        call.observe(context.waker());
        let polled = self.poll_call(&call, id, context);
        if matches!(polled, Ok(Poll::Pending)) {
            Self::arm_pending_wakes(&call)?;
        }
        polled
    }

    fn arm_pending_wakes(call: &Arc<Call>) -> Result<()> {
        if let Some(deadline) = call.deadline
            && !call.deadline_armed.swap(true, Ordering::AcqRel)
        {
            wake::wake_at(
                deadline,
                Waker::from(Arc::new(DeadlineWake(Arc::downgrade(call)))),
            )?;
        }
        Ok(())
    }

    fn poll_call(
        &self,
        call: &Arc<Call>,
        id: u64,
        context: &mut Context<'_>,
    ) -> Result<Poll<NativeOutcome>> {
        let (future, admission) = {
            let mut state = call
                .state
                .lock()
                .map_err(|_| failure("operation state poisoned"))?;
            match &*state {
                CallState::Ready(outcome) => return Ok(Poll::Ready(outcome.clone())),
                CallState::Failed(error) => return Err(failure(error.clone())),
                CallState::Polling => {
                    // Registered under the state lock: the poller releases the
                    // slot under the same lock before waking contenders.
                    call.contended
                        .lock()
                        .map_err(|_| failure("operation contention poisoned"))?
                        .push(context.waker().clone());
                    return Ok(Poll::Pending);
                }
                CallState::Pending { .. } => {}
            }
            match std::mem::replace(&mut *state, CallState::Polling) {
                CallState::Pending { future, admission } => (future, admission),
                _ => return Err(failure("invalid operation state")),
            }
        };
        let _release = ReleaseContended(call);
        let mut future = future;
        if call
            .deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            call.cancellation.cancel();
        }
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<Poll<NativeOutcome>> {
            if let Some(future) = &mut future {
                // Bundled plug-rt spawns made while polling join this scope.
                let _scope = self.background.enter();
                return Ok(future.as_mut().poll(context));
            }
            let ExecutionBackend::Dynamic { artifact, handle } = &self.backend else {
                return Err(failure("missing static future"));
            };
            if call.cancellation.is_cancelled() {
                status(unsafe { (artifact.descriptor.cancel)(*handle, id) })?;
            }
            // The guest retains its own references; ours ends with this read.
            let waker = abi::HostWaker::new(context.waker());
            let bytes = read_bounded(|out, length| unsafe {
                (artifact.read_wake)(
                    *handle,
                    id,
                    out.as_mut_ptr(),
                    out.len(),
                    length,
                    waker.as_ptr(),
                )
            })?;
            bytes.map_or(Ok(Poll::Pending), |bytes| {
                serde_json::from_slice(&bytes)
                    .map(Poll::Ready)
                    .map_err(|e| failure(e.to_string()))
            })
        }));
        let mut result = result
            .unwrap_or_else(|_| Err(failure("author operation panicked; cleanup required")))
            .and_then(|polled| match polled {
                Poll::Ready(outcome) if !outcome_is_bounded(&outcome) => {
                    Err(failure("final outcome exceeds byte budget"))
                }
                value => Ok(value),
            });
        if !matches!(&result, Ok(Poll::Pending)) {
            // Close callback admission first. Keep the operation admission alive
            // through author future destruction, including a panicking `Drop`.
            if let Some(invocation) = &call.invocation
                && !call.invocation_retired.load(Ordering::Acquire)
                && let Err(error) = invocation.close()
            {
                result = Err(scope_error(error));
            }
            if destroy_completed_future(&mut future).is_err() {
                // A panicked destructor cannot be rerun safely. Retain the
                // execution and its admission as explicit unrepaired evidence.
                if let Some(admission) = admission {
                    std::mem::forget(admission);
                }
                let error = "completed author future destructor panicked; cleanup required";
                self.scope.fail_cleanup(error.into()).map_err(scope_error)?;
                *call
                    .state
                    .lock()
                    .map_err(|_| failure("operation state poisoned"))? =
                    CallState::Failed(error.into());
                return Err(failure(error));
            }
        }
        self.settle_poll(call, result, future, admission, context)
    }

    fn settle_poll(
        &self,
        call: &Call,
        result: Result<Poll<NativeOutcome>>,
        future: Option<PluginFuture<'static, NativeOutcome>>,
        admission: Option<AdmissionGuard>,
        context: &Context<'_>,
    ) -> Result<Poll<NativeOutcome>> {
        let next = match &result {
            Ok(Poll::Pending) => CallState::Pending { future, admission },
            Ok(Poll::Ready(outcome)) => {
                drop(admission);
                if let Some(invocation) = &call.invocation {
                    if !call.invocation_retired.load(Ordering::Acquire) {
                        invocation.close().map_err(scope_error)?;
                    }
                    if !call.finalizing.swap(true, Ordering::AcqRel) {
                        let invocation = invocation.clone();
                        let outcome = outcome.clone();
                        let stream_failure = call.stream_failure.clone();
                        let lifecycle = call.lifecycle;
                        let retired = call.invocation_retired.clone();
                        let future = Box::pin(async move {
                            let drained = invocation.drain().await.and_then(|()| {
                                if lifecycle.is_some() {
                                    invocation.finish()?;
                                    retired.store(true, Ordering::Release);
                                }
                                Ok(())
                            });
                            match drained {
                                Ok(())
                                    if stream_failure
                                        .as_ref()
                                        .is_some_and(|failed| failed.load(Ordering::Acquire)) =>
                                {
                                    NativeOutcome::Error {
                                        code: 1,
                                        message: "stream callback delivery failed".into(),
                                    }
                                }
                                Ok(()) => outcome,
                                Err(error) => NativeOutcome::Error {
                                    code: 1,
                                    message: error.to_string(),
                                },
                            }
                        });
                        *call
                            .state
                            .lock()
                            .map_err(|_| failure("operation state poisoned"))? =
                            CallState::Pending {
                                future: Some(future),
                                admission: None,
                            };
                        // Every event callback has closed admission before final
                        // delivery; the final is withheld until admitted sinks drain.
                        context.waker().wake_by_ref();
                        return Ok(Poll::Pending);
                    }
                }
                if call.lifecycle == Some(true) && matches!(outcome, NativeOutcome::Unit) {
                    self.activation()?.mark_active().map_err(scope_error)?;
                    if let Some(port) = self.registration_port()? {
                        port.publish().map_err(|e| failure(e.to_string()))?;
                    }
                } else if call.lifecycle.is_some() && !matches!(outcome, NativeOutcome::Unit) {
                    self.scope
                        .fail_cleanup("lifecycle operation failed".into())
                        .map_err(scope_error)?;
                }
                CallState::Ready(outcome.clone())
            }
            Err(error) => {
                drop(admission);
                self.scope
                    .fail_cleanup(error.to_string())
                    .map_err(scope_error)?;
                CallState::Failed(error.to_string())
            }
        };
        *call
            .state
            .lock()
            .map_err(|_| failure("operation state poisoned"))? = next;
        result
    }
    fn call(&self, id: u64) -> Result<Arc<Call>> {
        self.state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .get(&id)
            .cloned()
            .ok_or_else(|| failure("unknown operation identity"))
    }
    ///
    /// # Errors
    /// The operation is unknown or native cancellation fails.
    pub fn cancel(&self, id: u64) -> Result<()> {
        let call = self.call(id)?;
        call.cancellation.cancel();
        // Wake the actual authority observed by gates, native subscriptions and
        // queued executors. Cancellation is not settlement or acknowledgement.
        // Lifecycle finalization can retire its root before the retained outcome
        // is acknowledged. Cancellation must not close that removed scope or
        // replace the authoritative failure with a cleanup-side Closed error.
        if let Some(invocation) = &call.invocation
            && !call.invocation_retired.load(Ordering::Acquire)
        {
            invocation.close().map_err(scope_error)?;
        }
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            status(unsafe { (artifact.descriptor.cancel)(*handle, id) })?;
        }
        // Cooperative authors may only check the flag when polled: cancellation
        // is host-side progress, so re-poll the operation through its observer.
        call.wake_observer();
        Ok(())
    }
    /// Final outcomes remain retrievable until this explicit acknowledgement.
    ///
    /// # Errors
    /// The outcome is not retireable or draining and acknowledgement fail.
    pub async fn acknowledge(&self, id: u64) -> Result<()> {
        let call = self.call(id)?;
        // Ordinary outcomes retire independently; lifecycle acknowledgement still
        // excludes all admission while it changes activation ownership.
        let _lifecycle = if call.lifecycle.is_some() {
            Some(self.control()?)
        } else {
            None
        };
        let _operation = if call.lifecycle.is_none() {
            Some(self.operation_control()?)
        } else {
            None
        };
        call.acknowledging
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| failure("operation acknowledgement already in progress"))?;
        let _acknowledging = ControlGuard(&call.acknowledging);
        // A concurrent observer may have fetched this Arc before the first
        // acknowledgement removed it; never acknowledge native state twice.
        if !Arc::ptr_eq(&self.call(id)?, &call) {
            return Err(failure("operation acknowledgement identity changed"));
        }
        if !matches!(&*call.state.lock().map_err(|_| failure("operation state poisoned"))?, CallState::Ready(outcome)
            if call.lifecycle.is_none() || matches!(outcome, NativeOutcome::Unit))
        {
            return Err(failure(
                "operation pending or failed lifecycle requires repaired-cleanup acknowledgement",
            ));
        }
        if call.lifecycle == Some(false)
            && self.scope.state().map_err(scope_error)?
                == bmux_plugin_runtime::LifecycleState::CleanupRequired
        {
            return Err(failure(
                "use acknowledge_repaired_cleanup after successful deactivation",
            ));
        }
        if let Some(invocation) = &call.invocation
            && !call.invocation_retired.load(Ordering::Acquire)
        {
            invocation.close().map_err(scope_error)?;
            invocation.drain().await.map_err(scope_error)?;
        }
        if call.lifecycle != Some(true)
            && let Some(table) = &call.ports
        {
            ports::HostPorts::retire(table)?;
        }
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            status(unsafe { (artifact.descriptor.ack)(*handle, id) })?;
        }
        if call.lifecycle == Some(true)
            && let Some(table) = &call.ports
        {
            self.state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?
                .lifecycle_transport = Some(LifecycleTransport { table: *table });
        }
        if let Some(invocation) = &call.invocation
            && !call.invocation_retired.load(Ordering::Acquire)
        {
            invocation.finish().map_err(scope_error)?;
        }
        if call.lifecycle == Some(false) {
            if let Some(port) = self.registration_port()? {
                port.withdraw_and_drain()
                    .await
                    .map_err(|e| failure(e.to_string()))?;
            }
            let transport = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?
                .lifecycle_transport
                .as_ref()
                .map(|transport| transport.table);
            if let Some(table) = transport {
                ports::HostPorts::retire(&table)?;
                self.state
                    .lock()
                    .map_err(|_| failure("execution state poisoned"))?
                    .lifecycle_transport = None;
            }
            self.activation()?.finish().map_err(scope_error)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?;
            state.activation = None;
            state.registration = None;
        }
        self.state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .remove(&id);
        Ok(())
    }
    /// Explicit host acknowledgement of repaired lifecycle cleanup. All failed
    /// lifecycle outcomes remain readable until successful deactivation, child
    /// retirement, and the runtime's resource/admission checks have succeeded.
    /// Panics/transport failures (`CallState::Failed`) are not repair evidence.
    ///
    /// # Errors
    /// Successful deactivation or retirement of all retained work cannot be established.
    pub async fn acknowledge_repaired_cleanup(&self, deactivation: u64) -> Result<()> {
        let _control = self.control()?;
        let calls = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .calls
            .iter()
            .map(|(id, call)| (*id, call.clone()))
            .collect::<Vec<_>>();
        if !calls.iter().any(|(id, call)| {
            *id == deactivation
                && call.lifecycle == Some(false)
                && matches!(
                    call.state.lock().as_deref(),
                    Ok(CallState::Ready(NativeOutcome::Unit))
                )
        }) {
            return Err(failure("successful terminal deactivation is required"));
        }
        for (id, call) in &calls {
            if *id != deactivation
                && !(call.lifecycle.is_some()
                    && matches!(call.state.lock().as_deref(),
                Ok(CallState::Ready(outcome)) if !matches!(outcome, NativeOutcome::Unit)))
            {
                return Err(failure("unretired work prevents cleanup acknowledgement"));
            }
            if call.invocation.is_some() && !call.invocation_retired.load(Ordering::Acquire) {
                return Err(failure(
                    "invocation must retire before cleanup acknowledgement",
                ));
            }
        }
        // No failed outcome is discarded if an activation-owned resource remains.
        let activation = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .activation
            .clone();
        let deadline = bmux_plugin_runtime::teardown_deadline();
        if let Some(activation) = activation {
            if let Some(port) = self.registration_port()? {
                port.withdraw_and_drain()
                    .await
                    .map_err(|e| failure(e.to_string()))?;
            }
            activation.close().map_err(scope_error)?;
            drain_activation(&activation, deadline).await?;
            activation.finish().map_err(scope_error)?;
            self.state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?
                .activation = None;
        }
        // Drain records settled task ownership even while reporting the retained
        // lifecycle failure. Only explicit resolve_cleanup clears that failure.
        if let Err(error) = self.scope.drain_until(deadline).await
            && !matches!(error, bmux_plugin_runtime::ScopeError::Cleanup(_))
        {
            return Err(scope_error(error));
        }
        if self.scope.state().map_err(scope_error)?
            == bmux_plugin_runtime::LifecycleState::CleanupRequired
        {
            self.scope.resolve_cleanup().map_err(scope_error)?;
        }
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            for (id, call) in &calls {
                if !call.native_acknowledged.load(Ordering::Acquire) {
                    if let Some(table) = &call.ports {
                        ports::HostPorts::retire(table)?;
                    }
                    status(unsafe { (artifact.descriptor.ack)(*handle, *id) })?;
                    call.native_acknowledged.store(true, Ordering::Release);
                }
            }
        }
        let transport = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?
            .lifecycle_transport
            .as_ref()
            .map(|transport| transport.table);
        if let Some(table) = transport {
            ports::HostPorts::retire(&table)?;
            self.state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?
                .lifecycle_transport = None;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        for (id, _) in calls {
            state.calls.remove(&id);
        }
        drop(state);
        Ok(())
    }

    /// Explicit retirement only after deactivation and outcome acknowledgement.
    ///
    /// # Errors
    /// Work remains outstanding or backend destruction fails.
    pub async fn destroy(&self) -> Result<()> {
        let _control = self.control()?;
        if retained::has_owner(self)? {
            return Err(failure(
                "retained host cleanup outstanding; execution image retained",
            ));
        }
        {
            let state = self
                .state
                .lock()
                .map_err(|_| failure("execution state poisoned"))?;
            if state.destroyed {
                return Ok(());
            }
            if state.activation.is_some()
                || state.lifecycle_transport.is_some()
                || !state.calls.is_empty()
            {
                return Err(failure("execution still owns activation or outcomes"));
            }
        }
        let dispatch = self
            .retained_dispatch
            .lock()
            .map_err(|_| failure("retained dispatch poisoned"))?
            .clone();
        if let Some(dispatch) = dispatch {
            if !dispatch.release()? {
                return Err(failure("retained activation owner cleanup pending"));
            }
            self.retained_dispatch
                .lock()
                .map_err(|_| failure("retained dispatch poisoned"))?
                .take();
        }
        self.scope.close().map_err(scope_error)?;
        self.scope
            .drain_until(bmux_plugin_runtime::teardown_deadline())
            .await
            .map_err(scope_error)?;
        // Abort work still owned by the activation scope (plugin futures are
        // dropped through the plugin's own drop function), wait for blocking
        // jobs until the deadline and report stragglers, then stop resolving
        // the instance's plug-rt scope before releasing the instance.
        self.background
            .abort_and_settle(bmux_plugin_runtime::teardown_deadline())
            .await;
        if let ExecutionBackend::Dynamic { artifact, handle } = &self.backend {
            if let Some(plug_rt) = artifact.plug_rt {
                plug_rt.unbind_scope(*handle);
            }
            // The guest owns no runtime: destruction completes synchronously.
            status(unsafe { (artifact.descriptor.destroy)(*handle) })?;
        }
        self.scope.finish().map_err(scope_error)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| failure("execution state poisoned"))?;
        state.destroyed = true;
        state.retained.take();
        drop(state);
        let owner = self
            .callback_runtime_owner
            .lock()
            .map_err(|_| failure("callback runtime owner poisoned"))?
            .take();
        let revocable = self
            .callback_runtime_pin
            .lock()
            .map_err(|_| failure("callback runtime pin poisoned"))?
            .take();
        // The drained, finished instance scope admits no callback entry, so
        // retained bindings no longer need the reactor. Revoke the shared pin
        // and drop the actual owner outside all loader locks.
        let released = revocable.map(|pin| pin.revoke()).transpose()?;
        drop(owner);
        drop(released);
        Ok(())
    }
}
struct OperationControlGuard<'a>(&'a AtomicUsize);
impl Drop for OperationControlGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ControlGuard<'a>(&'a AtomicBool);
impl Drop for ControlGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct ScopedEventSink {
    invocation: InvocationScope,
    identity: PluginInvocationId,
    downstream: ServiceEventSinkHandle,
    remaining: Mutex<(usize, usize)>,
    failed: Arc<AtomicBool>,
}
impl ServiceEventSink for ScopedEventSink {
    fn emit_event(&self, mut event: ServiceEvent) -> Result<()> {
        let _admission = self.invocation.try_admit().map_err(scope_error)?;
        let fail = || {
            self.failed.store(true, Ordering::Release);
        };
        // try_lock is deliberate: concurrent/reentrant publication reports
        // backpressure, never waits for a callback that might need its parent.
        let mut remaining = self.remaining.try_lock().map_err(|_| {
            fail();
            failure("stream callback busy; explicit backpressure")
        })?;
        if remaining.0 == 0 || event.payload.len() > remaining.1 {
            fail();
            return Err(failure("stream event count/byte budget exhausted"));
        }
        remaining.0 -= 1;
        remaining.1 -= event.payload.len();
        drop(remaining);
        event.invocation_id = self.identity.clone();
        let result = self.downstream.emit_event(event);
        if result.is_err() {
            fail();
        }
        result
    }
}

fn apply_registration(
    port: &crate::ActivationRegistrationPort,
    mutation: bmux_plugin_sdk::definition::RegistrationMutation,
) -> Result<Vec<u8>> {
    use bmux_plugin_sdk::definition::RegistrationMutation;
    mutation.validate().map_err(|e| failure(e.message))?;
    match mutation {
        RegistrationMutation::Register { contract, value } => {
            let id = port
                .register(contract, value, None)
                .map_err(|e| failure(e.to_string()))?;
            encode_bounded(&id)
        }
        RegistrationMutation::SetRetained {
            contract,
            expected_revision,
            value,
        } => encode_bounded(
            &port
                .replace(&contract, expected_revision, value)
                .map_err(|e| failure(e.to_string()))?,
        ),
        RegistrationMutation::Emit { contract, value } => encode_bounded(
            &port
                .emit(&contract, value)
                .map_err(|e| failure(e.to_string()))?,
        ),
        RegistrationMutation::MarkReady { contract } => encode_bounded(
            &port
                .mark_ready(&contract)
                .map_err(|e| failure(e.to_string()))?,
        ),
    }
}

fn destroy_completed_future(
    future: &mut Option<PluginFuture<'static, NativeOutcome>>,
) -> Result<()> {
    let completed = future.take();
    catch_unwind(AssertUnwindSafe(|| drop(completed)))
        .map_err(|_| failure("author future destruction panicked"))
}

fn reject_executable_context(present: bool) -> Result<()> {
    if present {
        Err(failure(
            "serialized executable host bridge is forbidden in scoped execution; bind a host port",
        ))
    } else {
        Ok(())
    }
}
fn artifact_version(backend: &ExecutionBackend) -> u32 {
    match backend {
        ExecutionBackend::Dynamic { artifact, .. } => artifact.descriptor.version,
        ExecutionBackend::Static(_) => abi::ABI_VERSION,
    }
}

/// A plugin that declared `bundled_only` needs host facilities plug-rt does
/// not proxy into dynamic libraries; refuse it before any instance exists.
fn refuse_bundled_only(plugin_id: &str, bundled_only: Option<&str>) -> Result<()> {
    bundled_only.map_or(Ok(()), |reason| {
        Err(PluginError::BundledOnlyPlugin {
            plugin_id: plugin_id.to_string(),
            reason: reason.to_string(),
        })
    })
}

fn validate_native_service_contracts(
    descriptors: &[bmux_plugin_sdk::definition::DeclaredServiceContract],
) -> Result<()> {
    let mut identities = std::collections::BTreeSet::new();
    for descriptor in descriptors {
        descriptor
            .validate()
            .map_err(|error| failure(error.to_string()))?;
        if !identities.insert((
            descriptor.capability.as_str(),
            descriptor.kind,
            descriptor.interface_id.as_str(),
        )) {
            return Err(failure("duplicate native modeled service contract"));
        }
    }
    Ok(())
}

fn validate_native_checked_service(
    descriptors: &[bmux_plugin_sdk::definition::DeclaredServiceContract],
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<()> {
    let service = &context.request.service;
    let evidence = descriptors
        .iter()
        .find(|descriptor| {
            descriptor.capability == service.capability.as_str()
                && descriptor.kind == service.kind
                && descriptor.interface_id == service.interface_id
        })
        .ok_or_else(|| {
            failure("selected native artifact has no exact modeled contract for checked route")
        })?;
    bmux_plugin_sdk::service_contract::decode_for(
        &service.interface_id,
        &evidence.contract,
        &context.request.payload,
    )?;
    Ok(())
}

// Static evidence comes from the actual selected implementation (including
// explicit alias routes). Dynamic evidence comes only from that retained
// artifact's validated v3 export, never host discovery or compatibility hints.
fn validate_checked_service(
    backend: &ExecutionBackend,
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<()> {
    use bmux_plugin_sdk::service_contract;
    let payload = &context.request.payload;
    if !service_contract::is_checked(payload) {
        return Ok(());
    }
    let interface = context.request.service.interface_id.as_str();
    service_contract::interface_id_for(interface, payload)?;
    let implementation = match backend {
        ExecutionBackend::Static(implementation) => implementation,
        ExecutionBackend::Dynamic { artifact, .. } => {
            return validate_native_checked_service(&artifact.service_contracts, context);
        }
    };
    let evidence = catch_unwind(AssertUnwindSafe(|| {
        implementation.service_contract_for(interface)
    }))
    .map_err(|_| failure("provider contract lookup panicked"))?
    .ok_or_else(|| failure("selected provider has no generated contract for checked interface"))?;
    // Validate only: generated provider dispatch must receive the original
    // canonical checked envelope, not an unchecked payload or rewritten route.
    service_contract::decode_for(interface, evidence, payload)?;
    Ok(())
}

fn reject_operation_executable_fields(operation: &NativeOperation) -> Result<()> {
    let present = match operation {
        NativeOperation::Activate(context) | NativeOperation::Deactivate(context) => {
            context.native.host_kernel_bridge.is_some()
        }
        NativeOperation::Command { context, .. } => context.host_kernel_bridge.is_some(),
        NativeOperation::Invoke { context, .. } | NativeOperation::Streaming { context, .. } => {
            context.host_kernel_bridge.is_some()
        }
        _ => false,
    };
    reject_executable_context(present)
}

fn validate_operation_limits(operation: &NativeOperation) -> Result<()> {
    // The bound is a shared admission contract, not static transport. Static
    // dispatch still receives Rust values directly and does not roundtrip bytes.
    let bytes = match operation {
        NativeOperation::Invoke { context, .. } | NativeOperation::Streaming { context, .. } => {
            context.request.payload.len()
        }
        NativeOperation::Extension {
            contract, payload, ..
        } => contract.len().saturating_add(payload.len()),
        NativeOperation::Command { context, .. } => context.command.len().saturating_add(
            context
                .arguments
                .iter()
                .map(String::len)
                .fold(0_usize, usize::saturating_add),
        ),
        _ => 0,
    };
    if bytes > abi::MAX_BUFFER_BYTES {
        return Err(failure("operation payload exceeds byte budget"));
    }
    Ok(())
}

fn outcome_is_bounded(outcome: &NativeOutcome) -> bool {
    let bytes = match outcome {
        NativeOutcome::Registrations(registrations) => {
            serde_json::to_vec(registrations).map_or(usize::MAX, |bytes| bytes.len())
        }
        NativeOutcome::Unit => 0,
        NativeOutcome::Bytes(bytes) => bytes.len(),
        NativeOutcome::Command(outcome) => {
            serde_json::to_vec(outcome).map_or(usize::MAX, |bytes| bytes.len())
        }
        NativeOutcome::Error { message, .. } => message.len(),
        NativeOutcome::Service(response) => {
            response
                .payload
                .len()
                .saturating_add(response.error.as_ref().map_or(0, |error| {
                    error.code.len().saturating_add(error.message.len())
                }))
        }
    };
    bytes <= abi::MAX_BUFFER_BYTES
}

fn author_result<T>(
    value: bmux_plugin_sdk::definition::PluginResult<T>,
    success: impl FnOnce(T) -> NativeOutcome,
) -> NativeOutcome {
    match value {
        Ok(value) => success(value),
        Err(error) => NativeOutcome::Error {
            code: error.code,
            message: error.message,
        },
    }
}
#[cfg(test)]
fn static_operation(
    implementation: Arc<dyn PluginInstance>,
    operation: NativeOperation,
    cancellation: ServiceCancellation,
    host_services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
) -> PluginFuture<'static, NativeOutcome> {
    static_operation_with_tasks(
        implementation,
        operation,
        cancellation,
        host_services,
        None,
        None,
    )
}

fn static_operation_with_tasks(
    implementation: Arc<dyn PluginInstance>,
    operation: NativeOperation,
    cancellation: ServiceCancellation,
    host_services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
    tasks: Option<bmux_plugin_runtime::background::BackgroundTasks>,
    callback: Option<bmux_plugin_sdk::PluginCallbackContext>,
) -> PluginFuture<'static, NativeOutcome> {
    Box::pin(async move {
        let call = |invocation_id| PluginCallContext {
            callback_execution: callback.clone(),
            invocation_id,
            cancellation: cancellation.clone(),
            host_services: host_services.clone(),
        };
        match operation {
            NativeOperation::Activate(mut context) => {
                context.callback_execution = callback.clone();
                let result = if let Some(tasks) = &tasks {
                    implementation
                        .activate_scoped_async(
                            context,
                            host_services.clone(),
                            bmux_plugin_sdk::HostAsyncHandle::new(tasks.clone()),
                        )
                        .await
                } else {
                    implementation
                        .activate_scoped(context, host_services.clone())
                        .await
                };
                author_result(result, |()| NativeOutcome::Unit)
            }
            NativeOperation::Deactivate(mut context) => {
                context.callback_execution = callback.clone();
                if let Some(tasks) = &tasks
                    && let Err(error) = tasks.shutdown_retained(None).await
                {
                    return author_result(
                        Err(bmux_plugin_sdk::PluginCommandError::failed(error)),
                        |()| NativeOutcome::Unit,
                    );
                }
                author_result(
                    implementation
                        .deactivate_scoped(context, host_services.clone())
                        .await,
                    |()| NativeOutcome::Unit,
                )
            }
            NativeOperation::Command {
                invocation_id,
                mut context,
            } => {
                // Match the author call capability, never caller-supplied authority.
                context.host_service_port = host_services.clone();
                NativeOutcome::Command(implementation.command(call(invocation_id), context).await)
            }
            NativeOperation::Invoke {
                invocation_id,
                mut context,
            } => {
                // Never accept executable authority supplied in the operation.
                context.host_service_port = host_services.clone();
                author_result(
                    implementation.invoke(call(invocation_id), context).await,
                    NativeOutcome::Service,
                )
            }
            NativeOperation::Streaming { .. } => NativeOutcome::Error {
                code: 1,
                message: "streaming requires explicit sink admission".into(),
            },
            NativeOperation::Registrations => {
                author_result(implementation.registrations(), NativeOutcome::Registrations)
            }
            NativeOperation::Event(event) => author_result(
                implementation
                    .event_scoped(call(PluginInvocationId::new()), event)
                    .await,
                |()| NativeOutcome::Unit,
            ),
            NativeOperation::Extension {
                invocation_id,
                contract,
                payload,
            } => author_result(
                implementation
                    .extension(call(invocation_id), contract, payload)
                    .await,
                NativeOutcome::Bytes,
            ),
        }
    })
}
fn encode_bounded(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|e| failure(e.to_string()))?;
    if bytes.len() > abi::MAX_BUFFER_BYTES {
        return Err(failure("native input exceeds bounded buffer"));
    }
    Ok(bytes)
}
/// Re-read a retained result only; never use this helper around `start`.
fn read_bounded(mut read: impl FnMut(&mut [u8], &mut usize) -> i32) -> Result<Option<Vec<u8>>> {
    let mut bytes = vec![0; 4096.min(abi::MAX_BUFFER_BYTES)];
    for _ in 0..8 {
        let mut length = 0;
        match read(&mut bytes, &mut length) {
            abi::PENDING | abi::BUSY => return Ok(None),
            abi::OK => {
                if length > bytes.len() {
                    return Err(failure("native output length exceeds capacity"));
                }
                bytes.truncate(length);
                return Ok(Some(bytes));
            }
            abi::BUFFER_TOO_SMALL => {
                if length <= bytes.len() || length > abi::MAX_BUFFER_BYTES {
                    return Err(failure("native output growth invalid or exceeds limit"));
                }
                bytes.resize(length, 0);
            }
            code => return Err(failure(format!("native read status {code}"))),
        }
    }
    Err(failure(
        "native retained output exceeded bounded growth attempts",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bundled_only_declarations_refuse_dynamic_loading_with_a_typed_error() {
        let declarations: bmux_plugin_sdk::definition::PluginDeclarations =
            toml::from_str("bundled_only = \"opens host sockets directly\"\n").unwrap();
        let error =
            refuse_bundled_only("example.net", declarations.bundled_only.as_deref()).unwrap_err();
        assert!(matches!(
            &error,
            PluginError::BundledOnlyPlugin { plugin_id, reason }
                if plugin_id == "example.net" && reason == "opens host sockets directly"
        ));
        assert!(refuse_bundled_only("example.any", None).is_ok());
    }

    #[tokio::test]
    async fn unverified_static_definition_has_no_artifact_receipt() {
        let execution = fixture_unactivated();
        assert!(execution.verified_artifact().is_none());
        execution.destroy().await.unwrap();
    }

    // Requires a real native component with an embedded manifest, not the
    // legacy counter fixture (which deliberately has no embedded declaration).
    fn verified_native_fixture() -> (tempfile::TempDir, crate::SelectedPluginCandidate, String) {
        use sha2::{Digest, Sha256};
        let source = std::env::var_os("BMUX_VERIFIED_NATIVE_ARTIFACT").expect(
            "set BMUX_VERIFIED_NATIVE_ARTIFACT to a native component with embedded manifest",
        );
        let root = tempfile::tempdir().unwrap();
        let source = std::path::PathBuf::from(source);
        let entry = root.path().join(source.file_name().unwrap());
        std::fs::copy(&source, &entry).unwrap();
        let bytes = std::fs::read(&entry).unwrap();
        let digest = hex_digest(&Sha256::digest(&bytes));
        let bare = unsafe { DefinitionArtifact::load(&entry) }.unwrap();
        assert!(bare.verified_selection.is_none());
        let embedded = bare.metadata().manifest_toml.as_ref().unwrap();
        let mut manifest = crate::PluginManifest::from_toml_str(embedded).unwrap();
        let path = root.path().join("plugin.toml");
        std::fs::write(&path, embedded).unwrap();
        manifest.entry = Some(entry);
        manifest.artifact_sha256 = Some(digest.clone());
        let role = manifest.component_roles[0];
        let request = crate::PluginSelectionRequest {
            plugin_id: manifest.id.clone(),
            role,
            preference: crate::PluginCandidatePreference::Dynamic {
                manifest_path: path.clone(),
            },
            plugin_api: manifest.plugin_api.minimum.parse().unwrap(),
            native_abi: manifest.native_abi.minimum.parse().unwrap(),
            contracts: manifest.contracts.clone(),
        };
        let mut inventory = crate::PluginCandidateInventory::new();
        inventory
            .add_dynamic_manifest(root.path(), &path, manifest)
            .unwrap();
        (root, inventory.select(&request).unwrap(), digest)
    }

    #[tokio::test]
    #[ignore = "requires BMUX_VERIFIED_NATIVE_ARTIFACT with an embedded component manifest"]
    async fn verified_receipt_binds_retained_bytes_metadata_runtime_and_instance() {
        let (_root, selected, digest) = verified_native_fixture();
        let artifact = unsafe { DefinitionArtifact::load_selected(&selected) }.unwrap();
        let ArtifactBackend::Dynamic(owner) = &artifact.backend else {
            panic!("native required")
        };
        let version = owner.descriptor.version;
        let capabilities = owner.descriptor.capabilities;
        let runtime = PluginRuntimeScopeId::new();
        let available = artifact.metadata.required_capabilities.clone();
        let first = artifact
            .create(runtime, selected.role(), &available)
            .unwrap();
        let second = artifact
            .create(runtime, selected.role(), &available)
            .unwrap();
        let other = artifact
            .create(PluginRuntimeScopeId::new(), selected.role(), &available)
            .unwrap();
        let receipt = first.verified_artifact().unwrap();
        assert_eq!(receipt.sha256(), digest);
        assert_eq!(receipt.metadata(), artifact.metadata());
        assert_eq!(receipt.role(), selected.role());
        assert_eq!(receipt.native_abi_version(), version);
        assert_eq!(receipt.native_capabilities(), capabilities);
        assert_eq!(
            receipt.runtime_scope(),
            first.create_context().runtime_scope
        );
        assert_eq!(receipt.instance(), first.create_context().instance);
        assert_eq!(
            receipt.runtime_scope(),
            second.verified_artifact().unwrap().runtime_scope()
        );
        assert_ne!(
            receipt.instance(),
            second.verified_artifact().unwrap().instance()
        );
        assert_ne!(
            receipt.runtime_scope(),
            other.verified_artifact().unwrap().runtime_scope()
        );
        let source = selected.candidate().plugin.manifest.entry.as_ref().unwrap();
        let retained_path = owner
            .artifact_copy
            .as_ref()
            .unwrap()
            .path()
            .join(source.file_name().unwrap());
        {
            use sha2::{Digest, Sha256};
            assert_eq!(
                hex_digest(&Sha256::digest(std::fs::read(&retained_path).unwrap())),
                digest
            );
        }
        let wrong_role = match selected.role() {
            ComponentRole::Backend => ComponentRole::Frontend,
            ComponentRole::Frontend => ComponentRole::Backend,
        };
        assert!(artifact.create(runtime, wrong_role, &available).is_err());
        // Replace only this test's source, never the retained loaded copy.
        let replacement = source.with_extension("replacement");
        std::fs::write(&replacement, b"replacement is not the loaded artifact").unwrap();
        std::fs::rename(&replacement, source).unwrap();
        assert!(unsafe { DefinitionArtifact::load_selected(&selected) }.is_err());
        assert!(retained_path.is_file());
        drop(artifact);
        assert_eq!(first.verified_artifact().unwrap().sha256(), digest);
        first.destroy().await.unwrap();
        second.destroy().await.unwrap();
        other.destroy().await.unwrap();
    }

    #[test]
    #[ignore = "requires BMUX_VERIFIED_NATIVE_ARTIFACT with an embedded component manifest"]
    fn mismatched_selected_metadata_cannot_issue_artifact_receipt() {
        let (root, selected, _) = verified_native_fixture();
        let plugin = &selected.candidate().plugin;
        let mut manifest = plugin.manifest.clone();
        manifest.version = "999999.0.0".into();
        assert_ne!(manifest.version, plugin.manifest.version);
        let mut inventory = crate::PluginCandidateInventory::new();
        inventory
            .add_dynamic_manifest(root.path(), &plugin.manifest_path, manifest)
            .unwrap();
        let wrong = inventory
            .select(&crate::PluginSelectionRequest {
                plugin_id: plugin.manifest.id.clone(),
                role: selected.role(),
                preference: crate::PluginCandidatePreference::Dynamic {
                    manifest_path: plugin.manifest_path.clone(),
                },
                plugin_api: plugin.manifest.plugin_api.minimum.parse().unwrap(),
                native_abi: plugin.manifest.native_abi.minimum.parse().unwrap(),
                contracts: plugin.manifest.contracts.clone(),
            })
            .unwrap();
        assert!(unsafe { DefinitionArtifact::load_selected(&wrong) }.is_err());
    }

    #[tokio::test]
    #[ignore = "requires BMUX_VERIFIED_NATIVE_ARTIFACT with an embedded component manifest"]
    async fn bare_native_load_cannot_issue_verified_receipt() {
        let (_root, selected, _) = verified_native_fixture();
        let source = selected.candidate().plugin.manifest.entry.as_ref().unwrap();
        let artifact = unsafe { DefinitionArtifact::load(source) }.unwrap();
        let execution = artifact
            .create(
                PluginRuntimeScopeId::new(),
                selected.role(),
                &artifact.metadata.required_capabilities,
            )
            .unwrap();
        assert!(execution.verified_artifact().is_none());
        execution.destroy().await.unwrap();
    }

    #[test]
    fn ordinary_admissions_overlap_and_fence_lifecycle_without_global_serialization() {
        let execution = fixture();
        let outer = execution.operation_control().unwrap();
        let sibling = execution.operation_control().unwrap();
        assert!(execution.control().is_err());
        let invocation = execution.begin_invocation_scope().unwrap();
        let operation = execution
            .begin_with_dispatch(
                NativeOperation::Extension {
                    invocation_id: PluginInvocationId::new(),
                    contract: "test".into(),
                    payload: vec![],
                },
                ServiceCancellation::new(),
                invocation,
                None,
                None,
            )
            .unwrap();
        assert!(execution.call(operation).is_ok());
        drop(sibling);
        drop(outer);
        let lifecycle = execution.control().unwrap();
        assert!(execution.operation_control().is_err());
        drop(lifecycle);
        assert!(execution.operation_control().is_ok());
    }

    #[test]
    fn concurrent_same_scope_reservation_admits_exactly_one_operation() {
        let execution = fixture();
        let invocation = execution.begin_invocation_scope().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        // Spawn eagerly before entering the three-party barrier. A lazy
        // iterator here would wait forever before launching either worker.
        let mut workers = Vec::with_capacity(2);
        for _ in 0..2 {
            let execution = execution.clone();
            let invocation = invocation.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                execution.begin_with_dispatch(
                    NativeOperation::Extension {
                        invocation_id: PluginInvocationId::new(),
                        contract: "test".into(),
                        payload: vec![],
                    },
                    ServiceCancellation::new(),
                    invocation,
                    None,
                    None,
                )
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(execution.state.lock().unwrap().calls.len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_activation_uses_standard_async_author_hook() {
        struct AsyncAuthor(Arc<AtomicUsize>);
        impl PluginInstance for AsyncAuthor {
            fn activate_with_async(
                &self,
                _: PluginLifecycleContext,
                handle: bmux_plugin_sdk::HostAsyncHandle,
            ) -> PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>> {
                let seen = self.0.clone();
                Box::pin(async move {
                    handle
                        .spawn_background("static-probe", move |mut cancel| async move {
                            cancel.cancelled().await;
                            seen.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .map_err(bmux_plugin_sdk::PluginCommandError::failed)?;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    Ok(())
                })
            }
        }
        let execution = fixture_unactivated();
        let tasks = bmux_plugin_runtime::background::BackgroundTasks::new();
        let effects = Arc::new(AtomicUsize::new(0));
        let implementation = Arc::new(AsyncAuthor(effects.clone())) as Arc<dyn PluginInstance>;
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        execution.state.lock().unwrap().activation = Some(activation.clone());
        let invocation = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
            .unwrap();
        let callback = execution
            .bind_root_callback(invocation.clone(), ServiceCancellation::new())
            .unwrap()
            .context();
        let context = PluginLifecycleContext {
            callback_execution: Some(callback.clone()),
            scope: execution.context.clone(),
            activation: ActivationId::from_uuid(activation.id().as_uuid()),
            native: auxiliary_lifecycle_fixture(),
            capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
        };
        let outcome = static_operation_with_tasks(
            implementation,
            NativeOperation::Activate(context),
            ServiceCancellation::new(),
            None,
            Some(tasks.clone()),
            Some(callback),
        )
        .await;
        assert!(matches!(outcome, NativeOutcome::Unit));
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        tasks.shutdown_retained(None).await.unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        invocation.close().unwrap();
        invocation.finish().unwrap();
    }

    #[test]
    fn lifecycle_port_uses_setup_only_until_activation_settles() {
        struct Capture(Arc<Mutex<Vec<bmux_plugin_runtime::RuntimeInvocationId>>>);
        impl bmux_plugin_sdk::HostServiceDispatch for Capture {
            fn invoke(
                &self,
                invocation: &InvocationScope,
                _: bmux_plugin_sdk::HostServiceRequest<'_>,
            ) -> Result<Vec<u8>> {
                self.0.lock().unwrap().push(invocation.id());
                Ok(vec![])
            }
        }
        let execution = fixture_unactivated();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        let setup = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = LoadedExecution::activation_services(
            activation.clone(),
            Arc::new(Capture(calls.clone())),
            Some(setup.clone()),
        );
        let invoke = || {
            port.invoke(bmux_plugin_sdk::HostServiceRequest {
                capability: "test",
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface: "test/v1",
                operation: "read",
                payload: &[],
            })
        };
        invoke().unwrap();
        setup.close().unwrap();
        setup.finish().unwrap();
        assert!(invoke().is_err());
        activation.mark_active().unwrap();
        invoke().unwrap();
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls[0], setup.id());
            assert_ne!(calls[1], setup.id());
        }
        activation.close().unwrap();
        assert!(invoke().is_err());
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn activation_caller_mints_fresh_calls_and_rejects_closed_owner() {
        struct Capture(Arc<Mutex<Vec<bmux_plugin_runtime::RuntimeInvocationId>>>);
        impl bmux_plugin_sdk::HostServiceDispatch for Capture {
            fn invoke(
                &self,
                invocation: &InvocationScope,
                _: bmux_plugin_sdk::HostServiceRequest<'_>,
            ) -> Result<Vec<u8>> {
                self.0.lock().unwrap().push(invocation.id());
                Ok(vec![42])
            }
        }
        let execution = fixture();
        let activation = execution.activation().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = LoadedExecution::activation_owned_services(
            activation.clone(),
            Arc::new(Capture(calls.clone())),
        );
        for _ in 0..2 {
            assert_eq!(
                port.invoke(bmux_plugin_sdk::HostServiceRequest {
                    capability: "test",
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "test/v1",
                    operation: "read",
                    payload: &[],
                })
                .unwrap(),
                vec![42]
            );
        }
        {
            let calls = calls.lock().unwrap();
            assert_ne!(calls[0], calls[1]);
        }
        activation.close().unwrap();
        assert!(
            port.invoke(bmux_plugin_sdk::HostServiceRequest {
                capability: "test",
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface: "test/v1",
                operation: "read",
                payload: &[],
            })
            .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    struct Dispatch(u8, bmux_plugin_runtime::RuntimeInvocationId);

    impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
        fn invoke(
            &self,
            scope: &InvocationScope,
            _: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> Result<Vec<u8>> {
            assert_eq!(scope.id(), self.1);
            Ok(vec![self.0])
        }
    }
    impl crate::InvocationKernelDispatch for Dispatch {
        fn execute(&self, scope: &InvocationScope, _: &[u8]) -> Result<Vec<u8>> {
            assert_eq!(scope.id(), self.1);
            Ok(vec![self.0])
        }
    }
    fn explicit_dispatch_fixture() -> Arc<LoadedExecution> {
        struct Implementation;
        impl PluginInstance for Implementation {
            fn extension(
                &self,
                call: PluginCallContext,
                _: String,
                _: Vec<u8>,
            ) -> PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<Vec<u8>>> {
                Box::pin(async move {
                    let Some(services) = call.host_services else {
                        return Ok(vec![]);
                    };
                    let mut response = services
                        .invoke(bmux_plugin_sdk::HostServiceRequest {
                            capability: "test",
                            kind: bmux_plugin_sdk::ServiceKind::Query,
                            interface: "test",
                            operation: "test",
                            payload: &[],
                        })
                        .map_err(|error| {
                            bmux_plugin_sdk::PluginCommandError::failed(error.to_string())
                        })?;
                    response.extend(services.execute_kernel_encoded(&[]).map_err(|error| {
                        bmux_plugin_sdk::PluginCommandError::failed(error.to_string())
                    })?);
                    Ok(response)
                })
            }
        }
        let artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
            PluginMetadata {
                plugin_id: "test.explicit".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            |_| Ok(Arc::new(Implementation) as Arc<dyn PluginInstance>),
        ));
        // Fresh implementation instance with a real active scope tree.
        let execution = artifact
            .create(
                PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(23)),
                ComponentRole::Backend,
                &[],
            )
            .unwrap();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        execution.state.lock().unwrap().activation = Some(activation);
        execution
    }

    #[tokio::test]
    async fn explicit_dispatch_is_operation_local_without_singleton_fallback() {
        let execution = explicit_dispatch_fixture();
        let first = execution.begin_invocation_scope().unwrap();
        let second = execution.begin_invocation_scope().unwrap();
        let operation = || NativeOperation::Extension {
            invocation_id: PluginInvocationId::new(),
            contract: "test".into(),
            payload: vec![],
        };
        // Bind deliberately unusable ambient grants: explicit operations must
        // never consult them, including when both explicit grants are absent.
        *execution.host_dispatch.lock().unwrap() = Some(Arc::new(Dispatch(99, first.id())));
        *execution.kernel_dispatch.lock().unwrap() = Some(Arc::new(Dispatch(99, first.id())));
        let left = Arc::new(Dispatch(1, first.id()));
        let right = Arc::new(Dispatch(2, second.id()));
        let a = execution
            .begin_with_dispatch(
                operation(),
                ServiceCancellation::new(),
                first.clone(),
                Some(left.clone()),
                Some(left),
            )
            .unwrap();
        let b = execution
            .begin_with_dispatch(
                operation(),
                ServiceCancellation::new(),
                second,
                Some(right.clone()),
                Some(right),
            )
            .unwrap();
        assert!(
            execution
                .begin_with_dispatch(
                    operation(),
                    ServiceCancellation::new(),
                    first.clone(),
                    None,
                    None
                )
                .is_err()
        );
        let empty = execution.begin_invocation_scope().unwrap();
        let c = execution
            .begin_with_dispatch(operation(), ServiceCancellation::new(), empty, None, None)
            .unwrap();
        // Author completion first closes callback admission and schedules the
        // drain future. Final delivery must wait for the subsequent drain poll.
        for id in [b, a, c] {
            assert!(poll_fixture(&execution, id).unwrap().is_pending());
        }
        assert!(
            matches!(poll_fixture(&execution, b).unwrap(), Poll::Ready(NativeOutcome::Bytes(bytes)) if bytes == [2, 2])
        );
        assert!(
            matches!(poll_fixture(&execution, a).unwrap(), Poll::Ready(NativeOutcome::Bytes(bytes)) if bytes == [1, 1])
        );
        assert!(
            matches!(poll_fixture(&execution, c).unwrap(), Poll::Ready(NativeOutcome::Bytes(bytes)) if bytes.is_empty())
        );
        for id in [a, b, c] {
            execution.acknowledge(id).await.unwrap();
        }
        assert!(
            execution
                .begin_with_dispatch(operation(), ServiceCancellation::new(), first, None, None)
                .is_err()
        );
    }

    #[test]
    fn explicit_dispatch_rejects_foreign_runtime_and_instance_before_admission() {
        let execution = fixture();
        for runtime in [
            execution.scope.runtime_id(),
            PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(24)),
        ] {
            let foreign = InstanceScope::new(runtime);
            let activation = foreign
                .begin_activation(PluginConcurrencyConfig::Concurrent)
                .unwrap();
            activation.mark_active().unwrap();
            let invocation = activation.begin_invocation().unwrap();
            let next = execution.state.lock().unwrap().next;
            assert!(
                execution
                    .begin_with_dispatch(
                        NativeOperation::Extension {
                            invocation_id: PluginInvocationId::new(),
                            contract: "test".into(),
                            payload: vec![]
                        },
                        ServiceCancellation::new(),
                        invocation,
                        None,
                        None,
                    )
                    .is_err()
            );
            assert_eq!(execution.state.lock().unwrap().next, next);
        }
        let stale = execution.begin_invocation_scope().unwrap();
        let activation = execution.activation().unwrap();
        execution.state.lock().unwrap().activation = None;
        assert!(
            execution
                .begin_with_dispatch(
                    NativeOperation::Extension {
                        invocation_id: PluginInvocationId::new(),
                        contract: "test".into(),
                        payload: vec![]
                    },
                    ServiceCancellation::new(),
                    stale.clone(),
                    None,
                    None,
                )
                .is_err()
        );
        execution.state.lock().unwrap().activation = Some(activation);
        stale.close().unwrap();
        assert!(
            execution
                .begin_streaming_with_dispatch(
                    NativeStreamingServiceContext {
                        service: super::super::tests::native_test_service_context(vec![]),
                        events: ServiceEventSinkHandle::noop(),
                    },
                    ServiceCancellation::new(),
                    stale,
                    None,
                    None,
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn decorated_host_services_preserve_selected_async_dispatch() {
        struct AsyncDispatch;
        impl bmux_plugin_sdk::HostServiceDispatch for AsyncDispatch {
            fn invoke(
                &self,
                _: &InvocationScope,
                _: bmux_plugin_sdk::HostServiceRequest<'_>,
            ) -> Result<Vec<u8>> {
                panic!("async dispatch must not fall back to sync")
            }
            fn invoke_async(
                &self,
                _: &InvocationScope,
                request: bmux_plugin_sdk::HostServiceOwnedRequest,
            ) -> PluginFuture<'static, Result<Vec<u8>>> {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    Ok(request.payload)
                })
            }
        }
        let execution = fixture();
        let invocation = execution.activation().unwrap().begin_invocation().unwrap();
        let services = execution
            .decorate_host_services(
                Some(bmux_plugin_sdk::BoundHostServicePort::new(
                    invocation.clone(),
                    Arc::new(AsyncDispatch),
                )),
                None,
            )
            .unwrap()
            .unwrap();
        let request = || bmux_plugin_sdk::HostServiceOwnedRequest {
            capability: "test.host".into(),
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface: "test/v1".into(),
            operation: "get".into(),
            payload: vec![7],
        };
        assert_eq!(
            services.call_service_async(request()).await.unwrap(),
            vec![7]
        );
        invocation.close().unwrap();
        assert!(services.call_service_async(request()).await.is_err());
        invocation.drain().await.unwrap();
    }

    #[test]
    fn selected_kernel_dispatch_is_bound_per_invocation_and_closes_with_scope() {
        struct Kernel;
        impl crate::InvocationKernelDispatch for Kernel {
            fn execute(&self, _: &InvocationScope, request: &[u8]) -> Result<Vec<u8>> {
                Ok(request.to_vec())
            }
        }
        let execution = fixture_unactivated();
        execution.bind_kernel_dispatch(Arc::new(Kernel)).unwrap();
        assert!(execution.bind_kernel_dispatch(Arc::new(Kernel)).is_err());
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        // Ordinary invocations require completed activation, unlike lifecycle roots.
        activation.mark_active().unwrap();
        let first = activation.begin_invocation().unwrap();
        let second = activation.begin_invocation().unwrap();
        let services = execution
            .decorate_host_services(None, execution.kernel_port(&first).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            services.execute_kernel_encoded(b"opaque request").unwrap(),
            b"opaque request"
        );
        first.close().unwrap();
        assert!(services.execute_kernel_encoded(b"closed request").is_err());
        let sibling = execution
            .decorate_host_services(None, execution.kernel_port(&second).unwrap())
            .unwrap()
            .unwrap();
        assert!(sibling.execute_kernel_encoded(b"sibling request").is_ok());
    }

    #[test]
    fn checked_native_contracts_require_exact_route_and_preserve_envelope() {
        use bmux_plugin_sdk::definition::DeclaredServiceContract;
        let mut context = super::super::tests::native_test_service_context(vec![]);
        context.request.service.interface_id = "test-service@1.0".into();
        let checked =
            bmux_plugin_sdk::service_contract::encode("test-service@1.0", &[7; 32], b"request")
                .unwrap();
        context.request.payload = checked.clone();
        let descriptor = DeclaredServiceContract {
            capability: context.request.service.capability.as_str().into(),
            kind: context.request.service.kind,
            interface_id: context.request.service.interface_id.clone(),
            contract: vec![7; 32],
        };
        let descriptors = vec![descriptor.clone()];
        validate_native_service_contracts(&descriptors).unwrap();
        validate_native_checked_service(&descriptors, &context).unwrap();
        assert_eq!(context.request.payload, checked);
        assert!(validate_native_checked_service(&[], &context).is_err());
        for wrong in [
            DeclaredServiceContract {
                capability: "different.capability".into(),
                ..descriptor.clone()
            },
            DeclaredServiceContract {
                interface_id: "test-service".into(),
                ..descriptor.clone()
            },
            DeclaredServiceContract {
                interface_id: "test-service@2.0".into(),
                ..descriptor.clone()
            },
            DeclaredServiceContract {
                contract: vec![8; 32],
                ..descriptor.clone()
            },
        ] {
            assert!(validate_native_checked_service(&[wrong], &context).is_err());
        }
        let mut wrong_kind = descriptor.clone();
        wrong_kind.kind = match descriptor.kind {
            bmux_plugin_sdk::ServiceKind::Query => bmux_plugin_sdk::ServiceKind::Command,
            bmux_plugin_sdk::ServiceKind::Command | bmux_plugin_sdk::ServiceKind::Event => {
                bmux_plugin_sdk::ServiceKind::Query
            }
        };
        assert!(validate_native_checked_service(&[wrong_kind], &context).is_err());
        assert!(
            validate_native_service_contracts(&[descriptor.clone(), descriptor.clone()]).is_err()
        );
        let mut missing = descriptor;
        missing.contract.clear();
        assert!(validate_native_service_contracts(&[missing]).is_err());
    }

    #[test]
    fn checked_static_alias_uses_actual_evidence_without_rewriting_payload() {
        struct Evidence;
        impl PluginInstance for Evidence {
            fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
                (interface == "test-service").then_some(&[7; 32])
            }
        }
        let backend = ExecutionBackend::Static(Arc::new(Evidence));
        let mut context = super::super::tests::native_test_service_context(vec![]);
        let checked =
            bmux_plugin_sdk::service_contract::encode("test-service", &[7; 32], b"request")
                .unwrap();
        context.request.payload = checked.clone();
        validate_checked_service(&backend, &context).unwrap();
        assert_eq!(context.request.payload, checked);
        context.request.payload =
            bmux_plugin_sdk::service_contract::encode("test-service", &[8; 32], b"request")
                .unwrap();
        assert!(validate_checked_service(&backend, &context).is_err());
        context.request.payload =
            bmux_plugin_sdk::service_contract::encode("other-service", &[7; 32], b"request")
                .unwrap();
        assert!(validate_checked_service(&backend, &context).is_err());
        context.request.service.interface_id = "other-service".into();
        assert!(validate_checked_service(&backend, &context).is_err());
    }

    #[test]
    fn bundled_owner_reuses_retained_content_not_replacement_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("executable");
        std::fs::write(&path, b"first executable").unwrap();
        let owner = BundledExecutableCache::default();
        let first = owner
            .observe_with(|| BundledExecutableArtifact::capture(&path))
            .unwrap();
        std::fs::write(&path, b"rebuilt executable").unwrap();
        let repeated = owner
            .observe_with(|| panic!("must observe only once per owner"))
            .unwrap();
        assert_eq!(
            first.executable_parent,
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(repeated.executable_parent, first.executable_parent);
        assert!(Arc::ptr_eq(&first, &repeated));
        let new_owner = BundledExecutableCache::default();
        let rebuilt = new_owner
            .observe_with(|| BundledExecutableArtifact::capture(&path))
            .unwrap();
        assert_ne!(first.identity, rebuilt.identity);
        assert_eq!(first.identity.len(), 64);
    }

    #[test]
    fn failed_bundled_observation_is_not_cached_as_success() {
        let owner = BundledExecutableCache::default();
        assert!(owner.observe_with(|| Err(failure("read failed"))).is_err());
        assert!(owner.observed.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn static_command_context_uses_only_selected_call_port() {
        struct Inspect;
        impl PluginInstance for Inspect {
            fn command(
                &self,
                call: PluginCallContext,
                context: bmux_plugin_sdk::NativeCommandContext,
            ) -> PluginFuture<'_, bmux_plugin_sdk::definition::CommandOutcome> {
                use crate::ServiceCaller;
                assert_eq!(context.host_service_port, call.host_services);
                let response = context.call_service_raw(
                    "test.host",
                    bmux_plugin_sdk::ServiceKind::Command,
                    "test/v1",
                    "run",
                    vec![1],
                );
                if call.host_services.is_some() {
                    assert_eq!(response.unwrap(), vec![7]);
                } else {
                    assert!(response.is_err());
                }
                Box::pin(async { Ok(0).into() })
            }
        }
        let selected = bmux_plugin_sdk::PluginHostServices::new(|request| {
            assert_eq!(request.payload, &[1]);
            Ok(vec![7])
        });
        for port in [None, Some(selected)] {
            let mut value =
                serde_json::to_value(super::super::tests::native_test_service_context(vec![]))
                    .unwrap();
            value["command"] = serde_json::json!("test");
            value["arguments"] = serde_json::json!([]);
            let mut context: bmux_plugin_sdk::NativeCommandContext =
                serde_json::from_value(value).unwrap();
            context.host_service_port = Some(bmux_plugin_sdk::PluginHostServices::new(|_| {
                panic!("unselected command port must never be invoked")
            }));
            let outcome = static_operation(
                Arc::new(Inspect),
                NativeOperation::Command {
                    invocation_id: PluginInvocationId::new(),
                    context,
                },
                ServiceCancellation::new(),
                port,
            )
            .await;
            assert!(matches!(outcome, NativeOutcome::Command(_)));
        }
    }

    #[tokio::test]
    async fn static_service_context_replaces_unselected_executable_port() {
        struct Inspect;
        impl PluginInstance for Inspect {
            fn invoke(
                &self,
                call: PluginCallContext,
                context: bmux_plugin_sdk::NativeServiceContext,
            ) -> PluginFuture<'_, bmux_plugin_sdk::PluginResult<bmux_plugin_sdk::ServiceResponse>>
            {
                assert_eq!(context.host_service_port, call.host_services);
                Box::pin(async {
                    Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                        "inspected",
                    ))
                })
            }
        }
        let selected = bmux_plugin_sdk::PluginHostServices::new(|_| Ok(vec![7]));
        for port in [None, Some(selected)] {
            let context = super::super::tests::native_test_service_context(vec![])
                .with_host_service_port(bmux_plugin_sdk::PluginHostServices::new(|_| {
                    panic!("unselected port must never be invoked")
                }));
            let result = static_operation(
                Arc::new(Inspect),
                NativeOperation::Invoke {
                    invocation_id: PluginInvocationId::new(),
                    context,
                },
                ServiceCancellation::new(),
                port,
            )
            .await;
            assert!(matches!(result, NativeOutcome::Error { .. }));
        }
    }
    #[test]
    fn retained_response_growth_never_restarts_mutation() {
        let mut reads = 0;
        let result = read_bounded(|output, length| {
            reads += 1;
            *length = 8192;
            if output.len() < *length {
                abi::BUFFER_TOO_SMALL
            } else {
                output.fill(7);
                abi::OK
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(reads, 2);
        assert_eq!(result, vec![7; 8192]);
    }
    #[test]
    fn oversized_and_non_growing_native_outputs_fail_closed() {
        assert!(
            read_bounded(|_, length| {
                *length = abi::MAX_BUFFER_BYTES + 1;
                abi::BUFFER_TOO_SMALL
            })
            .is_err()
        );
        assert!(
            read_bounded(|_, length| {
                *length = 1;
                abi::BUFFER_TOO_SMALL
            })
            .is_err()
        );
    }
    #[tokio::test]
    async fn factories_are_fresh_and_observer_loss_does_not_retire_instances() {
        struct Implementation;
        impl PluginInstance for Implementation {}
        let artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
            bmux_plugin_sdk::definition::PluginMetadata {
                plugin_id: "test.scoped".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            |_| Ok(Arc::new(Implementation) as Arc<dyn PluginInstance>),
        ));
        let runtime = PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(1));
        let a = artifact
            .create(runtime, ComponentRole::Backend, &[])
            .unwrap();
        let b = artifact
            .create(runtime, ComponentRole::Backend, &[])
            .unwrap();
        assert_ne!(a.scope.id(), b.scope.id());
        let ExecutionBackend::Static(a_impl) = &a.backend else {
            panic!("expected static");
        };
        let ExecutionBackend::Static(b_impl) = &b.backend else {
            panic!("expected static");
        };
        assert!(!Arc::ptr_eq(a_impl, b_impl));
        let weak = Arc::downgrade(&a);
        drop(a);
        let retained = weak.upgrade().expect("observer loss must retain authority");
        retained.destroy().await.unwrap();
        drop(retained);
        assert!(weak.upgrade().is_none());
        b.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn static_outcomes_are_retained_and_mutations_execute_once() {
        struct Implementation(Arc<std::sync::atomic::AtomicUsize>);
        impl PluginInstance for Implementation {
            fn extension(
                &self,
                _call: PluginCallContext,
                _contract: String,
                _payload: Vec<u8>,
            ) -> PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<Vec<u8>>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![3; 8192])
                })
            }
        }
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_count = count.clone();
        let artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
            bmux_plugin_sdk::definition::PluginMetadata {
                plugin_id: "test.scoped".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            move |_| Ok(Arc::new(Implementation(factory_count.clone())) as Arc<dyn PluginInstance>),
        ));
        let execution = artifact
            .create(
                PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(2)),
                ComponentRole::Backend,
                &[],
            )
            .unwrap();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        execution.state.lock().unwrap().activation = Some(activation.clone());
        let id = execution
            .begin(NativeOperation::Extension {
                invocation_id: PluginInvocationId::new(),
                contract: "example".into(),
                payload: vec![],
            })
            .unwrap();
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(execution.poll(id, &mut context).unwrap().is_pending());
        assert!(
            matches!(execution.poll(id, &mut context).unwrap(), Poll::Ready(NativeOutcome::Bytes(bytes)) if bytes.len() == 8192)
        );
        assert!(execution.poll(id, &mut context).unwrap().is_ready());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(execution.destroy().await.is_err());
        execution.acknowledge(id).await.unwrap();
        assert!(execution.poll(id, &mut context).is_err());
        activation.close().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        execution.state.lock().unwrap().activation = None;
        execution.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn stream_callbacks_are_byte_bounded_and_closed_with_the_invocation() {
        let instance =
            InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(3)));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let downstream = bmux_plugin_sdk::BoundedServiceEventSink::new(2);
        let sink = ScopedEventSink {
            invocation: invocation.clone(),
            identity: PluginInvocationId::new(),
            downstream: ServiceEventSinkHandle::new(downstream),
            remaining: Mutex::new((1, 2)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        assert!(
            sink.emit_event(ServiceEvent {
                invocation_id: PluginInvocationId::new(),
                payload: vec![0; 3]
            })
            .is_err()
        );
        sink.emit_event(ServiceEvent {
            invocation_id: PluginInvocationId::new(),
            payload: vec![0; 2],
        })
        .unwrap();
        assert!(
            sink.emit_event(ServiceEvent {
                invocation_id: PluginInvocationId::new(),
                payload: vec![]
            })
            .is_err()
        );
        invocation.close().unwrap();
        assert!(
            sink.emit_event(ServiceEvent {
                invocation_id: PluginInvocationId::new(),
                payload: vec![]
            })
            .is_err()
        );
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.close().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        instance.close().unwrap();
        instance.drain().await.unwrap();
        instance.finish().unwrap();
    }

    #[tokio::test]
    async fn inherited_cancellation_is_live_and_registration_outcomes_are_portable() {
        struct Implementation;
        impl PluginInstance for Implementation {
            fn extension(
                &self,
                call: PluginCallContext,
                _contract: String,
                _payload: Vec<u8>,
            ) -> PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<Vec<u8>>> {
                Box::pin(async move { Ok(vec![u8::from(call.cancellation.is_cancelled())]) })
            }
        }
        let artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
            bmux_plugin_sdk::definition::PluginMetadata {
                plugin_id: "test.cancellation".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            |_| Ok(Arc::new(Implementation) as Arc<dyn PluginInstance>),
        ));
        let execution = artifact
            .create(
                PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(4)),
                ComponentRole::Backend,
                &[],
            )
            .unwrap();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        execution.state.lock().unwrap().activation = Some(activation.clone());
        let parent = ServiceCancellation::new();
        let id = execution
            .begin_with_cancellation(
                NativeOperation::Extension {
                    invocation_id: PluginInvocationId::new(),
                    contract: "example".into(),
                    payload: vec![],
                },
                parent.clone(),
            )
            .unwrap();
        parent.cancel();
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(execution.poll(id, &mut context).unwrap().is_pending());
        assert!(
            matches!(execution.poll(id, &mut context).unwrap(), Poll::Ready(NativeOutcome::Bytes(bytes)) if bytes == vec![1])
        );
        execution.acknowledge(id).await.unwrap();
        let registrations = execution.begin(NativeOperation::Registrations).unwrap();
        assert!(
            execution
                .poll(registrations, &mut context)
                .unwrap()
                .is_pending()
        );
        assert!(matches!(
            execution.poll(registrations, &mut context).unwrap(),
            Poll::Ready(NativeOutcome::Registrations(_))
        ));
        execution.acknowledge(registrations).await.unwrap();
        activation.close().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        execution.state.lock().unwrap().activation = None;
        execution.destroy().await.unwrap();
    }

    /// Both packaging paths reject executable context fields before invoking
    /// author code. Caller authority is installed only through `bind_host_dispatch`.
    #[test]
    fn executable_context_presence_is_rejected_before_admission() {
        assert!(reject_executable_context(true).is_err());
        assert!(reject_executable_context(false).is_ok());
    }

    #[test]
    fn admitted_native_start_status_includes_pending_not_read_retry_codes() {
        assert!(start_status(abi::PENDING).is_ok());
        assert!(start_status(abi::OK).is_ok());
        assert!(start_status(abi::BUSY).is_err());
        assert!(start_status(abi::BUFFER_TOO_SMALL).is_err());
    }

    pub(super) fn fixture_unactivated() -> Arc<LoadedExecution> {
        let execution = fixture_artifact()
            .create(
                PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(12)),
                ComponentRole::Backend,
                &[],
            )
            .unwrap();
        // Synchronous authority-only fixtures do not execute author callbacks.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            execution.bind_callback_runtime(runtime).unwrap();
        }
        execution
    }

    fn fixture_artifact() -> DefinitionArtifact {
        struct Implementation;
        impl PluginInstance for Implementation {}
        DefinitionArtifact::from_definition(PluginDefinition::new(
            bmux_plugin_sdk::definition::PluginMetadata {
                plugin_id: "test.safety".into(),
                version: "1".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: vec![],
                contracts: vec![],
                manifest_toml: None,
            },
            |_| Ok(Arc::new(Implementation) as Arc<dyn PluginInstance>),
        ))
    }
    pub(super) fn fixture() -> Arc<LoadedExecution> {
        let execution = fixture_unactivated();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        execution.state.lock().unwrap().activation = Some(activation);
        execution
    }
    fn poll_fixture(execution: &LoadedExecution, id: u64) -> Result<Poll<NativeOutcome>> {
        let waker = std::task::Waker::noop();
        execution.poll(id, &mut Context::from_waker(waker))
    }

    #[cfg(unix)]
    fn auxiliary_lifecycle_fixture() -> NativeLifecycleContext {
        let service = super::super::tests::native_test_service_context(vec![]);
        NativeLifecycleContext {
            plugin_id: "test.safety".into(),
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            registered_plugins: vec![],
            host: bmux_plugin_sdk::HostMetadata {
                plugin_abi_version: bmux_plugin_sdk::ApiVersion::new(1, 1),
                ..service.host
            },
            connection: service.connection,
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            auxiliary_executables: BTreeMap::new(),
            auxiliary_executable_errors: BTreeMap::new(),
            host_service_port: None,
            capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
            host_kernel_bridge: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lifecycle_factory_receives_actual_root_before_author_entry() {
        let execution = fixture_unactivated();
        let observed = Arc::new(Mutex::new(None));
        let capture = observed.clone();
        execution
            .bind_lifecycle_callback_factory(Arc::new(move |binding| {
                assert!(binding.active_entry().is_err());
                *capture.lock().unwrap() = Some(binding.invocation().clone());
                Ok(callbacks::LifecycleCallbackDispatch {
                    callback: None,
                    host: None,
                    kernel: None,
                })
            }))
            .unwrap();
        let id = execution
            .begin_activation(
                auxiliary_lifecycle_fixture(),
                PluginConcurrencyConfig::Concurrent,
            )
            .unwrap();
        let actual = observed.lock().unwrap().clone().unwrap();
        assert_eq!(
            execution
                .call(id)
                .unwrap()
                .invocation
                .as_ref()
                .unwrap()
                .id(),
            actual.id()
        );
        assert!(
            execution
                .bind_lifecycle_callback_factory(Arc::new(|_| panic!("replacement must not run")))
                .is_err()
        );
        execution.cancel(id).unwrap();
        assert!(actual.is_cancelled().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_scope_activation_failure_survives_cleanup_attempt() {
        unsafe extern "C" fn forbidden_bridge(
            _: *const u8,
            _: usize,
            _: *mut u8,
            _: usize,
            _: *mut usize,
        ) -> i32 {
            panic!("pre-scope rejection must not execute bridge")
        }
        let execution = fixture_unactivated();
        let mut context = auxiliary_lifecycle_fixture();
        // Rejection is before scope creation and before any author effects.
        context.host_kernel_bridge =
            Some(bmux_plugin_sdk::HostKernelBridge::from_fn(forbidden_bridge));
        let primary = execution
            .begin_activation(context, PluginConcurrencyConfig::Concurrent)
            .unwrap_err()
            .to_string();
        assert!(execution.state.lock().unwrap().activation.is_none());
        let cleanup = execution
            .begin_deactivation(auxiliary_lifecycle_fixture())
            .await
            .unwrap_err()
            .to_string();
        assert!(cleanup.contains(&primary));
        assert!(cleanup.contains("before cleanup scope creation"));
        assert!(execution.state.lock().unwrap().calls.is_empty());
        execution.destroy().await.unwrap();
    }

    #[cfg(unix)]
    fn private_auxiliary_lifecycle_fixture(root: &Path) -> NativeLifecycleContext {
        use std::os::unix::fs::DirBuilderExt as _;

        // The host initializes these private roots before plugin activation;
        // retention creates only its own auxiliary-images subtree.
        for directory in ["state", "runtime"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(directory))
                .unwrap();
        }
        let mut context = auxiliary_lifecycle_fixture();
        context.connection.state_dir = root.join("state").to_string_lossy().into_owned();
        context.connection.runtime_dir = root.join("runtime").to_string_lossy().into_owned();
        context
    }

    #[cfg(unix)]
    struct AuxiliaryActivationObserver(Arc<Mutex<Option<NativeLifecycleContext>>>);

    #[cfg(unix)]
    impl PluginInstance for AuxiliaryActivationObserver {
        fn activate(
            &self,
            context: PluginLifecycleContext,
        ) -> PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>> {
            *self.0.lock().unwrap() = Some(context.native);
            Box::pin(async { Ok(()) })
        }
    }

    #[cfg(unix)]
    async fn observe_auxiliary_activation(execution: &LoadedExecution, id: u64) -> NativeOutcome {
        // Bound only the observer wait; timeout does not unload retained work.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match poll_fixture(execution, id)? {
                    Poll::Ready(outcome) => break Ok::<_, PluginError>(outcome),
                    Poll::Pending => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await
        .expect("selected activation did not settle within fixture observation budget")
        .expect("selected activation polling failed")
    }

    thread_local! {
        static OBSERVER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    /// Synchronous author code runs on the host runtime's blocking pool,
    /// never on the observing async thread.
    fn assert_blocking_pool() {
        assert!(!OBSERVER.with(std::cell::Cell::get));
        assert!(tokio::runtime::Handle::try_current().is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_factory_instance_binds_blocking_lifecycle_before_author_effects() {
        struct Author(Arc<AtomicUsize>);
        impl bmux_plugin_sdk::author::Plugin for Author {
            fn declare(d: &mut bmux_plugin_sdk::author::Declarations<Self>) {
                d.blocking_activate(|author, _, _| {
                    assert_blocking_pool();
                    author.0.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                })
                .blocking_deactivate(|author, _| {
                    assert_blocking_pool();
                    author.0.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                });
            }
        }
        const MANIFEST: &str =
            "id = \"test.safety\"\nversion = \"1\"\ncomponent_roles = [\"backend\"]\n";
        OBSERVER.with(|observer| observer.set(true));
        for bound in [false, true] {
            let effects = Arc::new(AtomicUsize::new(0));
            let author_effects = effects.clone();
            let observed = Arc::new(Mutex::new(None));
            let factory_observed = observed.clone();
            let artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
                fixture_artifact().metadata.clone(),
                move |context: PluginCreateContext| {
                    *factory_observed.lock().unwrap() = Some(context.instance);
                    let instance = bmux_plugin_sdk::author::DeclaredInstance::new(
                        Author(author_effects.clone()),
                        MANIFEST,
                        &[],
                    )?;
                    let instance = if bound {
                        instance.for_instance(context.instance)
                    } else {
                        instance
                    };
                    Ok(Arc::new(instance) as Arc<dyn PluginInstance>)
                },
            ));
            let execution = artifact
                .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
                .unwrap();
            execution
                .bind_callback_runtime(tokio::runtime::Handle::current())
                .unwrap();
            assert_eq!(
                observed.lock().unwrap().unwrap().as_uuid(),
                execution.scope().id().as_uuid()
            );
            let id = execution
                .begin_activation(
                    auxiliary_lifecycle_fixture(),
                    PluginConcurrencyConfig::Concurrent,
                )
                .unwrap();
            let outcome = observe_auxiliary_activation(&execution, id).await;
            if !bound {
                assert!(matches!(outcome, NativeOutcome::Error { code: 70, message }
                    if message == "callback instance binding missing"));
                assert_eq!(effects.load(Ordering::Acquire), 0);
                execution.cancel_background_tasks().unwrap();
                execution.drain_background_tasks().await.unwrap();
                assert!(execution.acknowledge(id).await.is_err());
                assert!(execution.call(id).is_ok());
                let cleanup = execution
                    .begin_deactivation(auxiliary_lifecycle_fixture())
                    .await
                    .unwrap();
                assert!(matches!(
                    observe_auxiliary_activation(&execution, cleanup).await,
                    NativeOutcome::Error { code: 70, message }
                        if message == "callback instance binding missing"
                ));
                assert_eq!(effects.load(Ordering::Acquire), 0);
                assert!(execution.acknowledge(cleanup).await.is_err());
                continue;
            }
            assert!(matches!(outcome, NativeOutcome::Unit));
            assert_eq!(effects.load(Ordering::Acquire), 1);
            execution.acknowledge(id).await.unwrap();
            let cleanup = execution
                .begin_deactivation(auxiliary_lifecycle_fixture())
                .await
                .unwrap();
            assert!(matches!(
                observe_auxiliary_activation(&execution, cleanup).await,
                NativeOutcome::Unit
            ));
            assert_eq!(effects.load(Ordering::Acquire), 2);
            execution.acknowledge(cleanup).await.unwrap();
            execution.destroy().await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_lifecycle_resolves_selected_auxiliary_and_reports_missing_without_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("package");
        std::fs::create_dir(&base).unwrap();
        let worker = base.join("worker");
        std::fs::write(&worker, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = crate::PluginManifest::from_toml_str(
            r#"
            id = "test.safety"
            name = "Test"
            version = "1.0.0"
            [native_abi]
            minimum = "1.1"
            [[auxiliary_executables]]
            id = "worker"
            cargo_package = "example"
            cargo_binary = "worker"
            relative_path = "worker"
            protocol_version = 1
        "#,
        )
        .unwrap();
        let registered = crate::RegisteredPlugin {
            search_root: base.clone(),
            manifest_path: base.join("plugin.toml"),
            declaration: manifest.to_declaration().unwrap(),
            manifest,
            bundled_static: false,
        };
        let observed = Arc::new(Mutex::new(None));
        let mut artifact = fixture_artifact();
        let captured = observed.clone();
        artifact = DefinitionArtifact::from_definition(PluginDefinition::new(
            artifact.metadata.clone(),
            move |_| {
                Ok(Arc::new(AuxiliaryActivationObserver(captured.clone()))
                    as Arc<dyn PluginInstance>)
            },
        ));
        artifact.lifecycle_selection = Some(Arc::new(LifecycleSelection {
            registered,
            base: Ok(base),
        }));
        let execution = artifact
            .create(
                PluginRuntimeScopeId::from_uuid(uuid::Uuid::from_u128(25)),
                ComponentRole::Backend,
                &[],
            )
            .unwrap();
        execution
            .bind_callback_runtime(tokio::runtime::Handle::current())
            .unwrap();
        let mut context = private_auxiliary_lifecycle_fixture(root.path());
        let resolved = execution
            .resolved_lifecycle_context(context.clone())
            .unwrap();
        assert!(
            resolved.auxiliary_executable_errors.is_empty(),
            "{:?}",
            resolved.auxiliary_executable_errors,
        );
        let retained = &resolved.auxiliary_executables["worker"];
        assert_eq!(
            std::fs::read(&retained.path).unwrap(),
            b"#!/bin/sh\nexit 0\n"
        );
        assert_ne!(std::path::Path::new(&retained.path), worker);
        let activation = execution
            .begin_activation(context.clone(), PluginConcurrencyConfig::Concurrent)
            .unwrap();
        let outcome = observe_auxiliary_activation(&execution, activation).await;
        assert!(matches!(outcome, NativeOutcome::Unit), "{outcome:?}");
        let observed = observed.lock().unwrap();
        let author = observed
            .as_ref()
            .expect("selected activation hook must run");
        assert_eq!(author.auxiliary_executables, resolved.auxiliary_executables);
        assert!(
            author.auxiliary_executable_errors.is_empty(),
            "{:?}",
            author.auxiliary_executable_errors,
        );
        drop(observed);
        std::fs::rename(&worker, root.path().join("unselected-worker")).unwrap();
        let missing = execution
            .resolved_lifecycle_context(resolved.clone())
            .unwrap();
        assert!(missing.auxiliary_executables.is_empty());
        assert_ne!(missing.auxiliary_executable_errors["worker"], "");
        // Prior retained identity remains usable; it does not become a fallback
        // for a new failed resolution of the declared selected source.
        assert!(std::path::Path::new(&retained.path).is_file());
        context.host.plugin_abi_version = bmux_plugin_sdk::ApiVersion::new(1, 0);
        assert!(execution.resolved_lifecycle_context(context).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn selected_runtime_owner_survives_observer_loss_and_instance_retirement() {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
        );
        let weak = Arc::downgrade(&runtime);
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            let execution = fixture_artifact()
                .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
                .unwrap();
            execution
                .bind_callback_runtime_owner(runtime.handle().clone(), runtime.clone())
                .unwrap();
            let activation = execution
                .scope
                .begin_activation(PluginConcurrencyConfig::Concurrent)
                .unwrap();
            activation.mark_active().unwrap();
            execution.state.lock().unwrap().activation = Some(activation);
            let invocation = execution.activation().unwrap().begin_invocation().unwrap();
            let binding = execution
                .bind_root_callback(invocation.clone(), ServiceCancellation::new())
                .unwrap();
            let context = binding.context();
            let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
            let (started, entry_started) = tokio::sync::oneshot::channel();
            let (resume, wait) = std::sync::mpsc::channel();
            let (settled, complete) = tokio::sync::oneshot::channel();
            let selected = runtime.handle().id();
            let mut operation = context.execute_entered(
                execution.scope.id(),
                entry,
                Box::new(move || {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert_eq!(tokio::runtime::Handle::current().id(), selected);
                    tokio::runtime::Handle::current().block_on(async {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    });
                    settled.send(()).unwrap();
                    Ok(())
                }),
            );
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(operation.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            entry_started.await.unwrap();
            drop(operation);
            assert!(execution.destroy().await.is_err());
            assert!(weak.upgrade().is_some());
            resume.send(()).unwrap();
            complete.await.unwrap();
            invocation.close().unwrap();
            invocation.drain().await.unwrap();
            invocation.finish().unwrap();
            let cleanup = execution
                .begin_deactivation(auxiliary_lifecycle_fixture())
                .await
                .unwrap();
            assert!(matches!(
                observe_auxiliary_activation(&execution, cleanup).await,
                NativeOutcome::Unit
            ));
            execution.acknowledge(cleanup).await.unwrap();
            execution.destroy().await.unwrap();
            assert!(execution.callback_runtime_owner.lock().unwrap().is_none());
            // A retired binding retained by host/author state can no longer
            // admit an entry, so it must not keep the reactor alive: successful
            // destruction revoked the shared pin. The external owner is the
            // only remaining pin even while the binding and context live on.
            assert_eq!(Arc::strong_count(&runtime), 1);
            assert!(binding.enter().is_err());
            drop(context);
            drop(binding);
            assert_eq!(Arc::strong_count(&runtime), 1);
        });
        drop(runtime);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn selected_runtime_owner_releases_only_after_successful_destruction() {
        let owner = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
        );
        let weak = Arc::downgrade(&owner);
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            let execution = fixture_artifact()
                .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
                .unwrap();
            execution
                .bind_callback_runtime(owner.handle().clone())
                .unwrap();
            assert!(
                execution
                    .bind_callback_runtime_owner(observer.handle().clone(), owner.clone())
                    .is_err()
            );
            execution
                .bind_callback_runtime_owner(owner.handle().clone(), owner.clone())
                .unwrap();
            assert!(
                execution
                    .bind_callback_runtime_owner(owner.handle().clone(), owner.clone())
                    .is_err()
            );
            let resource = execution.scope.register_resource(Arc::new(())).unwrap();
            assert!(execution.destroy().await.is_err());
            assert_eq!(Arc::strong_count(&owner), 2);
            execution.scope.release_resource(resource).unwrap();
            execution.destroy().await.unwrap();
            assert_eq!(Arc::strong_count(&owner), 1);
        });
        drop(owner);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn lifecycle_root_retires_before_activation_publication_and_cleanup_finishes() {
        let execution = fixture_unactivated();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        execution.state.lock().unwrap().activation = Some(activation.clone());
        let invocation = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
            .unwrap();
        let admission = invocation.try_admit().unwrap();
        let id = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async { NativeOutcome::Unit })),
                cancellation: ServiceCancellation::new(),
                invocation: Some(invocation.clone()),
                lifecycle: Some(true),
                admission: Some(admission),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(activation.mark_active().is_err());
        assert!(poll_fixture(&execution, id).unwrap().is_pending());
        assert_eq!(
            activation.state().unwrap(),
            bmux_plugin_runtime::LifecycleState::Activating
        );
        assert!(poll_fixture(&execution, id).unwrap().is_ready());
        assert!(invocation.state().is_err());
        assert_eq!(
            activation.state().unwrap(),
            bmux_plugin_runtime::LifecycleState::Active
        );
        execution.acknowledge(id).await.unwrap();
        execution.cancel_background_tasks().unwrap();
        execution.drain_background_tasks().await.unwrap();
        let cleanup = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
            .unwrap();
        let admission = cleanup.try_admit().unwrap();
        let id = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async { NativeOutcome::Unit })),
                cancellation: ServiceCancellation::new(),
                invocation: Some(cleanup.clone()),
                lifecycle: Some(false),
                admission: Some(admission),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, id).unwrap().is_pending());
        assert!(poll_fixture(&execution, id).unwrap().is_ready());
        assert!(cleanup.state().is_err());
        execution.acknowledge(id).await.unwrap();
        execution.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_preserves_failed_activation_after_root_retirement() {
        let execution = fixture_unactivated();
        let activation = execution
            .scope
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        execution.state.lock().unwrap().activation = Some(activation.clone());
        let invocation = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Activation)
            .unwrap();
        let admission = invocation.try_admit().unwrap();
        let id = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async {
                    NativeOutcome::Error {
                        code: 7,
                        message: "original activation failure".into(),
                    }
                })),
                cancellation: ServiceCancellation::new(),
                invocation: Some(invocation.clone()),
                lifecycle: Some(true),
                admission: Some(admission),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, id).unwrap().is_pending());
        assert!(invocation.try_admit().is_err());
        assert!(matches!(
            poll_fixture(&execution, id).unwrap(),
            Poll::Ready(NativeOutcome::Error { code: 7, .. })
        ));
        assert!(invocation.state().is_err());
        assert!(
            execution
                .call(id)
                .unwrap()
                .invocation_retired
                .load(Ordering::Acquire)
        );
        // Startup cleanup visits retained failures twice: cancellation, then drain.
        execution.cancel_background_tasks().unwrap();
        execution.drain_background_tasks().await.unwrap();
        execution.cancel(id).unwrap();
        assert!(matches!(
            poll_fixture(&execution, id).unwrap(),
            Poll::Ready(NativeOutcome::Error { code: 7, message })
                if message == "original activation failure"
        ));
        assert!(execution.acknowledge(id).await.is_err());
        assert!(execution.call(id).is_ok());
        assert!(execution.state.lock().unwrap().retained.is_some());
        assert!(execution.destroy().await.is_err());
    }

    #[tokio::test]
    async fn repaired_lifecycle_cleanup_preserves_failure_until_explicit_acknowledgement() {
        let execution = fixture();
        let activation = execution.activation().unwrap();
        let resource = activation
            .register_resource(Arc::new(String::from("retained cleanup owner")))
            .unwrap();
        let failed = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async {
                    NativeOutcome::Error {
                        code: 1,
                        message: "activation failed".into(),
                    }
                })),
                cancellation: ServiceCancellation::new(),
                invocation: None,
                lifecycle: Some(true),
                admission: None,
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(matches!(
            poll_fixture(&execution, failed).unwrap(),
            Poll::Ready(NativeOutcome::Error { .. })
        ));
        assert!(execution.acknowledge(failed).await.is_err());
        assert!(
            execution
                .acknowledge_repaired_cleanup(failed)
                .await
                .is_err()
        );
        let deactivation = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async { NativeOutcome::Unit })),
                cancellation: ServiceCancellation::new(),
                invocation: None,
                lifecycle: Some(false),
                admission: None,
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, deactivation).unwrap().is_ready());
        assert!(poll_fixture(&execution, failed).unwrap().is_ready());
        assert!(execution.destroy().await.is_err());
        assert!(
            execution
                .acknowledge_repaired_cleanup(deactivation)
                .await
                .is_err()
        );
        assert!(poll_fixture(&execution, failed).unwrap().is_ready());
        activation.release_resource(resource).unwrap();
        execution
            .acknowledge_repaired_cleanup(deactivation)
            .await
            .unwrap();
        assert!(poll_fixture(&execution, failed).is_err());
        assert!(poll_fixture(&execution, deactivation).is_err());
        execution.destroy().await.unwrap();
    }

    struct DropObservedFuture {
        scope: InvocationScope,
        observed: Arc<AtomicBool>,
        panic_on_drop: bool,
    }
    impl std::future::Future for DropObservedFuture {
        type Output = NativeOutcome;
        fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(NativeOutcome::Unit)
        }
    }
    impl Drop for DropObservedFuture {
        fn drop(&mut self) {
            // Closing has already fenced callbacks, but the operation admission
            // must still prevent retirement throughout this destructor.
            assert!(self.scope.try_admit().is_err());
            assert!(matches!(
                self.scope.finish(),
                Err(bmux_plugin_runtime::ScopeError::Outstanding(_))
            ));
            self.observed.store(true, Ordering::Release);
            assert!(
                !self.panic_on_drop,
                "intentional completed-future destructor panic"
            );
        }
    }
    #[tokio::test]
    async fn completed_future_drop_is_fenced_before_final_delivery() {
        let execution = fixture();
        let activation = execution.activation().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let guard = invocation.try_admit().unwrap();
        let observed = Arc::new(AtomicBool::new(false));
        let future = Box::pin(DropObservedFuture {
            scope: invocation.clone(),
            observed: observed.clone(),
            panic_on_drop: false,
        });
        let id = execution
            .insert_call(CallStart {
                future: Some(future),
                cancellation: ServiceCancellation::new(),
                invocation: Some(invocation),
                lifecycle: None,
                admission: Some(guard),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, id).unwrap().is_pending());
        assert!(observed.load(Ordering::Acquire));
        assert!(poll_fixture(&execution, id).unwrap().is_ready());
        execution.acknowledge(id).await.unwrap();
        activation.close().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        execution.state.lock().unwrap().activation = None;
        execution.destroy().await.unwrap();
    }
    #[test]
    fn completed_future_destructor_panic_retains_cleanup_authority() {
        let execution = fixture();
        let invocation = execution.activation().unwrap().begin_invocation().unwrap();
        let guard = invocation.try_admit().unwrap();
        let observed = Arc::new(AtomicBool::new(false));
        let future = Box::pin(DropObservedFuture {
            scope: invocation.clone(),
            observed: observed.clone(),
            panic_on_drop: true,
        });
        let id = execution
            .insert_call(CallStart {
                future: Some(future),
                cancellation: ServiceCancellation::new(),
                invocation: Some(invocation),
                lifecycle: None,
                admission: Some(guard),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, id).is_err());
        assert!(observed.load(Ordering::Acquire));
        assert_eq!(
            execution.scope.state().unwrap(),
            bmux_plugin_runtime::LifecycleState::CleanupRequired
        );
        assert!(execution.state.lock().unwrap().calls.contains_key(&id));
        let weak = Arc::downgrade(&execution);
        drop(execution);
        assert!(weak.upgrade().is_some()); // Deliberately retained; not recoverable proof.
    }

    #[tokio::test]
    async fn selected_host_registry_owns_registration_mutations_and_retirement() {
        let execution = fixture();
        let activation = execution.activation().unwrap();
        let host = Arc::new(crate::PluginStateRegistry::new());
        let unrelated = crate::PluginStateRegistry::new();
        let port = host.registrations().bind(activation.clone()).unwrap();
        execution.state.lock().unwrap().registration = Some(port.clone());
        *execution.host_state.lock().unwrap() = Some(host.clone());
        let services = execution
            .decorate_host_services(None, None)
            .unwrap()
            .unwrap();
        let contract = bmux_plugin_sdk::RegistrationContract {
            kind: bmux_plugin_sdk::RegistrationKind::Service,
            interface: "test.selected-host".into(),
            version: 1,
            name: "service".into(),
        };
        services
            .registration(
                bmux_plugin_sdk::definition::RegistrationMutation::Register {
                    contract,
                    value: vec![1],
                },
            )
            .unwrap();
        port.publish().unwrap();
        let (visible, _) = host.registrations().subscribe().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(
            unrelated.registrations().subscribe().unwrap().0,
            [] as [crate::plugin_state::RegistrationSnapshot; 0],
        );
        assert!(activation.finish().is_err());
        port.withdraw().unwrap();
        port.withdraw_and_drain().await.unwrap();
        activation.finish().unwrap();
        execution.state.lock().unwrap().activation = None;
        execution.destroy().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_root_keeps_selected_registration_context_until_retirement() {
        let execution = fixture();
        let activation = execution.activation().unwrap();
        let host = Arc::new(crate::PluginStateRegistry::new());
        let port = host.registrations().bind(activation.clone()).unwrap();
        execution.state.lock().unwrap().registration = Some(port.clone());
        *execution.host_state.lock().unwrap() = Some(host.clone());
        execution.cancel_background_tasks().unwrap();
        execution.drain_background_tasks().await.unwrap();
        let cleanup = activation
            .begin_lifecycle_invocation(LifecycleInvocationKind::Cleanup)
            .unwrap();
        let admission = cleanup.try_admit().unwrap();
        assert!(host.registrations().context_for(&cleanup).is_ok());
        // No activation drain between root minting and author cleanup. The
        // callback authority stays live until the completed hook is destroyed.
        assert!(cleanup.try_admit().is_ok());
        let id = execution
            .insert_call(CallStart {
                future: Some(Box::pin(async { NativeOutcome::Unit })),
                cancellation: ServiceCancellation::new(),
                invocation: Some(cleanup.clone()),
                lifecycle: Some(false),
                admission: Some(admission),
                operation: None,
                stream_failure: None,
                deadline_ms: None,
                table: None,
            })
            .unwrap();
        assert!(poll_fixture(&execution, id).unwrap().is_pending());
        assert!(poll_fixture(&execution, id).unwrap().is_ready());
        assert!(cleanup.state().is_err());
        execution.acknowledge(id).await.unwrap();
        assert!(host.registrations().context_for(&cleanup).is_err());
        execution.destroy().await.unwrap();
    }

    #[test]
    fn pending_does_not_fabricate_an_empty_final_response() {
        assert!(read_bounded(|_, _| abi::PENDING).unwrap().is_none());
    }
}
