//! Trusted application-composition approval, separate from plugin declarations.
//! Only table entries explicitly carrying this policy may issue an approval.
#[cfg(test)]
#[path = "kernel_policy_tests.rs"]
mod tests;
use crate::runtime::plugin_kernel::AuthenticatedServiceInvocation;
use bmux_plugin::{
    InvocationKernelDispatch, LoadedExecution, PluginCandidateSource, PluginManifest,
    SelectedPluginCandidate,
};
use bmux_plugin_runtime::{InvocationScope, PluginInstanceId, PluginRuntimeScopeId};
use bmux_plugin_sdk::{
    PluginError, Result,
    definition::{ComponentRole, PluginMetadata},
};
use std::sync::Arc;

fn denied(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

/// Host-composed validator for one encoded kernel request; never plugin-supplied code.
type KernelRequestAuthorizer = fn(&[u8]) -> Result<()>;

/// Closed application composition selector. Config labels never resolve arbitrary code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeRequestPolicy {
    ClientsKernelV1,
}
impl NativeRequestPolicy {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "clients-kernel-v1" => Ok(Self::ClientsKernelV1),
            _ => Err(denied("unsupported native artifact request policy")),
        }
    }

    fn authorize(self, plugin_id: &str) -> Result<KernelRequestAuthorizer> {
        match self {
            #[cfg(feature = "bundled-plugin-clients")]
            Self::ClientsKernelV1 if plugin_id == "bmux.clients" => {
                Ok(bmux_clients_plugin::authorize_kernel_request)
            }
            _ => {
                let _ = plugin_id;
                Err(denied(
                    "native request policy is unavailable for this host composition",
                ))
            }
        }
    }
}

/// Read protected host approval storage and reject all unsupported composition selectors.
/// Call with the final resolved runtime paths, never paths derived from plugin settings.
pub(super) fn load_native_approvals(
    paths: &bmux_config::ConfigPaths,
) -> Result<Option<bmux_config::artifact_approval::ArtifactApprovalStore>> {
    let store = bmux_config::artifact_approval::ArtifactApprovalStore::load(paths)
        .map_err(|error| denied(format!("loading host artifact approvals: {error}")))?;
    if let Some(store) = &store {
        NativeKernelApproval::validate_store(store)?;
    }
    Ok(store)
}

/// Host-loaded policy intent. Only an actual selected-loader receipt can bind it
/// to a runtime instance; configuration matching alone grants nothing.
pub(super) struct NativeKernelApproval {
    plugin_id: String,
    artifact_sha256: String,
    role: ComponentRole,
    selected: SelectedPluginCandidate,
    authorize: KernelRequestAuthorizer,
}
impl NativeKernelApproval {
    /// Consume host policy intent only after exact selected-loader verification.
    /// The expected runtime is supplied by the owning host, not by plugin payloads.
    pub(super) fn bind_created(
        self,
        execution: &LoadedExecution,
        expected_runtime: PluginRuntimeScopeId,
    ) -> Result<InstanceKernelGrant> {
        let receipt = checked_receipt(&self.selected, execution)?;
        if receipt.runtime_scope().as_uuid() != expected_runtime.as_uuid()
            || !self.matches_observation(
                receipt.metadata().plugin_id.as_str(),
                receipt.role(),
                receipt.sha256(),
            )
            || receipt.native_abi_version() != bmux_plugin_sdk::abi::ABI_VERSION
            || receipt.native_capabilities() & bmux_plugin_sdk::abi::CAP_KERNEL_REQUESTS == 0
        {
            return Err(denied(
                "verified native artifact does not match host approval or kernel negotiation",
            ));
        }
        Ok(InstanceKernelGrant {
            instance: execution.scope().id(),
            runtime: execution.scope().runtime_id(),
            authorize: self.authorize,
        })
    }

    /// Check a loader-issued observation before it can be bound to a grant.
    /// This comparison is not a public grant constructor: the M3 receipt accessor
    /// must supply these observations, never a plugin callback or pathname hash.
    fn matches_observation(
        &self,
        plugin_id: &str,
        role: ComponentRole,
        artifact_sha256: &str,
    ) -> bool {
        self.plugin_id == plugin_id
            && self.role == role
            && self.artifact_sha256.eq_ignore_ascii_case(artifact_sha256)
    }

    /// Resolve every configured selector first, even if its plugin is disabled.
    /// Unknown or unavailable policies are explicit errors, never ignored entries.
    pub(super) fn validate_store(
        store: &bmux_config::artifact_approval::ArtifactApprovalStore,
    ) -> Result<()> {
        for approval in store.iter() {
            if approval.role() != "backend" {
                return Err(denied("kernel approval requires backend component"));
            }
            NativeRequestPolicy::parse(approval.request_policy())?
                .authorize(approval.plugin_id())?;
        }
        Ok(())
    }

    /// Match explicit selected native packaging against host policy intent.
    /// Artifact bytes are deliberately not read here. No executable authority is issued.
    pub(super) fn for_selected(
        store: &bmux_config::artifact_approval::ArtifactApprovalStore,
        selected: &SelectedPluginCandidate,
        runtime_dir: &std::path::Path,
        explicitly_selected: bool,
    ) -> Result<Option<Self>> {
        Self::validate_store(store)?;
        if store.runtime_dir() != runtime_dir {
            return Err(denied(
                "native approval store belongs to another selected runtime",
            ));
        }
        let plugin = &selected.candidate().plugin;
        let entry = store
            .iter()
            .find(|entry| entry.plugin_id() == plugin.manifest.id);
        let Some(entry) = entry else {
            return Ok(None);
        };
        if !explicitly_selected
            || !matches!(
                selected.candidate().source,
                PluginCandidateSource::Dynamic { .. }
            )
            || plugin.manifest.runtime != bmux_plugin::PluginRuntime::Native
            || selected.role() != ComponentRole::Backend
        {
            return Err(denied(
                "host native approval does not match explicit selected backend artifact",
            ));
        }
        if plugin
            .manifest
            .artifact_sha256
            .as_ref()
            .is_some_and(|pin| !pin.eq_ignore_ascii_case(entry.artifact_sha256()))
        {
            return Err(denied(
                "selected installation pin conflicts with host artifact approval",
            ));
        }
        Ok(Some(Self {
            plugin_id: entry.plugin_id().to_owned(),
            artifact_sha256: entry.artifact_sha256().to_owned(),
            role: selected.role(),
            selected: selected.clone(),
            authorize: NativeRequestPolicy::parse(entry.request_policy())?
                .authorize(entry.plugin_id())?,
        }))
    }
}

/// Not deserializable and not constructed from a plugin's requested capabilities.
/// The application composition table supplies the canonical bundled metadata.
pub(super) struct TrustedKernelPolicy {
    canonical: PluginManifest,
    authorize: KernelRequestAuthorizer,
}

impl TrustedKernelPolicy {
    pub(super) fn bundled(
        metadata: &PluginMetadata,
        authorize: KernelRequestAuthorizer,
    ) -> Result<Self> {
        let manifest = metadata
            .manifest_toml
            .as_deref()
            .ok_or_else(|| denied("trusted bundled policy requires canonical declaration"))?;
        let canonical = PluginManifest::from_toml_str(manifest)?;
        let same_identity = canonical.id == metadata.plugin_id;
        let same_version = canonical.version == metadata.version;
        if !same_identity || !same_version {
            return Err(denied("trusted composition declaration identity mismatch"));
        }
        Ok(Self {
            canonical,
            authorize,
        })
    }

    /// Runs before loading, but creates no executable authority. Successful
    /// selected loading must still verify observed executable/embedded metadata.
    pub(super) fn approve(
        self,
        selected: &SelectedPluginCandidate,
    ) -> Result<ApprovedKernelArtifact> {
        if selected.candidate().source != PluginCandidateSource::Bundled
            || selected.candidate().plugin.manifest != self.canonical
            || selected.role() != ComponentRole::Backend
        {
            return Err(denied(
                "selected artifact is not approved by trusted bundled kernel composition",
            ));
        }
        Ok(ApprovedKernelArtifact {
            selected: selected.clone(),
            authorize: self.authorize,
        })
    }
}

/// Private to this composition module and its caller. It is consumed directly
/// after the selected loader succeeds; it cannot be reconstructed from wire data.
pub(super) struct ApprovedKernelArtifact {
    selected: SelectedPluginCandidate,
    authorize: KernelRequestAuthorizer,
}
impl ApprovedKernelArtifact {
    pub(super) fn bind_created(self, execution: &LoadedExecution) -> Result<InstanceKernelGrant> {
        if execution.create_context().role != self.selected.role() {
            return Err(denied(
                "loaded component differs from approved composition role",
            ));
        }
        Ok(InstanceKernelGrant {
            instance: execution.scope().id(),
            runtime: execution.scope().runtime_id(),
            authorize: self.authorize,
        })
    }
}

/// Verify receipt provenance and the selected declaration before issuing authority.
/// Unverified loading paths cannot acquire a grant even if their metadata matches.
fn checked_receipt<'a>(
    selected: &SelectedPluginCandidate,
    execution: &'a LoadedExecution,
) -> Result<&'a bmux_plugin::VerifiedArtifact> {
    let receipt = execution
        .verified_artifact()
        .ok_or_else(|| denied("kernel approval requires selected-loader artifact receipt"))?;
    if receipt.instance().as_uuid() != execution.scope().id().as_uuid()
        || receipt.runtime_scope().as_uuid() != execution.scope().runtime_id().as_uuid()
        || receipt.role() != execution.create_context().role
        || receipt.role() != selected.role()
        || !matches!(
            selected.candidate().source,
            PluginCandidateSource::Dynamic { .. }
        )
    {
        return Err(denied(
            "artifact receipt does not belong to selected runtime instance",
        ));
    }
    selected.validate_embedded_metadata(receipt.metadata(), receipt.sha256())?;
    Ok(receipt)
}

/// Exact host-issued identity, never logical plugin-name authority.
#[derive(Clone)]
pub(super) struct InstanceKernelGrant {
    instance: PluginInstanceId,
    runtime: PluginRuntimeScopeId,
    authorize: KernelRequestAuthorizer,
}
impl InstanceKernelGrant {
    /// Bind this grant to one invocation's server-authenticated authority.
    ///
    /// A request issued while the author holds `callback`'s entry carries that
    /// entry as its call-chain parent, exactly like an entered host service
    /// call, so anything the host re-enters to answer it (e.g. a provider
    /// probe) is a descendant of this entry rather than unrelated work.
    /// Requests made outside the entry keep the invocation's own ancestry.
    pub(super) fn bind_context(
        &self,
        authority: AuthenticatedServiceInvocation,
        callback: bmux_plugin::CallbackBinding,
    ) -> Arc<dyn InvocationKernelDispatch> {
        Arc::new(GrantedInvocation {
            grant: self.clone(),
            authority,
            callback,
        })
    }
}
struct GrantedInvocation {
    grant: InstanceKernelGrant,
    authority: AuthenticatedServiceInvocation,
    callback: bmux_plugin::CallbackBinding,
}
impl InvocationKernelDispatch for GrantedInvocation {
    fn execute(&self, invocation: &InvocationScope, request: &[u8]) -> Result<Vec<u8>> {
        if invocation.instance_id() != self.grant.instance
            || invocation.runtime_id() != self.grant.runtime
        {
            return Err(denied(
                "kernel grant does not belong to this runtime instance",
            ));
        }
        let _admission = invocation
            .try_admit()
            .map_err(|error| denied(error.to_string()))?;
        (self.grant.authorize)(request)?;
        self.callback.active_entry().ok().map_or_else(
            || self.authority.execute(invocation, request),
            |entry| {
                self.authority
                    .clone()
                    .with_callback_parent(&entry)
                    .execute(invocation, request)
            },
        )
    }
}
