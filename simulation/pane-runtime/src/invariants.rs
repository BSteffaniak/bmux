//! The pane runtime simulation's invariants (see the crate documentation, which lists them in
//! this order).

/// One of the simulation's invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invariant {
    SessionLeader,
    TypedInOrder,
    RequestedSize,
    CloseCleansUp,
    Finishes,
    CrashSessionsEnd,
    RestartEndsSession,
    ExitOnce,
    NothingLeft,
    EndedSaysSo,
    SiblingsAlone,
    BystanderHarmless,
    ScreenFromInput,
    OutputDelivered,
    ReplicaLevel,
    AttachSnapshot,
    Reservations,
    ExitCode,
}

impl sim_support::invariants::Invariant for Invariant {
    const ALL: &'static [Self] = &[
        Self::SessionLeader,
        Self::TypedInOrder,
        Self::RequestedSize,
        Self::CloseCleansUp,
        Self::Finishes,
        Self::CrashSessionsEnd,
        Self::RestartEndsSession,
        Self::ExitOnce,
        Self::NothingLeft,
        Self::EndedSaysSo,
        Self::SiblingsAlone,
        Self::BystanderHarmless,
        Self::ScreenFromInput,
        Self::OutputDelivered,
        Self::ReplicaLevel,
        Self::AttachSnapshot,
        Self::Reservations,
        Self::ExitCode,
    ];

    fn statement(self) -> &'static str {
        match self {
            Self::SessionLeader => "an opened pane leads its own session",
            Self::TypedInOrder => "typed lines reach the program in order",
            Self::RequestedSize => "the terminal ends up the requested size",
            Self::CloseCleansUp => "closing drains, cleans up, leaves nothing running",
            Self::Finishes => "the workload finishes and its threads end",
            Self::CrashSessionsEnd => "a crash's sessions end, and only they",
            Self::RestartEndsSession => "restarting ends the old session",
            Self::ExitOnce => "each program's exit is reported at most once",
            Self::NothingLeft => "nothing is left once every pane closed",
            Self::EndedSaysSo => "a pane whose program ended says so",
            Self::SiblingsAlone => "closing a pane leaves its siblings alone",
            Self::BystanderHarmless => "a runtime activated beside the server kills nothing",
            Self::ScreenFromInput => "a pane's screen is what its input made it",
            Self::OutputDelivered => "a pane takes in what its terminal gave it, in order",
            Self::ReplicaLevel => "a client's replica of a pane shows what the pane shows",
            Self::AttachSnapshot => "an attach snapshot shows a state the pane passed through",
            Self::Reservations => "a mutation's reservation names one execution; a crash's retire",
            Self::ExitCode => "an execution's reported exit code is its program's",
        }
    }
}

#[cfg(test)]
mod tests {
    use sim_support::invariants::Invariant as _;

    /// The crate documentation numbers the invariants as they are declared.
    #[test]
    fn the_documentation_lists_every_invariant_in_order() {
        let docs = include_str!("lib.rs");
        for invariant in super::Invariant::ALL {
            let number = invariant.number().unwrap();
            assert!(
                docs.contains(&format!("//! {number}. ")),
                "lib.rs documents no invariant {number} ({invariant:?})"
            );
        }
    }
}
