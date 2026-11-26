use crate::{HostScope, PluginError, RegisteredService, Result, ServiceKind, TypedServiceHandle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Explicit invocation-bound host dispatch.
///
/// Static implementations use safe Rust handles; a native
/// ABI adapter implements this trait by owning opaque callback handles. Neither
/// the trait object nor its callbacks are serialized into remote protocols.
/// The implementation binds caller provenance, deadline and live cancellation
/// from `invocation`, never from fields inside the operation payload. Nested
/// calls must create distinct child invocation identities.
pub trait HostServiceDispatch: Send + Sync {
    /// Dispatch once through the selected host.
    ///
    /// # Errors
    /// Returns admission, authorization, protocol or provider errors.
    fn invoke(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: HostServiceRequest<'_>,
    ) -> Result<Vec<u8>>;

    /// Acquire an independent owner-scoped retained host capability.
    ///
    /// # Errors
    /// Unsupported or stale authority must fail, never adapt `invoke` as a ledger.
    fn open_retained_host(
        &self,
        _invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> Result<crate::abi::retained::RetainedHostPort> {
        Err(PluginError::UnsupportedHostOperation {
            operation: "open_retained_host",
        })
    }

    /// Acquire explicitly negotiated recovery-v2 for the selected owner.
    ///
    /// # Errors
    /// Missing V2 support or stale authority fails without V1 or service fallback.
    fn open_retained_host_v2(
        &self,
        _invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> Result<crate::abi::retained::RetainedHostPort> {
        Err(PluginError::UnsupportedHostOperation {
            operation: "open_retained_host_v2",
        })
    }

    /// Observe selected registry presence through an admitted host lookup.
    ///
    /// # Errors
    /// Unsupported negotiation, denied access, stale scope and registry failures
    /// are errors, never evidence that a provider is absent. Implementations own
    /// authoritative lookup and must not infer presence from caller metadata.
    fn selected_service_availability(
        &self,
        _invocation: &bmux_plugin_runtime::InvocationScope,
        _key: crate::typed_dispatch::ServiceAvailabilityKey,
    ) -> crate::definition::PluginFuture<
        'static,
        Result<crate::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(async {
            Err(PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability",
            })
        })
    }

    /// Dispatch through an explicitly supported asynchronous host executor.
    /// Unsupported hosts never run a synchronous callback on the async worker.
    ///
    /// # Errors
    /// Returns unsupported dispatch by default, or selected executor failures.
    /// Implementations must retain admission for spawned work through settlement.
    fn invoke_async(
        &self,
        _invocation: &bmux_plugin_runtime::InvocationScope,
        _request: crate::HostServiceOwnedRequest,
    ) -> crate::definition::PluginFuture<'static, Result<Vec<u8>>> {
        Box::pin(async {
            Err(PluginError::UnsupportedHostOperation {
                operation: "invoke_async",
            })
        })
    }
}

/// Operation metadata is not caller authority. Generated BPDL clients own the
/// typed request/response encoding; this port does not invent a second envelope.
pub struct HostServiceRequest<'a> {
    pub capability: &'a str,
    pub kind: ServiceKind,
    pub interface: &'a str,
    pub operation: &'a str,
    pub payload: &'a [u8],
}

/// Invocation-bound host capability.
///
/// Supplied by trusted host wiring. It cannot
/// be reconstructed from serializable scope IDs. A stale captured port fails
/// closed once its invocation/activation closes. No global fallback is used.
#[derive(Clone)]
pub struct BoundHostServicePort {
    invocation: bmux_plugin_runtime::InvocationScope,
    dispatch: std::sync::Arc<dyn HostServiceDispatch>,
}

impl std::fmt::Debug for BoundHostServicePort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundHostServicePort")
            .field("invocation", &self.invocation.id())
            .finish_non_exhaustive()
    }
}

impl BoundHostServicePort {
    #[must_use]
    pub const fn invocation_scope(&self) -> &bmux_plugin_runtime::InvocationScope {
        &self.invocation
    }

    /// Host/ABI adapter construction only. Plugins receive an already bound
    /// capability; accepting payload-provided invocation IDs is not equivalent.
    #[must_use]
    pub fn new(
        invocation: bmux_plugin_runtime::InvocationScope,
        dispatch: std::sync::Arc<dyn HostServiceDispatch>,
    ) -> Self {
        Self {
            invocation,
            dispatch,
        }
    }

    /// Acquire an independent retained table while this invocation is admitted.
    ///
    /// # Errors
    /// Closed scopes and unnegotiated retained capability fail without fallback.
    /// The returned table has its own lifetime and requires explicit release.
    pub fn open_retained_host(&self) -> Result<crate::abi::retained::RetainedHostPort> {
        let _admission =
            self.invocation
                .try_admit()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: error.to_string(),
                })?;
        self.dispatch.open_retained_host(&self.invocation)
    }

    /// Open only recovery-v2 while this exact invocation is admitted.
    ///
    /// # Errors
    /// Rejects closed admission and unnegotiated V2; never falls back to V1.
    pub fn open_retained_host_v2(&self) -> Result<crate::abi::retained::RetainedHostPort> {
        let _admission =
            self.invocation
                .try_admit()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: error.to_string(),
                })?;
        self.dispatch.open_retained_host_v2(&self.invocation)
    }

    /// Query presence only through this capability's live invocation.
    ///
    /// # Errors
    /// Rejects invalid keys, cancelled/closed scopes, unsupported negotiation,
    /// access denial and registry errors; none are converted to `Absent`.
    pub fn selected_service_availability(
        &self,
        key: crate::typed_dispatch::ServiceAvailabilityKey,
    ) -> crate::definition::PluginFuture<
        'static,
        Result<crate::typed_dispatch::SelectedServiceAvailability>,
    > {
        let port = self.clone();
        Box::pin(async move {
            key.validate()?;
            let cancelled =
                port.invocation
                    .is_cancelled()
                    .map_err(|error| PluginError::ServiceProtocol {
                        details: error.to_string(),
                    })?;
            if cancelled {
                return Err(PluginError::ServiceProtocol {
                    details: "service availability invocation cancelled".into(),
                });
            }
            let _admission =
                port.invocation
                    .try_admit()
                    .map_err(|error| PluginError::ServiceProtocol {
                        details: error.to_string(),
                    })?;
            let presence = port
                .dispatch
                .selected_service_availability(&port.invocation, key)
                .await?;
            if port
                .invocation
                .is_cancelled()
                .map_err(|error| PluginError::ServiceProtocol {
                    details: error.to_string(),
                })?
            {
                return Err(PluginError::ServiceProtocol {
                    details: "service availability invocation cancelled during lookup".into(),
                });
            }
            Ok(presence)
        })
    }

    /// Dispatch owned data while retaining this exact invocation admission.
    ///
    /// # Errors
    /// Rejects stale scopes, unsupported async hosts and oversized responses.
    pub fn invoke_async(
        &self,
        request: crate::HostServiceOwnedRequest,
    ) -> crate::definition::PluginFuture<'static, Result<Vec<u8>>> {
        let port = self.clone();
        Box::pin(async move {
            request.validate()?;
            let _guard =
                port.invocation
                    .try_admit()
                    .map_err(|error| PluginError::ServiceProtocol {
                        details: format!("host port invocation admission rejected: {error}"),
                    })?;
            let response = port
                .dispatch
                .invoke_async(&port.invocation, request)
                .await?;
            if response.len() > crate::abi::MAX_BUFFER_BYTES {
                return Err(PluginError::ServiceProtocol {
                    details: "async host response exceeds bound; operation must not be replayed"
                        .into(),
                });
            }
            Ok(response)
        })
    }

    /// Dispatch one bounded operation without replay.
    ///
    /// # Errors
    /// Returns closed admission, oversized payload or selected dispatch errors.
    pub fn invoke(&self, request: HostServiceRequest<'_>) -> Result<Vec<u8>> {
        const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
        if request.payload.len() > MAX_PAYLOAD {
            return Err(PluginError::ServiceProtocol {
                details: "host port request exceeds bound".into(),
            });
        }
        let _guard = self
            .invocation
            .try_admit()
            .map_err(|error| PluginError::ServiceProtocol {
                details: format!("host port invocation admission rejected: {error}"),
            })?;
        let response = self.dispatch.invoke(&self.invocation, request)?;
        if response.len() > MAX_PAYLOAD {
            // Do not retry: the operation may already have committed effects.
            return Err(PluginError::ServiceProtocol {
                details: "host port response exceeds bound; operation must not be replayed".into(),
            });
        }
        Ok(response)
    }
}

impl crate::TypedDispatchClient for BoundHostServicePort {
    fn requires_service_contracts(&self) -> bool {
        true
    }

    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<
        Output = std::result::Result<Vec<u8>, crate::TypedDispatchClientError>,
    > + Send {
        let future = self.invoke_async(crate::HostServiceOwnedRequest {
            capability: capability.into(),
            kind: match kind {
                bmux_ipc::InvokeServiceKind::Query => ServiceKind::Query,
                bmux_ipc::InvokeServiceKind::Command => ServiceKind::Command,
            },
            interface: interface_id.into(),
            operation: operation.into(),
            payload,
        });
        let interface = interface_id.to_owned();
        let operation = operation.to_owned();
        async move {
            future.await.map_err(|error| {
                crate::TypedDispatchClientError::from_host_error(interface, operation, &error)
            })
        }
    }
}

#[cfg(test)]
mod host_service_port_tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn async_dispatch_has_no_sync_fallback_and_rejects_closed_scope() {
        let owner = InstanceScope::new(PluginRuntimeScopeId::new());
        let activation = owner
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = BoundHostServicePort::new(invocation.clone(), Arc::new(Dispatch(calls.clone())));
        let request = || crate::HostServiceOwnedRequest {
            capability: "example.service".into(),
            kind: ServiceKind::Query,
            interface: "example-query/v1".into(),
            operation: "read".into(),
            payload: Vec::new(),
        };
        let services = crate::definition::PluginHostServices::from_bound(port.clone());
        assert!(matches!(
            services.call_service_async(request()).await,
            Err(PluginError::UnsupportedHostOperation {
                operation: "invoke_async"
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        invocation.close().unwrap();
        invocation.finish().unwrap();
        assert!(
            port.invoke_async(request())
                .await
                .unwrap_err()
                .to_string()
                .contains("admission rejected")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retained_open_unsupported_and_closed_scope_never_use_sync_dispatch() {
        let (activation, invocation) = availability_scope();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = BoundHostServicePort::new(invocation.clone(), Arc::new(Dispatch(calls.clone())));
        let services = crate::definition::PluginHostServices::from_bound(port);
        assert!(matches!(
            services.open_retained_host(),
            Err(PluginError::UnsupportedHostOperation {
                operation: "open_retained_host"
            })
        ));
        invocation.close().unwrap();
        assert!(matches!(
            services.open_retained_host(),
            Err(PluginError::ServiceProtocol { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    #[tokio::test]
    async fn retained_v2_open_never_calls_v1_and_checks_actual_scope() {
        struct V1Only(Arc<AtomicUsize>);
        impl HostServiceDispatch for V1Only {
            fn invoke(
                &self,
                _: &bmux_plugin_runtime::InvocationScope,
                _: HostServiceRequest<'_>,
            ) -> Result<Vec<u8>> {
                panic!("V2 open cannot invoke synchronous service fallback");
            }
            fn open_retained_host(
                &self,
                _: &bmux_plugin_runtime::InvocationScope,
            ) -> Result<crate::abi::retained::RetainedHostPort> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(PluginError::ServiceProtocol {
                    details: "V1 fallback forbidden".into(),
                })
            }
        }
        let (activation, invocation) = availability_scope();
        let v1_calls = Arc::new(AtomicUsize::new(0));
        let port =
            BoundHostServicePort::new(invocation.clone(), Arc::new(V1Only(v1_calls.clone())));
        assert!(matches!(
            port.open_retained_host_v2(),
            Err(PluginError::UnsupportedHostOperation {
                operation: "open_retained_host_v2"
            })
        ));
        assert_eq!(v1_calls.load(Ordering::SeqCst), 0);
        invocation.close().unwrap();
        assert!(matches!(
            port.open_retained_host_v2(),
            Err(PluginError::ServiceProtocol { .. })
        ));
        assert_eq!(v1_calls.load(Ordering::SeqCst), 0);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    fn availability_key() -> crate::typed_dispatch::ServiceAvailabilityKey {
        crate::typed_dispatch::ServiceAvailabilityKey {
            capability: "example.service".into(),
            kind: ServiceKind::Query,
            interface_id: "example-query/v1".into(),
        }
    }

    fn availability_scope() -> (
        bmux_plugin_runtime::ActivationScope,
        bmux_plugin_runtime::InvocationScope,
    ) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        (activation, invocation)
    }

    #[tokio::test]
    async fn availability_defaults_to_unsupported_never_absent_or_service_dispatch() {
        let (activation, invocation) = availability_scope();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = BoundHostServicePort::new(invocation.clone(), Arc::new(Dispatch(calls.clone())));
        let services = crate::definition::PluginHostServices::from_bound(port);
        assert!(matches!(
            services
                .selected_service_availability(availability_key())
                .await,
            Err(PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability"
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }

    struct AvailabilityDispatch {
        instance: bmux_plugin_runtime::PluginInstanceId,
        calls: Arc<AtomicUsize>,
    }
    impl HostServiceDispatch for AvailabilityDispatch {
        fn invoke(
            &self,
            _: &bmux_plugin_runtime::InvocationScope,
            _: HostServiceRequest<'_>,
        ) -> Result<Vec<u8>> {
            panic!("availability must not invoke the service");
        }
        fn selected_service_availability(
            &self,
            invocation: &bmux_plugin_runtime::InvocationScope,
            _: crate::typed_dispatch::ServiceAvailabilityKey,
        ) -> crate::definition::PluginFuture<
            'static,
            Result<crate::typed_dispatch::SelectedServiceAvailability>,
        > {
            let matches = invocation.instance_id() == self.instance;
            let calls = self.calls.clone();
            Box::pin(async move {
                if !matches {
                    return Err(PluginError::ServiceProtocol {
                        details: "availability caller instance mismatch".into(),
                    });
                }
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::typed_dispatch::SelectedServiceAvailability::Present)
            })
        }
    }

    #[tokio::test]
    async fn availability_rejects_oversized_key_foreign_owner_and_closed_scope() {
        let (activation, invocation) = availability_scope();
        let (foreign_activation, foreign) = availability_scope();
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatch = Arc::new(AvailabilityDispatch {
            instance: invocation.instance_id(),
            calls: calls.clone(),
        });
        let port = BoundHostServicePort::new(invocation.clone(), dispatch.clone());
        let mut oversized = availability_key();
        oversized.interface_id = "x".repeat(crate::service_contract::MAX_INTERFACE_ID_BYTES + 1);
        assert!(port.selected_service_availability(oversized).await.is_err());
        let foreign_port = BoundHostServicePort::new(foreign.clone(), dispatch);
        assert!(
            foreign_port
                .selected_service_availability(availability_key())
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            port.selected_service_availability(availability_key())
                .await
                .unwrap(),
            crate::typed_dispatch::SelectedServiceAvailability::Present
        );
        invocation.close().unwrap();
        assert!(
            port.selected_service_availability(availability_key())
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        foreign.drain().await.unwrap();
        foreign.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
        foreign_activation.drain().await.unwrap();
        foreign_activation.finish().unwrap();
    }

    struct Dispatch(Arc<AtomicUsize>);
    impl HostServiceDispatch for Dispatch {
        fn invoke(
            &self,
            _invocation: &bmux_plugin_runtime::InvocationScope,
            request: HostServiceRequest<'_>,
        ) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(request.payload.to_vec())
        }
    }

    #[test]
    fn closed_invocation_never_reaches_host_dispatch() {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = BoundHostServicePort::new(invocation.clone(), Arc::new(Dispatch(calls.clone())));
        let request = || HostServiceRequest {
            capability: "example",
            kind: ServiceKind::Query,
            interface: "example/v1",
            operation: "get",
            payload: b"hello",
        };
        assert_eq!(port.invoke(request()).unwrap(), b"hello");
        invocation.close().unwrap();
        assert!(port.invoke(request()).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub product_name: String,
    pub product_version: String,
    pub plugin_api_version: crate::ApiVersion,
    pub plugin_abi_version: crate::ApiVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConnectionInfo {
    /// Canonical configuration directory. On macOS this is typically
    /// `~/Library/Application Support/bmux`; on Linux `~/.config/bmux`.
    /// Plugins that want to read user configuration files should
    /// prefer [`HostConnectionInfo::probe_config_file`] over joining
    /// against this path directly, because the host may expose a
    /// fallback chain through `config_dir_candidates`.
    pub config_dir: String,
    /// Ordered fallback chain for config-file lookups. The first entry
    /// is typically equal to `config_dir`; on macOS a second entry
    /// points at `~/.config/bmux` so XDG-style setups (e.g. home-manager
    /// managed dotfiles) resolve correctly. When empty, consumers fall
    /// back to `[config_dir]` alone. Serialized payloads that predate
    /// this field (recording playbooks, persisted state snapshots)
    /// round-trip cleanly thanks to `#[serde(default)]`.
    #[serde(default)]
    pub config_dir_candidates: Vec<String>,
    pub runtime_dir: String,
    pub data_dir: String,
    pub state_dir: String,
}

impl HostConnectionInfo {
    /// Probe the candidate config-dir chain for a file with the given
    /// relative path. Returns the first `candidate.join(relative)`
    /// that exists on disk.
    ///
    /// When `config_dir_candidates` is empty (older host or minimal
    /// test construction), the probe falls back to checking
    /// `config_dir.join(relative)` alone. This keeps the helper
    /// forward-compatible with payloads serialized before the
    /// candidate chain was introduced.
    #[must_use]
    pub fn probe_config_file(&self, relative: impl AsRef<Path>) -> Option<PathBuf> {
        let relative = relative.as_ref();
        let candidates: Vec<&str> = if self.config_dir_candidates.is_empty() {
            vec![self.config_dir.as_str()]
        } else {
            self.config_dir_candidates
                .iter()
                .map(String::as_str)
                .collect()
        };
        candidates
            .into_iter()
            .map(|dir| Path::new(dir).join(relative))
            .find(|path| path.exists())
    }

    /// Return the ordered list of candidate config directories as
    /// `Path` references. Falls back to `[config_dir]` when the
    /// explicit chain is empty, matching [`Self::probe_config_file`].
    pub fn config_dir_candidate_paths(&self) -> Vec<PathBuf> {
        if self.config_dir_candidates.is_empty() {
            vec![PathBuf::from(&self.config_dir)]
        } else {
            self.config_dir_candidates
                .iter()
                .map(PathBuf::from)
                .collect()
        }
    }
}

/// Result of looking up a registered service with optional typed dispatch.
///
/// Always carries the descriptor; optionally carries an in-process typed
/// handle when the provider is a native Rust plugin that registered a
/// trait impl via `register_typed_services`.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedService<'a> {
    descriptor: &'a RegisteredService,
    typed: Option<&'a TypedServiceHandle>,
}

impl<'a> ResolvedService<'a> {
    /// Construct a resolved service.
    #[must_use]
    pub const fn new(
        descriptor: &'a RegisteredService,
        typed: Option<&'a TypedServiceHandle>,
    ) -> Self {
        Self { descriptor, typed }
    }

    /// The service descriptor carrying capability, kind, interface id,
    /// and provider identity.
    #[must_use]
    pub const fn descriptor(&self) -> &'a RegisteredService {
        self.descriptor
    }

    /// The typed handle, if the provider registered one in-process.
    #[must_use]
    pub const fn typed(&self) -> Option<&'a TypedServiceHandle> {
        self.typed
    }
}

impl AsRef<RegisteredService> for ResolvedService<'_> {
    fn as_ref(&self) -> &RegisteredService {
        self.descriptor
    }
}

pub trait PluginHost: Send + Sync {
    fn plugin_id(&self) -> &str;
    fn metadata(&self) -> &HostMetadata;
    fn connection(&self) -> &HostConnectionInfo;
    fn required_capabilities(&self) -> &BTreeSet<HostScope>;
    fn provided_capabilities(&self) -> &BTreeSet<HostScope>;

    fn has_capability(&self, capability: &HostScope) -> bool {
        self.required_capabilities().contains(capability)
            || self.provided_capabilities().contains(capability)
    }

    fn available_services(&self) -> &[RegisteredService];

    /// Return the typed handle for a given service key, if one was
    /// registered by an in-process provider.
    ///
    /// The default implementation always returns `None`; hosts that
    /// support typed dispatch override this to look up their typed map.
    fn typed_handle(
        &self,
        _capability: &HostScope,
        _kind: ServiceKind,
        _interface_id: &str,
    ) -> Option<&TypedServiceHandle> {
        None
    }

    /// Resolve a registered service by capability, kind, and interface ID.
    ///
    /// The default implementation checks that the plugin has access to the
    /// requested capability, then searches the available service list.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::CapabilityAccessDenied`] if the plugin does not
    /// hold the requested capability, or [`PluginError::ServiceProtocol`] if
    /// no matching service registration is found.
    fn resolve_service(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Result<&RegisteredService> {
        if !self.has_capability(capability) {
            return Err(PluginError::CapabilityAccessDenied {
                plugin_id: self.plugin_id().to_string(),
                capability: capability.as_str().to_string(),
                operation: "resolve_service",
                hint: Some(
                    crate::CapabilityAccessDeniedHint::declare_required_capability(
                        capability.as_str(),
                    ),
                ),
            });
        }

        self.available_services()
            .iter()
            .find(|service| {
                service.capability == *capability
                    && service.kind == kind
                    && service.interface_id == interface_id
            })
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: format!(
                    "missing service registration for capability '{}' ({kind:?}) interface '{}'",
                    capability.as_str(),
                    interface_id,
                ),
            })
    }

    /// Resolve a registered service and any in-process typed handle
    /// registered for it.
    ///
    /// Consumers that can use typed dispatch call this variant and, if
    /// a typed handle is present, bypass byte-encoded calls by invoking
    /// the provider's trait directly. Consumers with no typed support
    /// use [`Self::resolve_service`] and the byte transport.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::resolve_service`].
    fn resolve_typed_service(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Result<ResolvedService<'_>> {
        let descriptor = self.resolve_service(capability, kind, interface_id)?;
        let typed = self.typed_handle(capability, kind, interface_id);
        Ok(ResolvedService::new(descriptor, typed))
    }
}

pub struct PluginContext<'a> {
    pub host: &'a dyn PluginHost,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, ProviderId};

    struct MockHost {
        plugin_id: String,
        metadata: HostMetadata,
        connection: HostConnectionInfo,
        required_capabilities: BTreeSet<HostScope>,
        provided_capabilities: BTreeSet<HostScope>,
        services: Vec<RegisteredService>,
    }

    impl MockHost {
        fn new(required: &[&str], provided: &[&str], services: Vec<RegisteredService>) -> Self {
            Self {
                plugin_id: "example.plugin".to_string(),
                metadata: HostMetadata {
                    product_name: "bmux".to_string(),
                    product_version: "0.1.0".to_string(),
                    plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                    plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
                },
                connection: HostConnectionInfo {
                    config_dir: "/tmp/config".to_string(),
                    config_dir_candidates: vec!["/tmp/config".to_string()],
                    runtime_dir: "/tmp/runtime".to_string(),
                    data_dir: "/tmp/data".to_string(),
                    state_dir: "/tmp/state".to_string(),
                },
                required_capabilities: required
                    .iter()
                    .map(|value| HostScope::new(*value).expect("capability should parse"))
                    .collect(),
                provided_capabilities: provided
                    .iter()
                    .map(|value| HostScope::new(*value).expect("capability should parse"))
                    .collect(),
                services,
            }
        }
    }

    impl PluginHost for MockHost {
        fn plugin_id(&self) -> &str {
            &self.plugin_id
        }

        fn metadata(&self) -> &HostMetadata {
            &self.metadata
        }

        fn connection(&self) -> &HostConnectionInfo {
            &self.connection
        }

        fn required_capabilities(&self) -> &BTreeSet<HostScope> {
            &self.required_capabilities
        }

        fn provided_capabilities(&self) -> &BTreeSet<HostScope> {
            &self.provided_capabilities
        }

        fn available_services(&self) -> &[RegisteredService] {
            &self.services
        }
    }

    #[test]
    fn resolve_service_allows_required_capability() {
        let capability = HostScope::new("example.read").expect("capability should parse");
        let host = MockHost::new(
            &["example.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Query,
                interface_id: "example-query/v1".to_string(),
                provider: ProviderId::Plugin("provider.plugin".to_string()),
            }],
        );

        let service =
            PluginHost::resolve_service(&host, &capability, ServiceKind::Query, "example-query/v1")
                .expect("service should resolve");
        assert_eq!(service.provider.to_string(), "provider.plugin");
    }

    #[test]
    fn resolve_service_rejects_missing_capability() {
        let capability = HostScope::new("example.write").expect("capability should parse");
        let host = MockHost::new(
            &["example.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Command,
                interface_id: "example-command/v1".to_string(),
                provider: ProviderId::Plugin("provider.plugin".to_string()),
            }],
        );

        let error = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "example-command/v1",
        )
        .expect_err("missing capability should fail");
        assert!(error.to_string().contains("example.write"));
    }

    #[test]
    fn resolve_typed_service_returns_descriptor_and_no_handle_by_default() {
        let capability = HostScope::new("example.read").expect("cap");
        let host = MockHost::new(
            &["example.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Query,
                interface_id: "example-query/v1".to_string(),
                provider: ProviderId::Plugin("provider.plugin".to_string()),
            }],
        );
        let resolved = PluginHost::resolve_typed_service(
            &host,
            &capability,
            ServiceKind::Query,
            "example-query/v1",
        )
        .expect("resolves");
        assert_eq!(resolved.descriptor().interface_id, "example-query/v1");
        assert!(resolved.typed().is_none());
    }
}
