//! Derived-color operations.
//!
//! Amounts are fractions clamped to `[0, 1]`. Mixing happens in OKLab and
//! lightness/chroma adjustments in OKLCH, so results stay perceptually even and
//! hue-stable. Every result is mapped back into the sRGB gamut.

use crate::{MAX_CONTRAST_RATIO, Oklab, Oklch, Rgb};

/// Bisection steps used when searching for a contrast-satisfying lightness.
const CONTRAST_STEPS: usize = 32;

/// Mix two colors, where `weight` is the share of `a` (`1.0` returns `a`, `0.0` returns `b`).
#[must_use]
pub fn mix(a: Rgb, b: Rgb, weight: f64) -> Rgb {
    let weight = unit(weight);
    Oklab::from_rgb(b).lerp(Oklab::from_rgb(a), weight).to_rgb()
}

/// Move lightness toward white by `amount` of the remaining distance.
#[must_use]
pub fn lighten(color: Rgb, amount: f64) -> Rgb {
    let mut lch = Oklch::from_rgb(color);
    lch.l = (1.0 - lch.l).mul_add(unit(amount), lch.l);
    lch.to_rgb()
}

/// Move lightness toward black by `amount` of the remaining distance.
#[must_use]
pub fn darken(color: Rgb, amount: f64) -> Rgb {
    let mut lch = Oklch::from_rgb(color);
    lch.l *= 1.0 - unit(amount);
    lch.to_rgb()
}

/// Increase chroma by `amount` of its current value (gamut permitting).
#[must_use]
pub fn saturate(color: Rgb, amount: f64) -> Rgb {
    let mut lch = Oklch::from_rgb(color);
    lch.c *= 1.0 + unit(amount);
    lch.to_rgb()
}

/// Decrease chroma by `amount` of its current value; `1.0` yields a gray.
#[must_use]
pub fn desaturate(color: Rgb, amount: f64) -> Rgb {
    let mut lch = Oklch::from_rgb(color);
    lch.c *= 1.0 - unit(amount);
    lch.to_rgb()
}

/// Return `foreground`, adjusted in lightness only as much as needed to reach
/// `ratio` contrast against `background`.
///
/// Hue is preserved and chroma is reduced only when gamut requires it. The
/// lightness moves toward whichever extreme (black or white) offers more
/// contrast against `background`. When even that extreme cannot reach `ratio`,
/// the extreme is returned. `ratio` is clamped to `[1, 21]`.
#[must_use]
pub fn ensure_contrast(foreground: Rgb, background: Rgb, ratio: f64) -> Rgb {
    let ratio = if ratio.is_nan() {
        1.0
    } else {
        ratio.clamp(1.0, MAX_CONTRAST_RATIO)
    };
    if foreground.contrast_ratio(background) >= ratio {
        return foreground;
    }
    let toward_white =
        Rgb::WHITE.contrast_ratio(background) >= Rgb::BLACK.contrast_ratio(background);
    let (extreme_l, extreme) = if toward_white {
        (1.0, Rgb::WHITE)
    } else {
        (0.0, Rgb::BLACK)
    };
    if extreme.contrast_ratio(background) < ratio {
        return extreme;
    }
    let lch = Oklch::from_rgb(foreground);
    let at = |l: f64| Oklch { l, ..lch }.to_rgb();
    // Invariant: `near` fails the ratio and `far` satisfies it.
    let (mut near, mut far) = (lch.l, extreme_l);
    if at(far).contrast_ratio(background) < ratio {
        return extreme;
    }
    for _ in 0..CONTRAST_STEPS {
        let mid = f64::midpoint(near, far);
        if at(mid).contrast_ratio(background) >= ratio {
            far = mid;
        } else {
            near = mid;
        }
    }
    at(far)
}

const fn unit(amount: f64) -> f64 {
    if amount.is_nan() {
        0.0
    } else {
        amount.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCENT: Rgb = Rgb::new(0x7d, 0xd3, 0xfc);
    const BG: Rgb = Rgb::new(0x0b, 0x10, 0x20);

    #[test]
    fn mix_endpoints_return_inputs() {
        assert_eq!(mix(ACCENT, BG, 1.0), ACCENT);
        assert_eq!(mix(ACCENT, BG, 0.0), BG);
        assert_eq!(mix(ACCENT, BG, 7.0), ACCENT);
        assert_eq!(mix(ACCENT, BG, f64::NAN), BG);
    }

    #[test]
    fn mix_is_perceptually_between_inputs() {
        let tint = mix(ACCENT, BG, 0.2);
        let l = Oklab::from_rgb(tint).l;
        assert!(Oklab::from_rgb(BG).l < l && l < Oklab::from_rgb(ACCENT).l);
        let half = Oklab::from_rgb(mix(Rgb::WHITE, Rgb::BLACK, 0.5)).l;
        assert!((half - 0.5).abs() < 0.01, "{half}");
    }

    #[test]
    fn lighten_and_darken_move_lightness_monotonically_and_keep_hue() {
        let base = Oklch::from_rgb(ACCENT);
        let lighter = Oklch::from_rgb(lighten(ACCENT, 0.5));
        let darker = Oklch::from_rgb(darken(ACCENT, 0.5));
        assert!(lighter.l > base.l && darker.l < base.l);
        assert!((darker.h - base.h).abs() < 2.0);
        assert_eq!(lighten(ACCENT, 1.0), Rgb::WHITE);
        assert_eq!(darken(ACCENT, 1.0), Rgb::BLACK);
        assert_eq!(lighten(ACCENT, 0.0), ACCENT);
    }

    #[test]
    fn desaturate_fully_yields_gray_and_saturate_increases_chroma() {
        let gray = desaturate(ACCENT, 1.0);
        assert!(Oklch::from_rgb(gray).c < 0.002);
        let muted = Rgb::new(0x80, 0x90, 0xa0);
        assert!(Oklch::from_rgb(saturate(muted, 0.5)).c > Oklch::from_rgb(muted).c);
    }

    #[test]
    fn ensure_contrast_leaves_passing_colors_unchanged() {
        assert_eq!(ensure_contrast(ACCENT, BG, 4.5), ACCENT);
    }

    #[test]
    fn ensure_contrast_reaches_ratio_with_minimal_change() {
        let dim = Rgb::new(0x33, 0x41, 0x55);
        for ratio in [3.0, 4.5, 7.0] {
            let fixed = ensure_contrast(dim, BG, ratio);
            let achieved = fixed.contrast_ratio(BG);
            assert!(achieved >= ratio, "{ratio}: {achieved}");
            assert!(achieved < ratio + 0.25, "overshot {ratio}: {achieved}");
            let (before, after) = (Oklch::from_rgb(dim), Oklch::from_rgb(fixed));
            assert!(after.l > before.l, "dark background must lighten");
            assert!((after.h - before.h).abs() < 3.0);
        }
    }

    #[test]
    fn ensure_contrast_darkens_on_light_backgrounds() {
        let light_bg = Rgb::new(0xf8, 0xfa, 0xfc);
        let pale = Rgb::new(0xbb, 0xf7, 0xd0);
        let fixed = ensure_contrast(pale, light_bg, 4.5);
        assert!(fixed.contrast_ratio(light_bg) >= 4.5);
        assert!(Oklch::from_rgb(fixed).l < Oklch::from_rgb(pale).l);
    }

    #[test]
    fn ensure_contrast_returns_extreme_when_ratio_is_unreachable() {
        let mid = Rgb::new(0x50, 0x50, 0x50);
        assert_eq!(ensure_contrast(mid, mid, 21.0), Rgb::WHITE);
        assert_eq!(ensure_contrast(Rgb::BLACK, Rgb::WHITE, 99.0), Rgb::BLACK);
    }
}
