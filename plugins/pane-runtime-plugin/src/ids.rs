//! Identifiers for panes, sessions and the other objects the runtime creates.
//!
//! Natively they are random (version 4) UUIDs. In a simulated run (the `simulator` feature)
//! their random bits come from the run's seed, so a run replays with the same identifiers, and
//! from one stream for the whole run, so a server restarted within it never repeats one.

use uuid::Uuid;

/// A new identifier.
#[must_use]
pub(crate) fn new_uuid() -> Uuid {
    #[cfg(feature = "simulator")]
    {
        let mut bytes = [0_u8; 16];
        switchy_simulation::stream("bmux_pane_runtime_ids")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fill_bytes(&mut bytes);
        uuid::Builder::from_random_bytes(bytes).into_uuid()
    }
    #[cfg(not(feature = "simulator"))]
    Uuid::new_v4()
}
