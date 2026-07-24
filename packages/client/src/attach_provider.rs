//! Domain-neutral attach-provider registration and target resolution.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock, Weak};

use crate::BmuxClient;

/// Parsed attach target presented to provider resolvers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTarget {
    raw: String,
    scheme: Option<String>,
    reference: String,
}

impl AttachTarget {
    /// Parse a target without assigning domain meaning to its scheme.
    #[must_use]
    pub fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let (scheme, reference) = raw.split_once("://").map_or_else(
            || (None, raw.clone()),
            |(scheme, reference)| {
                if valid_scheme(scheme) {
                    (Some(scheme.to_ascii_lowercase()), reference.to_string())
                } else {
                    (None, raw.clone())
                }
            },
        );
        Self {
            raw,
            scheme,
            reference,
        }
    }

    /// Original target text.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Normalized URI scheme, when the target has a valid `scheme://` prefix.
    #[must_use]
    pub fn scheme(&self) -> Option<&str> {
        self.scheme.as_deref()
    }

    /// Target text following a valid scheme, or the entire bare target.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|value| value.is_ascii_alphanumeric() || matches!(value, '+' | '-' | '.'))
}

/// Boxed provider future.
pub type AttachProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AttachProviderError>> + Send + 'a>>;

/// Opaque provider resolution carried into attach opening.
pub trait ResolvedAttachTarget: fmt::Debug + Send + Sync + 'static {
    /// Stable provider ID that produced this plan.
    fn provider_id(&self) -> &str;

    /// Runtime downcast support for provider-private plans.
    fn as_any(&self) -> &dyn Any;
}

/// Neutral backend returned by an attach provider.
#[derive(Debug)]
pub enum AttachProviderBackend {
    /// Existing one-server request/event client path.
    Legacy(BmuxClient),
    /// Native generic provider session.
    Session(Box<dyn crate::AttachSession>),
}

/// Neutral attach input returned by providers.
#[derive(Debug)]
pub struct AttachProviderSession {
    pub backend: AttachProviderBackend,
    /// Provider-resolved session/follow target. `None` preserves caller input.
    pub target: Option<String>,
}

/// Provider resolution/open failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachProviderError {
    /// Provider rejected a target it previously claimed.
    #[error("attach provider '{provider_id}' rejected target '{target}': {reason}")]
    InvalidTarget {
        provider_id: String,
        target: String,
        reason: String,
    },
    /// Provider could not open the resolved attach target.
    #[error("attach provider '{provider_id}' failed opening target: {reason}")]
    OpenFailed { provider_id: String, reason: String },
}

/// A provider capable of recognizing, resolving, and opening attach targets.
pub trait AttachProvider: fmt::Debug + Send + Sync + 'static {
    /// Stable process-local provider identifier.
    fn id(&self) -> &str;

    /// Resolution precedence. Higher values win.
    fn priority(&self) -> i32 {
        0
    }

    /// Whether this provider owns `target`.
    fn supports(&self, target: &AttachTarget) -> bool;

    /// Whether this provider requires the caller to establish the legacy
    /// fallback client before [`open`](Self::open).
    fn requires_fallback_client(&self) -> bool {
        false
    }

    /// Resolve target syntax/configuration into a provider-private plan.
    ///
    /// # Errors
    ///
    /// Returns a structured provider error when the claimed target is invalid
    /// or cannot be resolved.
    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError>;

    /// Open the resolved plan into the neutral client/session input consumed by
    /// the current attach runtime. `fallback_client` is present only when
    /// [`requires_fallback_client`](Self::requires_fallback_client) returned
    /// `true`.
    fn open(
        &self,
        resolved: Arc<dyn ResolvedAttachTarget>,
        resume: Option<crate::AttachResumeState>,
        fallback_client: Option<BmuxClient>,
    ) -> AttachProviderFuture<'_, AttachProviderSession>;
}

/// Failure to register a provider.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachProviderRegistrationError {
    /// IDs are required for deterministic diagnostics and deregistration.
    #[error("attach provider id cannot be empty")]
    EmptyId,
    /// IDs must not have surrounding whitespace.
    #[error("attach provider id cannot contain surrounding whitespace: '{id}'")]
    InvalidId { id: String },
    /// A live provider already owns this ID.
    #[error("attach provider '{id}' is already registered")]
    DuplicateId { id: String },
}

/// Failure to resolve an attach target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachProviderResolutionError {
    /// No registered provider accepted the target.
    #[error("no attach provider accepts target '{target}'")]
    NoProvider { target: String },
    /// Multiple providers at the highest priority accepted the target.
    #[error("attach target '{target}' is ambiguous between providers: {providers:?}")]
    Ambiguous {
        target: String,
        providers: Vec<String>,
    },
}

#[derive(Debug, Default)]
struct RegistryState {
    providers: BTreeMap<String, Arc<dyn AttachProvider>>,
}

/// Thread-safe attach-provider registry.
#[derive(Debug, Default)]
pub struct AttachProviderRegistry {
    state: RwLock<RegistryState>,
}

impl AttachProviderRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider until the returned guard is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate provider IDs.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub fn register(
        self: &Arc<Self>,
        provider: Arc<dyn AttachProvider>,
    ) -> Result<AttachProviderRegistration, AttachProviderRegistrationError> {
        let id = provider.id().to_string();
        if id.is_empty() {
            return Err(AttachProviderRegistrationError::EmptyId);
        }
        if id.trim() != id {
            return Err(AttachProviderRegistrationError::InvalidId { id });
        }
        let mut state = self
            .state
            .write()
            .expect("attach provider registry poisoned");
        if state.providers.contains_key(&id) {
            return Err(AttachProviderRegistrationError::DuplicateId { id });
        }
        state.providers.insert(id.clone(), provider);
        drop(state);
        Ok(AttachProviderRegistration {
            registry: Arc::downgrade(self),
            id: Some(id),
        })
    }

    /// Resolve a parsed target to exactly one highest-priority provider.
    ///
    /// # Errors
    ///
    /// Returns no-provider or same-priority ambiguity errors.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn AttachProvider>, AttachProviderResolutionError> {
        let mut matches = {
            let state = self
                .state
                .read()
                .expect("attach provider registry poisoned");
            state
                .providers
                .values()
                .filter(|provider| provider.supports(target))
                .cloned()
                .collect::<Vec<_>>()
        };
        if matches.is_empty() {
            return Err(AttachProviderResolutionError::NoProvider {
                target: target.raw().to_string(),
            });
        }
        matches.sort_by(|left, right| {
            right
                .priority()
                .cmp(&left.priority())
                .then_with(|| left.id().cmp(right.id()))
        });
        let priority = matches[0].priority();
        let winners = matches
            .iter()
            .take_while(|provider| provider.priority() == priority)
            .collect::<Vec<_>>();
        if winners.len() > 1 {
            return Err(AttachProviderResolutionError::Ambiguous {
                target: target.raw().to_string(),
                providers: winners
                    .iter()
                    .map(|provider| provider.id().to_string())
                    .collect(),
            });
        }
        Ok(Arc::clone(&matches[0]))
    }

    /// Return provider IDs in stable order.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        self.state
            .read()
            .expect("attach provider registry poisoned")
            .providers
            .keys()
            .cloned()
            .collect()
    }
}

/// Registration lifetime guard.
#[derive(Debug)]
pub struct AttachProviderRegistration {
    registry: Weak<AttachProviderRegistry>,
    id: Option<String>,
}

impl AttachProviderRegistration {
    /// Permanently keep this registration for the process lifetime.
    pub fn forget(mut self) {
        let _ = self.id.take();
    }
}

impl Drop for AttachProviderRegistration {
    fn drop(&mut self) {
        let (Some(registry), Some(id)) = (self.registry.upgrade(), self.id.take()) else {
            return;
        };
        registry
            .state
            .write()
            .expect("attach provider registry poisoned")
            .providers
            .remove(&id);
    }
}

/// Process-wide attach-provider registry.
#[must_use]
pub fn global_attach_provider_registry() -> Arc<AttachProviderRegistry> {
    static REGISTRY: OnceLock<Arc<AttachProviderRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(AttachProviderRegistry::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestResolvedTarget {
        provider_id: &'static str,
    }

    impl ResolvedAttachTarget for TestResolvedTarget {
        fn provider_id(&self) -> &str {
            self.provider_id
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct SchemeProvider {
        id: &'static str,
        scheme: Option<&'static str>,
        priority: i32,
    }

    impl AttachProvider for SchemeProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn priority(&self) -> i32 {
            self.priority
        }

        fn supports(&self, target: &AttachTarget) -> bool {
            target.scheme() == self.scheme
        }

        fn resolve(
            &self,
            _target: &AttachTarget,
        ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
            Ok(Arc::new(TestResolvedTarget {
                provider_id: self.id,
            }))
        }

        fn open(
            &self,
            _resolved: Arc<dyn ResolvedAttachTarget>,
            _resume: Option<crate::AttachResumeState>,
            _fallback_client: Option<BmuxClient>,
        ) -> AttachProviderFuture<'_, AttachProviderSession> {
            Box::pin(async move {
                Err(AttachProviderError::OpenFailed {
                    provider_id: self.id.to_string(),
                    reason: "test provider does not open clients".to_string(),
                })
            })
        }
    }

    #[test]
    fn target_parser_normalizes_valid_schemes_and_preserves_bare_targets() {
        let target = AttachTarget::parse("SYNTHETIC://workspace/main");
        assert_eq!(target.scheme(), Some("synthetic"));
        assert_eq!(target.reference(), "workspace/main");
        assert_eq!(target.raw(), "SYNTHETIC://workspace/main");

        let bare = AttachTarget::parse("main");
        assert_eq!(bare.scheme(), None);
        assert_eq!(bare.reference(), "main");

        let invalid = AttachTarget::parse("1bad://main");
        assert_eq!(invalid.scheme(), None);
        assert_eq!(invalid.reference(), "1bad://main");
    }

    #[test]
    fn resolves_one_matching_provider() {
        let registry = Arc::new(AttachProviderRegistry::new());
        let _registration = registry
            .register(Arc::new(SchemeProvider {
                id: "synthetic",
                scheme: Some("synthetic"),
                priority: 10,
            }))
            .expect("register provider");
        let provider = registry
            .resolve(&AttachTarget::parse("synthetic://main"))
            .expect("resolve provider");
        assert_eq!(provider.id(), "synthetic");
    }

    #[test]
    fn higher_priority_provider_wins_and_ties_are_rejected() {
        let registry = Arc::new(AttachProviderRegistry::new());
        let _low = registry
            .register(Arc::new(SchemeProvider {
                id: "low",
                scheme: Some("synthetic"),
                priority: 1,
            }))
            .unwrap();
        let high = registry
            .register(Arc::new(SchemeProvider {
                id: "high",
                scheme: Some("synthetic"),
                priority: 2,
            }))
            .unwrap();
        assert_eq!(
            registry
                .resolve(&AttachTarget::parse("synthetic://main"))
                .unwrap()
                .id(),
            "high"
        );
        drop(high);
        let _tie = registry
            .register(Arc::new(SchemeProvider {
                id: "tie",
                scheme: Some("synthetic"),
                priority: 1,
            }))
            .unwrap();
        assert!(matches!(
            registry.resolve(&AttachTarget::parse("synthetic://main")),
            Err(AttachProviderResolutionError::Ambiguous { providers, .. })
                if providers == ["low", "tie"]
        ));
    }

    #[test]
    fn duplicate_ids_are_rejected_and_drop_unregisters() {
        let registry = Arc::new(AttachProviderRegistry::new());
        let registration = registry
            .register(Arc::new(SchemeProvider {
                id: "provider",
                scheme: None,
                priority: 0,
            }))
            .unwrap();
        assert!(matches!(
            registry.register(Arc::new(SchemeProvider {
                id: "provider",
                scheme: Some("other"),
                priority: 0,
            })),
            Err(AttachProviderRegistrationError::DuplicateId { .. })
        ));
        drop(registration);
        assert!(registry.provider_ids().is_empty());
    }

    #[test]
    fn no_match_is_structured() {
        let registry = AttachProviderRegistry::new();
        assert!(matches!(
            registry.resolve(&AttachTarget::parse("unknown://main")),
            Err(AttachProviderResolutionError::NoProvider { target })
                if target == "unknown://main"
        ));
    }
}
