//! Derived output invalidation; swapchain images are never assumed to preserve contents.
#[derive(Debug, Default)]
pub(crate) struct DamageTracker {
    generation: u64,
    invalidated: bool,
}
impl DamageTracker {
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }
    pub fn committed(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.invalidated = false;
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_output_does_not_advance_generation() {
        let mut damage = DamageTracker::default();
        damage.invalidate();
        assert_eq!(damage.generation, 0);
        assert_eq!(damage.committed(), 1);
        damage.invalidate();
        assert_eq!(damage.generation, 1);
    }
}
