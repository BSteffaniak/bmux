//! Remote roles live in the permissions plugin; connections sets them through
//! its typed BPDL commands.
use crate::trust::Role;
use anyhow::{Result, ensure};
use bmux_permissions_plugin_api::remote_role_types::RemoteRole;
use uuid::Uuid;

/// The grant side of the approval transition.
pub trait RoleGrants {
    fn set(&self, principal: Uuid, role: Role) -> Result<()>;
    fn clear(&self, principal: Uuid) -> Result<()>;
    fn get(&self, principal: Uuid) -> Result<Option<Role>>;
}

const fn to_api(role: Role) -> RemoteRole {
    match role {
        Role::Full => RemoteRole::Full,
        Role::ViewOnly => RemoteRole::ViewOnly,
    }
}

const fn from_api(role: RemoteRole) -> Role {
    match role {
        RemoteRole::Full => Role::Full,
        RemoteRole::ViewOnly => Role::ViewOnly,
    }
}

/// Grants through the permissions plugin, from inside a service or lifecycle call.
pub struct PermissionsGrants<'a, C: bmux_plugin::ServiceCaller + Sync + ?Sized> {
    pub caller: &'a C,
}

impl<C: bmux_plugin::ServiceCaller + Sync + ?Sized> RoleGrants for PermissionsGrants<'_, C> {
    fn set(&self, principal: Uuid, role: Role) -> Result<()> {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self.caller);
        let ack = bmux_plugin::block_on_typed_dispatch(
            bmux_permissions_plugin_api::remote_role_commands::client::set_remote_role(
                &mut client,
                principal,
                to_api(role),
            ),
        )
        .map_err(|error| anyhow::anyhow!("permissions plugin unavailable: {error}"))?;
        ensure!(ack.ok, "the permissions plugin refused the role");
        Ok(())
    }

    fn clear(&self, principal: Uuid) -> Result<()> {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self.caller);
        let ack = bmux_plugin::block_on_typed_dispatch(
            bmux_permissions_plugin_api::remote_role_commands::client::clear_remote_role(
                &mut client,
                principal,
            ),
        )
        .map_err(|error| anyhow::anyhow!("permissions plugin unavailable: {error}"))?;
        ensure!(ack.ok, "the permissions plugin refused to clear the role");
        Ok(())
    }

    fn get(&self, principal: Uuid) -> Result<Option<Role>> {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self.caller);
        bmux_plugin::block_on_typed_dispatch(
            bmux_permissions_plugin_api::remote_role_state::client::remote_role(
                &mut client,
                principal,
            ),
        )
        .map(|role| role.map(from_api))
        .map_err(|error| anyhow::anyhow!("permissions plugin unavailable: {error}"))
    }
}

#[cfg(test)]
pub mod memory {
    use super::{Role, RoleGrants};
    use anyhow::{Result, bail};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// In-memory grants for tests; `fail` makes `set` fail.
    #[derive(Default)]
    pub struct MemoryGrants {
        pub roles: Mutex<BTreeMap<Uuid, Role>>,
        pub fail: std::sync::atomic::AtomicBool,
    }

    impl RoleGrants for MemoryGrants {
        fn set(&self, principal: Uuid, role: Role) -> Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                bail!("injected grant failure");
            }
            self.roles.lock().unwrap().insert(principal, role);
            Ok(())
        }
        fn clear(&self, principal: Uuid) -> Result<()> {
            self.roles.lock().unwrap().remove(&principal);
            Ok(())
        }
        fn get(&self, principal: Uuid) -> Result<Option<Role>> {
            Ok(self.roles.lock().unwrap().get(&principal).copied())
        }
    }
}
