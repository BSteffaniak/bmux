use std::fmt;

use crate::{Oklab, Rgb};

/// Largest number of stops a gradient accepts.
pub const MAX_GRADIENT_STOPS: usize = 16;

/// Error returned when a gradient cannot be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientError {
    /// No stops were supplied.
    Empty,
    /// More than [`MAX_GRADIENT_STOPS`] stops were supplied.
    TooManyStops(usize),
}

impl fmt::Display for GradientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a gradient needs at least one stop"),
            Self::TooManyStops(count) => write!(
                f,
                "a gradient accepts at most {MAX_GRADIENT_STOPS} stops, found {count}"
            ),
        }
    }
}

impl std::error::Error for GradientError {}

/// Perceptually uniform gradient through evenly spaced color stops.
///
/// Interpolation happens in OKLab, so midpoints avoid the muddy, uneven
/// transitions of sRGB interpolation.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    stops: Vec<Oklab>,
}

impl Gradient {
    /// Create a gradient through `stops`, evenly spaced from `t = 0` to `t = 1`.
    ///
    /// # Errors
    ///
    /// * [`GradientError::Empty`] when `stops` is empty.
    /// * [`GradientError::TooManyStops`] when `stops` exceeds [`MAX_GRADIENT_STOPS`].
    pub fn new(stops: &[Rgb]) -> Result<Self, GradientError> {
        if stops.is_empty() {
            return Err(GradientError::Empty);
        }
        if stops.len() > MAX_GRADIENT_STOPS {
            return Err(GradientError::TooManyStops(stops.len()));
        }
        Ok(Self {
            stops: stops.iter().copied().map(Oklab::from_rgb).collect(),
        })
    }

    /// Sample the gradient at `t`, clamped to `[0, 1]`.
    #[must_use]
    pub fn sample(&self, t: f64) -> Rgb {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let last = self.stops.len() - 1;
        if last == 0 {
            return self.stops[0].to_rgb();
        }
        #[allow(clippy::cast_precision_loss)] // Stop counts are bounded by MAX_GRADIENT_STOPS.
        let scaled = t * last as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Non-negative, < 16.
        let index = (scaled.floor() as usize).min(last - 1);
        #[allow(clippy::cast_precision_loss)] // Stop counts are bounded by MAX_GRADIENT_STOPS.
        let local = scaled - index as f64;
        self.stops[index]
            .lerp(self.stops[index + 1], local)
            .to_rgb()
    }

    /// Sample `count` evenly spaced colors including both endpoints.
    ///
    /// A count of one yields the start color.
    pub fn samples(&self, count: usize) -> impl Iterator<Item = Rgb> + '_ {
        let denominator = count.saturating_sub(1).max(1);
        (0..count).map(move |index| {
            #[allow(clippy::cast_precision_loss)] // Cell counts are far below 2^52.
            let t = index as f64 / denominator as f64;
            self.sample(t)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: Rgb = Rgb::new(0xff, 0x00, 0xaa);
    const TO: Rgb = Rgb::new(0x00, 0xff, 0xff);

    #[test]
    fn rejects_empty_and_oversized_stop_lists() {
        assert_eq!(Gradient::new(&[]), Err(GradientError::Empty));
        let many = vec![FROM; MAX_GRADIENT_STOPS + 1];
        assert_eq!(
            Gradient::new(&many),
            Err(GradientError::TooManyStops(MAX_GRADIENT_STOPS + 1))
        );
    }

    #[test]
    fn endpoints_and_clamping_return_stop_colors() {
        let gradient = Gradient::new(&[FROM, TO]).unwrap();
        assert_eq!(gradient.sample(0.0), FROM);
        assert_eq!(gradient.sample(1.0), TO);
        assert_eq!(gradient.sample(-3.0), FROM);
        assert_eq!(gradient.sample(9.0), TO);
        assert_eq!(gradient.sample(f64::NAN), FROM);
    }

    #[test]
    fn single_stop_is_constant() {
        let gradient = Gradient::new(&[FROM]).unwrap();
        assert!(gradient.samples(5).all(|color| color == FROM));
    }

    #[test]
    fn interior_stops_are_hit_exactly() {
        let mid = Rgb::new(0xff, 0xd7, 0x00);
        let gradient = Gradient::new(&[FROM, mid, TO]).unwrap();
        assert_eq!(gradient.sample(0.5), mid);
    }

    #[test]
    fn samples_include_both_endpoints_and_requested_count() {
        let gradient = Gradient::new(&[FROM, TO]).unwrap();
        let colors: Vec<_> = gradient.samples(7).collect();
        assert_eq!(colors.len(), 7);
        assert_eq!(colors.first(), Some(&FROM));
        assert_eq!(colors.last(), Some(&TO));
        assert_eq!(gradient.samples(0).count(), 0);
        assert_eq!(gradient.samples(1).collect::<Vec<_>>(), vec![FROM]);
    }

    #[test]
    fn lightness_progresses_monotonically_between_two_stops() {
        let gradient = Gradient::new(&[Rgb::BLACK, Rgb::WHITE]).unwrap();
        let lightness: Vec<_> = gradient.samples(12).map(|c| Oklab::from_rgb(c).l).collect();
        assert!(lightness.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
