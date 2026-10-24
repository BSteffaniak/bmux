//! Local-only activation status. No listener, remote authority, or replicated state.
use crate::{
    config::{RelayPolicy, RemoteSettings},
    identity::RuntimeIdentity,
};

/// Public, non-secret status of the connections plugin's local transport identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStatus {
    /// Stable endpoint public key, not a principal or cluster membership credential.
    pub endpoint_id: String,
    /// Validated relay policy name; custom URLs are deliberately omitted.
    pub relay_mode: &'static str,
    /// Upper bound for a future explicitly enabled transport readiness wait.
    pub startup_wait_ms: u64,
    /// Always false until authenticated ingress is implemented and enabled.
    pub listener_enabled: bool,
}

pub struct LocalRuntime {
    // Retain the actual key for the future secured transport, never a Debug value.
    identity: RuntimeIdentity,
    settings: RemoteSettings,
}

impl LocalRuntime {
    pub(crate) const fn new(identity: RuntimeIdentity, settings: RemoteSettings) -> Self {
        Self { identity, settings }
    }

    pub(crate) fn ingress_enabled(&self, preference: Option<bool>) -> bool {
        self.settings.ingress_enabled(preference)
    }

    pub(crate) fn ingress_prohibited(&self) -> bool {
        self.settings.allow_full_runtime_ingress == Some(false)
    }

    pub(crate) const fn auto_start(&self) -> bool {
        self.settings.auto_start
    }

    pub(crate) fn endpoint_builder(&self) -> anyhow::Result<iroh::endpoint::Builder> {
        let mut builder = self
            .settings
            .relay
            .apply(self.identity.endpoint_builder())?;
        if let Some(addr) = self.settings.bind_addr {
            builder = builder.clear_ip_transports().bind_addr(addr)?;
        }
        Ok(builder)
    }

    pub(crate) fn status(&self) -> LocalStatus {
        LocalStatus {
            endpoint_id: self.identity.endpoint_id(),
            relay_mode: match self.settings.relay {
                RelayPolicy::Default => "default",
                RelayPolicy::Disabled => "disabled",
                RelayPolicy::Custom { .. } => "custom",
            },
            startup_wait_ms: self.settings.startup_wait_ms,
            listener_enabled: false,
        }
    }
}
