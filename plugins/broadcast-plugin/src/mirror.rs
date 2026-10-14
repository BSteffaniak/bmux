//! Revision validation for the asynchronous attach mirror.
#[derive(Default)]
pub(super) struct Mirror {
    revision: Option<u64>,
    pub established: bool,
}
impl Mirror {
    pub fn reconnect(&mut self) {
        self.revision = None;
    }
    pub fn accept(&mut self, state: &str, revision: u64) -> bool {
        if !["live", "paused", "offline", "unavailable"].contains(&state) {
            return false;
        }
        if self.revision.is_some_and(|previous| revision <= previous) {
            return false;
        }
        self.revision = Some(revision);
        self.established = true;
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_unknown_and_reconnect() {
        let mut mirror = Mirror::default();
        assert!(!mirror.established);
        assert!(mirror.accept("live", 4));
        assert!(!mirror.accept("paused", 3));
        assert!(!mirror.accept("unknown", 5));
        assert!(!mirror.accept("offline", 4));
        mirror.reconnect();
        assert!(mirror.accept("offline", 1));
        assert!(mirror.established);
    }
}
