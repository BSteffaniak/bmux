//! Host-provided, activation-owned in-process capabilities.
//!
//! A host program declares what it offers a component's activation by
//! registering an activation binder with the loader. The loader calls that
//! binder with the activation's actual scope and delivers whatever it inserts
//! here through [`super::PluginLifecycleContext::capabilities`]. The set is
//! keyed by Rust type, so it carries no domain vocabulary of its own: the
//! value types belong to the crates that define them.
//!
//! Values are live Rust objects and never cross the native ABI (the field is
//! not serialized). Only in-process (static) components can receive them;
//! loaders refuse to bind activation binders to dynamic artifacts instead of
//! silently activating them without the capability.
use crate::PluginCommandError;
use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

type Slots = BTreeMap<TypeId, Box<dyn Any + Send>>;

/// A typed set of host values for exactly one activation.
///
/// Each value is taken at most once; clones share one set, so taking through
/// any clone consumes it. Untaken values are dropped with the last clone
/// (after `activate` settles).
#[derive(Clone, Default)]
pub struct ActivationCapabilities(Arc<Mutex<Slots>>);

impl ActivationCapabilities {
    /// Offer one value of type `T` to the activation.
    ///
    /// # Errors
    /// Rejects a second value of the same type (two binders must not race to
    /// supply one capability) and a poisoned set.
    pub fn insert<T: Send + 'static>(&self, value: T) -> Result<(), PluginCommandError> {
        let mut slots = self.lock()?;
        if slots.contains_key(&TypeId::of::<T>()) {
            return Err(PluginCommandError::failed(format!(
                "activation capability {} supplied twice",
                std::any::type_name::<T>()
            )));
        }
        slots.insert(TypeId::of::<T>(), Box::new(value));
        drop(slots);
        Ok(())
    }

    /// Take the activation's value of type `T`, if the host supplied one.
    ///
    /// # Errors
    /// Returns an explicit unavailable error when the host did not supply it,
    /// it was already taken, or the set is poisoned. There is no global
    /// fallback.
    pub fn take<T: Send + 'static>(&self) -> Result<T, PluginCommandError> {
        let value = self.lock()?.remove(&TypeId::of::<T>()).ok_or_else(|| {
            PluginCommandError::unavailable(format!(
                "activation capability {} not supplied by host",
                std::any::type_name::<T>()
            ))
        })?;
        value.downcast::<T>().map(|value| *value).map_err(|_| {
            PluginCommandError::failed("activation capability type mismatch".to_string())
        })
    }

    /// Whether any value is still present (not yet taken).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.lock().map_or(true, |slots| slots.is_empty())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Slots>, PluginCommandError> {
        self.0
            .lock()
            .map_err(|_| PluginCommandError::failed("activation capabilities poisoned".to_string()))
    }
}

// Identity equality: two sets holding values are equal only when they are
// the same set (clones of one another), never because they hold equal
// values. Two sets with nothing left to take are interchangeable, so a
// context round-tripped through the wire (which never carries capabilities)
// still compares equal to its source.
impl PartialEq for ActivationCapabilities {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || (self.is_empty() && other.is_empty())
    }
}

impl std::fmt::Debug for ActivationCapabilities {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.0.lock().map_or(0, |slots| slots.len());
        formatter
            .debug_struct("ActivationCapabilities")
            .field("count", &count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_typed_taken_once_and_shared_across_clones() {
        let capabilities = ActivationCapabilities::default();
        capabilities.insert(7_u32).unwrap();
        assert!(capabilities.insert(8_u32).is_err());
        capabilities.insert(String::from("port")).unwrap();
        let clone = capabilities.clone();
        assert_eq!(clone.take::<u32>().unwrap(), 7);
        assert!(capabilities.take::<u32>().is_err());
        assert_eq!(capabilities.take::<String>().unwrap(), "port");
        assert!(clone.is_empty());
        assert!(clone.take::<u64>().is_err());
    }

    #[test]
    fn equality_is_identity_unless_both_are_empty() {
        let full = ActivationCapabilities::default();
        full.insert(1_u8).unwrap();
        let other = ActivationCapabilities::default();
        other.insert(1_u8).unwrap();
        assert_eq!(full, full.clone());
        assert_ne!(full, other);
        assert_ne!(full, ActivationCapabilities::default());
        assert_eq!(
            ActivationCapabilities::default(),
            ActivationCapabilities::default()
        );
    }

    #[test]
    fn debug_reports_count_without_values() {
        let capabilities = ActivationCapabilities::default();
        capabilities.insert(1_u8).unwrap();
        assert_eq!(
            format!("{capabilities:?}"),
            "ActivationCapabilities { count: 1 }"
        );
    }
}
