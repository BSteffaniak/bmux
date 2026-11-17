//! Plugin-owned saved-host attach adapter. Targets carry names, never credentials.
use anyhow::{Context, Result, ensure};
use bmux_client::{
    AttachProvider, AttachProviderBackend, AttachProviderError, AttachProviderFuture,
    AttachProviderSession, AttachTarget, ResolvedAttachTarget,
};
use std::{any::Any, sync::Arc};
const ID: &str = "bmux.connections";
/// The paired-terminal provider, owned by whichever host registers it.
#[derive(Debug)]
struct PairedProvider {
    paths: Option<bmux_config::ConfigPaths>,
}
#[derive(Debug)]
struct PairedTarget {
    host: String,
    terminal: String,
}
impl ResolvedAttachTarget for PairedTarget {
    fn provider_id(&self) -> &str {
        ID
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
fn parse(reference: &str) -> Result<PairedTarget> {
    let (host, terminal) = reference
        .split_once('/')
        .context("expected host/terminal")?;
    crate::remote_preferences::name_valid(host)?;
    crate::remote_preferences::name_valid(terminal)?;
    ensure!(!terminal.contains('/'), "invalid terminal name");
    Ok(PairedTarget {
        host: host.into(),
        terminal: terminal.into(),
    })
}
impl AttachProvider for PairedProvider {
    fn id(&self) -> &str {
        ID
    }
    fn priority(&self) -> i32 {
        100
    }
    fn supports(&self, target: &AttachTarget) -> bool {
        target.scheme() == Some("paired-terminal")
    }
    fn requires_fallback_client(&self) -> bool {
        false
    }
    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
        parse(target.reference())
            .map(|target| Arc::new(target) as Arc<dyn ResolvedAttachTarget>)
            .map_err(|_| AttachProviderError::InvalidTarget {
                provider_id: ID.into(),
                target: "paired-terminal".into(),
                reason: "expected a saved host and terminal name".into(),
            })
    }
    fn open(
        &self,
        resolved: Arc<dyn ResolvedAttachTarget>,
        _resume: Option<bmux_client::AttachResumeState>,
        _context: bmux_client::AttachProviderOpenContext,
    ) -> AttachProviderFuture<'_, AttachProviderSession> {
        Box::pin(async move {
            let target = resolved
                .as_any()
                .downcast_ref::<PairedTarget>()
                .ok_or_else(|| AttachProviderError::OpenFailed {
                    provider_id: ID.into(),
                    reason: "invalid resolved target".into(),
                })?;
            let paths = self
                .paths
                .clone()
                .ok_or_else(|| AttachProviderError::OpenFailed {
                    provider_id: ID.into(),
                    reason: "client paths were not initialized".into(),
                })?;
            let session = crate::terminal_attach::RemoteTerminal::open(
                paths,
                target.host.clone(),
                target.terminal.clone(),
            )
            .await
            .map_err(|_| AttachProviderError::OpenFailed {
                provider_id: ID.into(),
                reason: "paired terminal could not be opened".into(),
            })?;
            Ok(AttachProviderSession {
                backend: AttachProviderBackend::Session(Box::new(session)),
                target: Some(target.terminal.clone()),
            })
        })
    }
}
/// The provider a host registers into its own attach-provider registry.
#[must_use]
pub fn provider(paths: bmux_config::ConfigPaths) -> Arc<dyn AttachProvider> {
    Arc::new(PairedProvider { paths: Some(paths) })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn targets_are_names_not_secrets() {
        assert!(parse("desktop/main").is_ok());
        for target in [
            "desktop",
            "desktop/main/extra",
            "device://secret",
            "desktop/main?secret=x",
        ] {
            assert!(parse(target).is_err());
        }
        assert!(!PairedProvider { paths: None }.requires_fallback_client());
    }
}
