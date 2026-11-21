//! State the pane runtime shares between its activations.
//!
//! Some bounds (writer threads, cleanup tasks, probes) are per process natively: every
//! activation in the process draws from them. Concurrent simulated runs in one process must not
//! share them, or one run's load would change what another does; with the `simulator` feature
//! each run has its own.

use switchy_thread::sync::Arc;

/// The value keyed by `K`, created with `init` on first use: one per process natively, one per
/// simulated run with the `simulator` feature.
pub(crate) fn run_shared<K: 'static, T: Send + Sync + 'static>(init: impl FnOnce() -> T) -> Arc<T> {
    #[cfg(feature = "simulator")]
    {
        /// A run's value for key `K` (one state slot per key, even for values of one type).
        struct Keyed<K, T>(Arc<T>, std::marker::PhantomData<fn() -> K>);
        Arc::clone(
            &switchy_simulation::state_with(|| {
                Keyed::<K, T>(Arc::new(init()), std::marker::PhantomData)
            })
            .0,
        )
    }
    #[cfg(not(feature = "simulator"))]
    {
        use std::{
            any::{Any, TypeId},
            collections::BTreeMap,
            sync::{LazyLock, Mutex, PoisonError},
        };
        type Shared = BTreeMap<TypeId, Arc<dyn Any + Send + Sync>>;
        static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(Mutex::default);
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        let value = Arc::clone(
            shared
                .entry(TypeId::of::<K>())
                .or_insert_with(|| Arc::new(init())),
        );
        drop(shared);
        value
            .downcast::<T>()
            .unwrap_or_else(|_| unreachable!("each key holds one type"))
    }
}
