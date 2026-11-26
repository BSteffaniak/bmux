//! When a client that lost its server tries again: one backoff schedule for
//! every long-lived client.

use std::time::Duration;

/// Reconnect attempts in a row that must find the attached target gone to end.
///
/// A server that has just come back may answer before it has restored what it
/// serves, so one answer is not proof.
pub const GONE_CONFIRMATIONS: u32 = 2;

/// How long to wait before each reconnect attempt: doubling from `base` up to
/// `max`, spread by up to `jitter_permille` thousandths either way so clients
/// that lost the same server do not all come back at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectBackoff {
    base: Duration,
    max: Duration,
    jitter_permille: u64,
}

impl ReconnectBackoff {
    /// Interactive clients: 300ms doubling to 5s, ±20%.
    pub const INTERACTIVE: Self = Self {
        base: Duration::from_millis(300),
        max: Duration::from_secs(5),
        jitter_permille: 200,
    };

    /// A schedule without jitter (tests time it exactly).
    #[must_use]
    pub const fn fixed(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            jitter_permille: 0,
        }
    }

    /// The delay before attempt `attempt` (from 1); `seed` picks the jitter.
    #[must_use]
    pub fn delay(self, attempt: u32, seed: u64) -> Duration {
        let exponent = attempt.saturating_sub(1).min(20);
        let nominal = self.base.saturating_mul(1 << exponent).min(self.max);
        if self.jitter_permille == 0 {
            return nominal;
        }
        let spread = splitmix64(seed ^ u64::from(attempt)) % (2 * self.jitter_permille + 1);
        let nominal_ms = u64::try_from(nominal.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(
            nominal_ms.saturating_mul(1000 - self.jitter_permille + spread) / 1000,
        )
    }
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// A jitter seed that differs between processes and runs.
#[must_use]
pub fn jitter_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    splitmix64(u64::from(nanos) ^ (u64::from(std::process::id()) << 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_backoff_doubles_to_its_cap_within_its_jitter() {
        let nominal = [300, 600, 1_200, 2_400, 4_800, 5_000, 5_000];
        for seed in [0, 1, 42, u64::MAX] {
            for (index, nominal) in nominal.into_iter().enumerate() {
                let attempt = u32::try_from(index + 1).unwrap();
                let delay = ReconnectBackoff::INTERACTIVE
                    .delay(attempt, seed)
                    .as_millis();
                assert!(
                    (nominal * 8 / 10..=nominal * 12 / 10).contains(&delay),
                    "attempt {attempt}: {delay}ms is not within 20% of {nominal}ms"
                );
            }
        }
        // Different clients spread out.
        let spread: std::collections::BTreeSet<_> = (0..16)
            .map(|seed| ReconnectBackoff::INTERACTIVE.delay(3, seed))
            .collect();
        assert!(spread.len() > 1);
    }
}
