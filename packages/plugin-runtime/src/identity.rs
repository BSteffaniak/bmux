//! Host-issued identities. Logical plugin names are deliberately not scope keys.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! identity {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub(crate) fn fresh() -> Self {
                Self(Uuid::new_v4())
            }

            /// Lossless boundary conversion, not proof of admission or authority.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, formatter)
            }
        }
    };
}

identity!(PluginRuntimeScopeId);
identity!(PluginInstanceId);
identity!(PluginActivationId);
identity!(RuntimeInvocationId);
identity!(PluginResourceId);

impl PluginRuntimeScopeId {
    /// Mint a fresh host runtime identity, independent of logical plugin names.
    #[must_use]
    pub fn new() -> Self {
        Self::fresh()
    }
}

impl Default for PluginRuntimeScopeId {
    fn default() -> Self {
        Self::new()
    }
}
