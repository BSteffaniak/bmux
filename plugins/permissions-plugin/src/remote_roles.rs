//! Roles of remote principals (paired devices) and the B2 authorizer the
//! server consults for every request on an admitted stream.
//!
//! The in-memory map is the authority (the authorizer never blocks); every
//! change is persisted through plugin storage before it becomes visible.
use bmux_permissions_plugin_api::remote_role_types::RemoteRole;
use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{
    RemoteAccessAuthorizer, RemoteAccessAuthorizerHandle, RouteAccess, StorageGetRequest,
    StorageSetRequest,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use uuid::Uuid;

type Roles = Arc<RwLock<BTreeMap<Uuid, RemoteRole>>>;

fn storage_key() -> bmux_plugin_sdk::StorageKey {
    bmux_plugin_sdk::storage_key!("remote-roles-v1")
}

/// `full` allows observe, input and manage; `view-only` allows observe.
/// Nothing allows owner-local, and unknown principals get nothing.
#[must_use]
pub const fn role_allows(role: RemoteRole, access: RouteAccess) -> bool {
    match (role, access) {
        (_, RouteAccess::OwnerLocal) => false,
        (RemoteRole::Full, _) | (RemoteRole::ViewOnly, RouteAccess::Observe) => true,
        (RemoteRole::ViewOnly, _) => false,
    }
}

struct Authorizer(Roles);

impl RemoteAccessAuthorizer for Authorizer {
    fn allows(&self, subject_id: Uuid, access: RouteAccess) -> bool {
        self.0
            .read()
            .ok()
            .and_then(|roles| roles.get(&subject_id).copied())
            .is_some_and(|role| role_allows(role, access))
    }
}

#[derive(Default)]
pub struct RemoteRoles {
    roles: Roles,
    mutation: Mutex<()>,
    /// The activation's registry view; dropping it withdraws the authorizer.
    registry: Mutex<Option<Arc<bmux_plugin::PluginStateRegistry>>>,
}

fn decode(bytes: &[u8]) -> Result<BTreeMap<Uuid, RemoteRole>, String> {
    serde_json::from_slice(bytes).map_err(|_| "invalid stored remote roles".to_owned())
}

impl RemoteRoles {
    /// The handle the server consults; it reads this instance's live map.
    #[must_use]
    pub fn authorizer(&self) -> RemoteAccessAuthorizerHandle {
        RemoteAccessAuthorizerHandle::new(Arc::new(Authorizer(Arc::clone(&self.roles))))
    }

    /// Load persisted roles, then register the authorizer in the host registry.
    pub fn activate(&self, context: &bmux_plugin_sdk::NativeLifecycleContext) {
        match Self::load(context) {
            Ok(loaded) => {
                if let Ok(mut roles) = self.roles.write() {
                    *roles = loaded;
                }
            }
            // Fail closed: without stored roles no remote principal has access.
            Err(error) => tracing::warn!("remote roles unavailable: {error}"),
        }
        let registry = bmux_plugin::activation_state_registry(context);
        let _previous = registry.register(&Arc::new(RwLock::new(self.authorizer())));
        if let Ok(mut slot) = self.registry.lock() {
            *slot = Some(registry);
        }
    }

    pub fn deactivate(&self) {
        if let Ok(mut slot) = self.registry.lock() {
            slot.take();
        }
    }

    fn load(caller: &(impl HostRuntimeApi + ?Sized)) -> Result<BTreeMap<Uuid, RemoteRole>, String> {
        let response = caller
            .storage_get(&StorageGetRequest::new(storage_key()))
            .map_err(|error| error.to_string())?;
        response
            .value
            .map_or_else(|| Ok(BTreeMap::new()), |bytes| decode(&bytes))
    }

    #[cfg(test)]
    pub fn load_for_test(
        caller: &(impl HostRuntimeApi + ?Sized),
    ) -> Result<BTreeMap<Uuid, RemoteRole>, String> {
        Self::load(caller)
    }

    /// Set (`Some`) or clear (`None`) a principal's role, durably first.
    pub fn set(
        &self,
        caller: &(impl HostRuntimeApi + ?Sized),
        principal: Uuid,
        role: Option<RemoteRole>,
    ) -> Result<(), String> {
        if principal.is_nil() {
            return Err("remote principal must not be nil".into());
        }
        let _guard = self
            .mutation
            .lock()
            .map_err(|_| "remote role lock poisoned")?;
        let mut next = self
            .roles
            .read()
            .map_err(|_| "remote role lock poisoned")?
            .clone();
        match role {
            Some(role) => next.insert(principal, role),
            None => next.remove(&principal),
        };
        let bytes = serde_json::to_vec(&next).map_err(|error| error.to_string())?;
        caller
            .storage_set(&StorageSetRequest::new(storage_key(), bytes))
            .map_err(|error| error.to_string())?;
        *self
            .roles
            .write()
            .map_err(|_| "remote role lock poisoned")? = next;
        Ok(())
    }

    #[must_use]
    pub fn get(&self, principal: Uuid) -> Option<RemoteRole> {
        self.roles.read().ok()?.get(&principal).copied()
    }

    /// The session-policy role a remote principal acts with, if any.
    #[must_use]
    pub fn policy_role(&self, principal: Uuid) -> Option<&'static str> {
        self.get(principal).map(|role| match role {
            RemoteRole::Full => "writer",
            RemoteRole::ViewOnly => "observer",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matrix() {
        use RouteAccess::{Input, Manage, Observe, OwnerLocal};
        for (role, allowed) in [
            (RemoteRole::Full, [true, true, true, false]),
            (RemoteRole::ViewOnly, [true, false, false, false]),
        ] {
            for (access, expected) in [Observe, Input, Manage, OwnerLocal]
                .into_iter()
                .zip(allowed)
            {
                assert_eq!(role_allows(role, access), expected, "{role:?} {access:?}");
            }
        }
    }

    #[test]
    fn authorizer_reads_the_live_map_and_denies_unknown_principals() {
        let roles = RemoteRoles::default();
        let handle = roles.authorizer();
        let principal = Uuid::new_v4();
        assert!(!handle.allows(principal, RouteAccess::Observe));
        roles
            .roles
            .write()
            .unwrap()
            .insert(principal, RemoteRole::ViewOnly);
        assert!(handle.allows(principal, RouteAccess::Observe));
        assert!(!handle.allows(principal, RouteAccess::Input));
        roles
            .roles
            .write()
            .unwrap()
            .insert(principal, RemoteRole::Full);
        assert!(handle.allows(principal, RouteAccess::Manage));
        assert!(!handle.allows(principal, RouteAccess::OwnerLocal));
        assert!(!handle.allows(Uuid::new_v4(), RouteAccess::Observe));
        assert_eq!(roles.policy_role(principal), Some("writer"));
    }
}
