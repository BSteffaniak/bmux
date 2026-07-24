//! Per-process routing map for plugin services.
//!
//! Typed service calls between plugins must resolve to the activated provider
//! instance. A provider may be activated in this process or reachable through
//! a domain-neutral endpoint. The compatibility endpoint is the current host
//! kernel bridge used by attach and standalone CLI processes.
//!
//! Providers that are not registered are left to the loader's legacy local
//! fallback for pre-bootstrap and test paths.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

/// Stable identifier for an endpoint capable of receiving typed service calls.
///
/// Endpoint IDs are opaque to the plugin runtime. The connections service owns
/// target resolution and transport details.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceEndpoint(Arc<str>);

impl ServiceEndpoint {
    /// Identifier for the existing one-server host kernel bridge route.
    pub const HOST_KERNEL_ID: &'static str = "host-kernel";

    /// Construct an endpoint identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceEndpointError`] when `value` is empty, has surrounding
    /// whitespace, or contains control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, ServiceEndpointError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ServiceEndpointError::Empty);
        }
        if value.trim() != value {
            return Err(ServiceEndpointError::SurroundingWhitespace);
        }
        if value.chars().any(char::is_control) {
            return Err(ServiceEndpointError::ControlCharacter);
        }
        Ok(Self(Arc::from(value)))
    }

    /// The endpoint used by the existing host kernel bridge path.
    #[must_use]
    pub fn host_kernel() -> Self {
        Self(Arc::from(Self::HOST_KERNEL_ID))
    }

    /// Borrow the opaque endpoint identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the compatibility host kernel bridge endpoint.
    #[must_use]
    pub fn is_host_kernel(&self) -> bool {
        self.as_str() == Self::HOST_KERNEL_ID
    }
}

impl fmt::Display for ServiceEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a service endpoint identifier was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceEndpointError {
    /// Endpoint identifiers must not be empty.
    Empty,
    /// Endpoint identifiers are exact and cannot have surrounding whitespace.
    SurroundingWhitespace,
    /// Endpoint identifiers cannot contain control characters.
    ControlCharacter,
}

impl fmt::Display for ServiceEndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "service endpoint id cannot be empty",
            Self::SurroundingWhitespace => {
                "service endpoint id cannot contain surrounding whitespace"
            }
            Self::ControlCharacter => "service endpoint id cannot contain control characters",
        })
    }
}

impl std::error::Error for ServiceEndpointError {}

/// Where a plugin's service handlers live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceLocation {
    /// The current process activated this plugin; dispatch in-process.
    Local,
    /// Another endpoint activated this plugin; route calls to that endpoint.
    Remote {
        /// Opaque endpoint identity understood by connection plumbing.
        endpoint: ServiceEndpoint,
    },
}

/// Process-wide map of plugin id to its service route.
#[derive(Default, Debug)]
pub struct ServiceLocationMap {
    entries: RwLock<HashMap<String, ServiceLocation>>,
}

impl ServiceLocationMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `plugin_id` is activated in the current process.
    ///
    /// Replaces any prior entry.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn mark_local(&self, plugin_id: &str) {
        let mut guard = self
            .entries
            .write()
            .expect("service location map lock poisoned");
        guard.insert(plugin_id.to_string(), ServiceLocation::Local);
    }

    /// Record that `plugin_id` is reachable through the compatibility host
    /// kernel bridge endpoint.
    ///
    /// Does not replace an existing local route.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn mark_remote(&self, plugin_id: &str) {
        self.mark_remote_endpoint(plugin_id, ServiceEndpoint::host_kernel());
    }

    /// Record that `plugin_id` is reachable through `endpoint`.
    ///
    /// Replaces an existing remote route, allowing endpoint selection to be
    /// updated, but never replaces an activated local provider.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn mark_remote_endpoint(&self, plugin_id: &str, endpoint: ServiceEndpoint) {
        let mut guard = self
            .entries
            .write()
            .expect("service location map lock poisoned");
        match guard.get(plugin_id) {
            Some(ServiceLocation::Local) => {}
            Some(ServiceLocation::Remote { .. }) | None => {
                guard.insert(plugin_id.to_string(), ServiceLocation::Remote { endpoint });
            }
        }
    }

    /// Look up the recorded route for `plugin_id`.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<ServiceLocation> {
        self.entries
            .read()
            .expect("service location map lock poisoned")
            .get(plugin_id)
            .cloned()
    }

    /// Remove every recorded entry. Test-only helper.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn clear(&self) {
        self.entries
            .write()
            .expect("service location map lock poisoned")
            .clear();
    }

    /// Number of recorded entries.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("service location map lock poisoned")
            .len()
    }

    /// `true` when no entries are recorded.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide shared [`ServiceLocationMap`] instance.
#[must_use]
pub fn global_service_locations() -> Arc<ServiceLocationMap> {
    static GLOBAL: OnceLock<Arc<ServiceLocationMap>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(ServiceLocationMap::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{
        ServiceEndpoint, ServiceEndpointError, ServiceLocation, ServiceLocationMap,
        global_service_locations,
    };
    use std::sync::Arc;

    #[test]
    fn endpoint_validation_preserves_opaque_identity() {
        let endpoint = ServiceEndpoint::new("region-a/server-2").unwrap();
        assert_eq!(endpoint.as_str(), "region-a/server-2");
        assert!(!endpoint.is_host_kernel());
        assert_eq!(ServiceEndpoint::new(""), Err(ServiceEndpointError::Empty));
        assert_eq!(
            ServiceEndpoint::new(" endpoint "),
            Err(ServiceEndpointError::SurroundingWhitespace)
        );
        assert_eq!(
            ServiceEndpoint::new("endpoint\n"),
            Err(ServiceEndpointError::SurroundingWhitespace)
        );
    }

    #[test]
    fn mark_local_records_local_location() {
        let map = ServiceLocationMap::new();
        map.mark_local("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn mark_remote_uses_host_kernel_compatibility_endpoint() {
        let map = ServiceLocationMap::new();
        map.mark_remote("bmux.contexts");
        assert_eq!(
            map.get("bmux.contexts"),
            Some(ServiceLocation::Remote {
                endpoint: ServiceEndpoint::host_kernel()
            })
        );
    }

    #[test]
    fn explicit_remote_endpoint_can_be_reselected() {
        let map = ServiceLocationMap::new();
        map.mark_remote_endpoint("bmux.contexts", ServiceEndpoint::new("endpoint-a").unwrap());
        map.mark_remote_endpoint("bmux.contexts", ServiceEndpoint::new("endpoint-b").unwrap());
        assert_eq!(
            map.get("bmux.contexts"),
            Some(ServiceLocation::Remote {
                endpoint: ServiceEndpoint::new("endpoint-b").unwrap()
            })
        );
    }

    #[test]
    fn remote_routes_do_not_override_local() {
        let map = ServiceLocationMap::new();
        map.mark_local("bmux.contexts");
        map.mark_remote_endpoint("bmux.contexts", ServiceEndpoint::new("endpoint-a").unwrap());
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn mark_local_overrides_remote() {
        let map = ServiceLocationMap::new();
        map.mark_remote("bmux.contexts");
        map.mark_local("bmux.contexts");
        assert_eq!(map.get("bmux.contexts"), Some(ServiceLocation::Local));
    }

    #[test]
    fn get_missing_plugin_returns_none() {
        let map = ServiceLocationMap::new();
        assert_eq!(map.get("bmux.unknown"), None);
    }

    #[test]
    fn concurrent_mark_and_get_is_safe() {
        use std::thread;

        let map = Arc::new(ServiceLocationMap::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let map = Arc::clone(&map);
            handles.push(thread::spawn(move || {
                for j in 0..500 {
                    let id = format!("plugin-{i}-{}", j % 16);
                    if j % 2 == 0 {
                        map.mark_local(&id);
                    } else {
                        map.mark_remote(&id);
                    }
                    let _ = map.get(&id);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(!map.is_empty());
    }

    #[test]
    fn clear_resets_all_entries() {
        let map = ServiceLocationMap::new();
        map.mark_local("a");
        map.mark_remote("b");
        assert_eq!(map.len(), 2);
        map.clear();
        assert!(map.is_empty());
    }

    #[test]
    fn global_service_locations_returns_same_instance() {
        let a = global_service_locations();
        let b = global_service_locations();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
