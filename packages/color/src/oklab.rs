use crate::Rgb;

/// Tolerance for treating a linear sRGB channel as in gamut.
const GAMUT_EPSILON: f64 = 1e-6;
/// Bisection steps used when reducing chroma into gamut.
const GAMUT_STEPS: usize = 24;
/// Chroma below which a color is treated as achromatic (hue reported as 0).
const ACHROMATIC_CHROMA: f64 = 1e-6;

/// Color in the OKLab perceptual space.
///
/// `l` is perceived lightness in `[0, 1]`; `a` and `b` are opponent axes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Oklab {
    /// Perceived lightness.
    pub l: f64,
    /// Green–red axis.
    pub a: f64,
    /// Blue–yellow axis.
    pub b: f64,
}

/// Color in OKLCH, the cylindrical form of [`Oklab`].
///
/// `l` is perceived lightness in `[0, 1]`, `c` is chroma (`>= 0`), and `h` is
/// hue in degrees `[0, 360)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Oklch {
    /// Perceived lightness.
    pub l: f64,
    /// Chroma.
    pub c: f64,
    /// Hue in degrees.
    pub h: f64,
}

impl Oklab {
    /// Create an OKLab color.
    #[must_use]
    pub const fn new(l: f64, a: f64, b: f64) -> Self {
        Self { l, a, b }
    }

    /// Convert from 8-bit sRGB.
    #[must_use]
    #[allow(clippy::suboptimal_flops)] // Published matrix coefficients read clearer as written.
    pub fn from_rgb(color: Rgb) -> Self {
        let [red, green, blue] = color.to_linear();
        let long = 0.412_221_470_8 * red + 0.536_332_536_3 * green + 0.051_445_992_9 * blue;
        let medium = 0.211_903_498_2 * red + 0.680_699_545_1 * green + 0.107_396_956_6 * blue;
        let short = 0.088_302_461_9 * red + 0.281_718_837_6 * green + 0.629_978_700_5 * blue;
        let (long, medium, short) = (long.cbrt(), medium.cbrt(), short.cbrt());
        Self {
            l: 0.210_454_255_3 * long + 0.793_617_785_0 * medium - 0.004_072_046_8 * short,
            a: 1.977_998_495_1 * long - 2.428_592_205_0 * medium + 0.450_593_709_9 * short,
            b: 0.025_904_037_1 * long + 0.782_771_766_2 * medium - 0.808_675_766_0 * short,
        }
    }

    /// Convert to linear-light sRGB without clamping; channels may be out of `[0, 1]`.
    #[must_use]
    #[allow(clippy::suboptimal_flops)] // Published matrix coefficients read clearer as written.
    pub fn to_linear_srgb(self) -> [f64; 3] {
        let long = self.l + 0.396_337_777_4 * self.a + 0.215_803_757_3 * self.b;
        let medium = self.l - 0.105_561_345_8 * self.a - 0.063_854_172_8 * self.b;
        let short = self.l - 0.089_484_177_5 * self.a - 1.291_485_548_0 * self.b;
        let (long, medium, short) = (long.powi(3), medium.powi(3), short.powi(3));
        [
            4.076_741_662_1 * long - 3.307_711_591_3 * medium + 0.230_969_929_2 * short,
            -1.268_438_004_6 * long + 2.609_757_401_1 * medium - 0.341_319_396_5 * short,
            -0.004_196_086_3 * long - 0.703_418_614_7 * medium + 1.707_614_701_0 * short,
        ]
    }

    /// Return whether this color is representable in sRGB.
    #[must_use]
    pub fn in_srgb_gamut(self) -> bool {
        self.to_linear_srgb()
            .iter()
            .all(|channel| (-GAMUT_EPSILON..=1.0 + GAMUT_EPSILON).contains(channel))
    }

    /// Convert to sRGB, reducing chroma (preserving lightness and hue) when out of gamut.
    #[must_use]
    pub fn to_rgb(self) -> Rgb {
        Oklch::from(self).to_rgb()
    }

    /// Squared Euclidean distance, a perceptual color difference.
    #[must_use]
    pub fn distance_squared(self, other: Self) -> f64 {
        let (dl, da, db) = (self.l - other.l, self.a - other.a, self.b - other.b);
        db.mul_add(db, dl.mul_add(dl, da * da))
    }

    /// Interpolate linearly toward `other`; `t = 0` is `self`, `t = 1` is `other`.
    #[must_use]
    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self {
            l: (other.l - self.l).mul_add(t, self.l),
            a: (other.a - self.a).mul_add(t, self.a),
            b: (other.b - self.b).mul_add(t, self.b),
        }
    }
}

impl Oklch {
    /// Create an OKLCH color.
    #[must_use]
    pub const fn new(l: f64, c: f64, h: f64) -> Self {
        Self { l, c, h }
    }

    /// Convert from 8-bit sRGB.
    #[must_use]
    pub fn from_rgb(color: Rgb) -> Self {
        Oklab::from_rgb(color).into()
    }

    /// Convert to sRGB, reducing chroma (preserving lightness and hue) when out of gamut.
    #[must_use]
    pub fn to_rgb(self) -> Rgb {
        let l = self.l.clamp(0.0, 1.0);
        let c = self.c.max(0.0);
        let candidate = Oklab::from(Self { l, c, h: self.h });
        if candidate.in_srgb_gamut() {
            return Rgb::from_linear(candidate.to_linear_srgb());
        }
        let (mut low, mut high) = (0.0, c);
        for _ in 0..GAMUT_STEPS {
            let mid = f64::midpoint(low, high);
            if Oklab::from(Self {
                l,
                c: mid,
                h: self.h,
            })
            .in_srgb_gamut()
            {
                low = mid;
            } else {
                high = mid;
            }
        }
        Rgb::from_linear(
            Oklab::from(Self {
                l,
                c: low,
                h: self.h,
            })
            .to_linear_srgb(),
        )
    }
}

impl From<Oklab> for Oklch {
    fn from(color: Oklab) -> Self {
        let c = color.a.hypot(color.b);
        let h = if c < ACHROMATIC_CHROMA {
            0.0
        } else {
            color.b.atan2(color.a).to_degrees().rem_euclid(360.0)
        };
        Self { l: color.l, c, h }
    }
}

impl From<Oklch> for Oklab {
    fn from(color: Oklch) -> Self {
        let (sin, cos) = color.h.to_radians().sin_cos();
        Self {
            l: color.l,
            a: color.c * cos,
            b: color.c * sin,
        }
    }
}

impl From<Rgb> for Oklab {
    fn from(color: Rgb) -> Self {
        Self::from_rgb(color)
    }
}

impl From<Rgb> for Oklch {
    fn from(color: Rgb) -> Self {
        Self::from_rgb(color)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black_have_reference_lightness() {
        let white = Oklab::from_rgb(Rgb::WHITE);
        assert!((white.l - 1.0).abs() < 1e-4);
        assert!(white.a.abs() < 1e-4 && white.b.abs() < 1e-4);
        assert!(Oklab::from_rgb(Rgb::BLACK).l.abs() < 1e-9);
    }

    #[test]
    fn red_matches_reference_oklab() {
        // Reference values from the OKLab specification for sRGB red.
        let red = Oklab::from_rgb(Rgb::new(255, 0, 0));
        assert!((red.l - 0.627_955).abs() < 1e-4);
        assert!((red.a - 0.224_863).abs() < 1e-4);
        assert!((red.b - 0.125_846).abs() < 1e-4);
    }

    #[test]
    fn rgb_round_trips_through_oklab_and_oklch() {
        for r in (0..=255_u8).step_by(15) {
            for g in (0..=255_u8).step_by(17) {
                for b in (0..=255_u8).step_by(51) {
                    let color = Rgb::new(r, g, b);
                    assert_eq!(Oklab::from_rgb(color).to_rgb(), color);
                    assert_eq!(Oklch::from_rgb(color).to_rgb(), color);
                }
            }
        }
    }

    #[test]
    fn out_of_gamut_chroma_is_reduced_preserving_lightness() {
        let wild = Oklch::new(0.7, 0.9, 140.0);
        assert!(!Oklab::from(wild).in_srgb_gamut());
        let mapped = Oklch::from_rgb(wild.to_rgb());
        assert!((mapped.l - 0.7).abs() < 0.01, "{mapped:?}");
        assert!(mapped.c < 0.9);
        assert!((mapped.h - 140.0).abs() < 2.0, "{mapped:?}");
    }

    #[test]
    fn achromatic_colors_have_zero_hue() {
        let gray = Oklch::from_rgb(Rgb::new(128, 128, 128));
        assert!(gray.c < 1e-4);
        assert!(gray.h.abs() < f64::EPSILON);
    }
}
