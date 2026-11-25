//! Route access classes for streams admitted from outside the host.
//!
//! Every service route declares a class in BPDL (`@access(...)` on an
//! interface, optionally overridden per operation). For a request on an
//! admitted stream the server looks up the route's class and asks the
//! registered [`RemoteAccessAuthorizerHandle`] whether the stream's subject
//! may use it. Local streams are never checked.
//!
//! A route with no class, or no registered authorizer, is denied on admitted
//! streams, and [`RouteAccess::OwnerLocal`] is denied whatever the authorizer
//! says.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What a route lets its caller do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouteAccess {
    /// Read state and receive output.
    Observe,
    /// Change what runs or what is shown.
    Input,
    /// Change durable configuration of the runtime.
    Manage,
    /// Never reachable from an admitted stream.
    OwnerLocal,
}

impl RouteAccess {
    /// Every class, in BPDL spelling order.
    pub const ALL: [Self; 4] = [Self::Observe, Self::Input, Self::Manage, Self::OwnerLocal];

    /// The BPDL / manifest spelling (`observe`, `input`, `manage`, `owner-local`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Input => "input",
            Self::Manage => "manage",
            Self::OwnerLocal => "owner-local",
        }
    }
}

impl fmt::Display for RouteAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error parsing a [`RouteAccess`] spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown route access class `{0}` (expected observe, input, manage or owner-local)")]
pub struct ParseRouteAccessError(pub String);

impl FromStr for RouteAccess {
    type Err = ParseRouteAccessError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|access| access.as_str() == value)
            .ok_or_else(|| ParseRouteAccessError(value.to_string()))
    }
}

/// Decides whether a remote subject may use a class of routes.
pub trait RemoteAccessAuthorizer: Send + Sync {
    /// Called for every request on an admitted stream; must not block.
    fn allows(&self, subject_id: Uuid, access: RouteAccess) -> bool;
}

/// Registered in `PluginStateRegistry` by the plugin that owns roles (permissions).
#[derive(Clone)]
pub struct RemoteAccessAuthorizerHandle(Arc<dyn RemoteAccessAuthorizer>);

impl fmt::Debug for RemoteAccessAuthorizerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteAccessAuthorizerHandle(..)")
    }
}

impl RemoteAccessAuthorizerHandle {
    /// Wrap an authorizer for registration.
    #[must_use]
    pub fn new(authorizer: Arc<dyn RemoteAccessAuthorizer>) -> Self {
        Self(authorizer)
    }

    /// Ask the authorizer. [`RouteAccess::OwnerLocal`] is always denied.
    #[must_use]
    pub fn allows(&self, subject_id: Uuid, access: RouteAccess) -> bool {
        access != RouteAccess::OwnerLocal && self.0.allows(subject_id, access)
    }
}

/// The single rule for a request on an admitted stream.
///
/// `access` is the route's declared class (`None` when undeclared) and
/// `authorizer` the registered handle, if any. Returns why it is denied.
///
/// # Errors
/// Returns a caller-facing denial reason.
pub fn authorize_remote_access(
    authorizer: Option<&RemoteAccessAuthorizerHandle>,
    subject_id: Uuid,
    access: Option<RouteAccess>,
) -> Result<(), RemoteAccessDenied> {
    let access = access.ok_or(RemoteAccessDenied::Undeclared)?;
    if access == RouteAccess::OwnerLocal {
        return Err(RemoteAccessDenied::OwnerLocal);
    }
    let authorizer = authorizer.ok_or(RemoteAccessDenied::NoAuthorizer)?;
    if authorizer.allows(subject_id, access) {
        Ok(())
    } else {
        Err(RemoteAccessDenied::NotGranted(access))
    }
}

/// Why a request on an admitted stream was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RemoteAccessDenied {
    /// The route declares no access class.
    #[error("route declares no access class and is not reachable remotely")]
    Undeclared,
    /// The route is owner-local.
    #[error("route is owner-local and not reachable remotely")]
    OwnerLocal,
    /// No plugin registered a remote access authorizer.
    #[error("no remote access authorizer is registered")]
    NoAuthorizer,
    /// The authorizer refused this class for the subject.
    #[error("remote principal is not granted `{0}` access")]
    NotGranted(RouteAccess),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Allow(Vec<RouteAccess>);
    impl RemoteAccessAuthorizer for Allow {
        fn allows(&self, _: Uuid, access: RouteAccess) -> bool {
            self.0.contains(&access)
        }
    }

    #[test]
    fn spelling_round_trips() {
        for access in RouteAccess::ALL {
            assert_eq!(access.as_str().parse::<RouteAccess>().unwrap(), access);
            assert_eq!(
                serde_json::to_string(&access).unwrap(),
                format!("\"{}\"", access.as_str())
            );
        }
        assert!("owner_local".parse::<RouteAccess>().is_err());
    }

    #[test]
    fn rule_denies_undeclared_owner_local_and_missing_authorizer() {
        let subject = Uuid::new_v4();
        let everything =
            RemoteAccessAuthorizerHandle::new(Arc::new(Allow(RouteAccess::ALL.to_vec())));
        let observe =
            RemoteAccessAuthorizerHandle::new(Arc::new(Allow(vec![RouteAccess::Observe])));
        assert_eq!(
            authorize_remote_access(Some(&everything), subject, None),
            Err(RemoteAccessDenied::Undeclared)
        );
        assert_eq!(
            authorize_remote_access(Some(&everything), subject, Some(RouteAccess::OwnerLocal)),
            Err(RemoteAccessDenied::OwnerLocal)
        );
        assert!(!everything.allows(subject, RouteAccess::OwnerLocal));
        assert_eq!(
            authorize_remote_access(None, subject, Some(RouteAccess::Observe)),
            Err(RemoteAccessDenied::NoAuthorizer)
        );
        assert_eq!(
            authorize_remote_access(Some(&observe), subject, Some(RouteAccess::Observe)),
            Ok(())
        );
        assert_eq!(
            authorize_remote_access(Some(&observe), subject, Some(RouteAccess::Input)),
            Err(RemoteAccessDenied::NotGranted(RouteAccess::Input))
        );
        assert_eq!(
            authorize_remote_access(Some(&everything), subject, Some(RouteAccess::Manage)),
            Ok(())
        );
    }
}
