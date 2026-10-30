//! Native font policy, independent of window zoom and device pixel density.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Logical typography before any window (or future surface-local) scale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TypographyConfiguration {
    pub size: f32,
    pub family: Option<String>,
    /// Ordered fallback families; platform fallback remains the final authority.
    pub fallback: Vec<String>,
    /// Multiplier of natural line metrics, not an independently scaled size.
    pub line_height: f32,
    /// Additional logical pixels between cells before presentation scaling.
    pub letter_spacing: f32,
}

impl Default for TypographyConfiguration {
    fn default() -> Self {
        Self {
            size: 16.0,
            family: None,
            fallback: Vec::new(),
            line_height: 1.0,
            letter_spacing: 0.0,
        }
    }
}

impl TypographyConfiguration {
    /// Reject invalid configuration instead of silently normalizing it. Limits
    /// bound font lookup work and prevent degenerate or overflowing geometry.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.size.is_finite() && (4.0..=128.0).contains(&self.size),
            "typography.size must be finite and between 4 and 128"
        );
        ensure!(
            self.line_height.is_finite() && (1.0..=4.0).contains(&self.line_height),
            "typography.line_height must be finite and between 1 and 4"
        );
        ensure!(
            self.letter_spacing.is_finite() && (0.0..=32.0).contains(&self.letter_spacing),
            "typography.letter_spacing must be finite and between 0 and 32"
        );
        ensure!(
            self.fallback.len() <= 16,
            "typography.fallback exceeds 16 families"
        );
        for family in self.family.iter().chain(&self.fallback) {
            ensure!(
                !family.trim().is_empty()
                    && family.len() <= 256
                    && !family.chars().any(char::is_control),
                "typography family must be nonblank, at most 256 bytes, and contain no control characters"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_fractional_values_are_valid() {
        let defaults = TypographyConfiguration::default();
        assert_eq!(defaults.size, 16.0);
        assert_eq!(defaults.line_height, 1.0);
        assert_eq!(defaults.letter_spacing, 0.0);
        defaults.validate().unwrap();
        TypographyConfiguration {
            size: 15.25,
            family: Some("Mono".into()),
            fallback: vec!["Emoji".into()],
            line_height: 1.125,
            letter_spacing: 0.25,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn rejects_nonfinite_and_out_of_bounds_metrics() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0, 129.0] {
            assert!(
                TypographyConfiguration {
                    size: value,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        for value in [f32::NAN, f32::INFINITY, 0.0, 0.99, 4.01] {
            assert!(
                TypographyConfiguration {
                    line_height: value,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        for value in [f32::NAN, f32::INFINITY, -0.1, 32.01] {
            assert!(
                TypographyConfiguration {
                    letter_spacing: value,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn family_lookup_is_bounded() {
        for family in [String::new(), "  ".into(), "x\n".into(), "x".repeat(257)] {
            assert!(
                TypographyConfiguration {
                    family: Some(family),
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            TypographyConfiguration {
                fallback: vec!["Mono".into(); 17],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(toml::from_str::<TypographyConfiguration>("unknown = 1").is_err());
    }
}
