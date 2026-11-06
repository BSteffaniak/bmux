use std::fmt;
use std::str::FromStr;

/// Opaque 8-bit sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

/// Error returned when a string is not a valid hex color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseHexError {
    /// The value is not three or six hex digits after an optional `#`.
    InvalidLength(usize),
    /// The value contains a non-hexadecimal character.
    InvalidDigit(char),
}

impl fmt::Display for ParseHexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength(len) => {
                write!(f, "expected 3 or 6 hex digits, found {len}")
            }
            Self::InvalidDigit(digit) => write!(f, "invalid hex digit {digit:?}"),
        }
    }
}

impl std::error::Error for ParseHexError {}

impl Rgb {
    /// Pure black.
    pub const BLACK: Self = Self::new(0, 0, 0);
    /// Pure white.
    pub const WHITE: Self = Self::new(255, 255, 255);

    /// Create a color from 8-bit sRGB channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rgb`, `#rrggbb`, `rgb`, or `rrggbb` (case-insensitive).
    ///
    /// # Errors
    ///
    /// * [`ParseHexError::InvalidLength`] when the digit count is not 3 or 6.
    /// * [`ParseHexError::InvalidDigit`] when a character is not hexadecimal.
    pub fn from_hex(value: &str) -> Result<Self, ParseHexError> {
        let digits = value.strip_prefix('#').unwrap_or(value);
        let mut nibbles = [0_u8; 6];
        let mut count = 0;
        for character in digits.chars() {
            let nibble = character
                .to_digit(16)
                .ok_or(ParseHexError::InvalidDigit(character))?;
            if count < nibbles.len() {
                #[allow(clippy::cast_possible_truncation)] // A hex digit is at most 15.
                {
                    nibbles[count] = nibble as u8;
                }
            }
            count += 1;
        }
        match count {
            3 => Ok(Self::new(nibbles[0] * 17, nibbles[1] * 17, nibbles[2] * 17)),
            6 => Ok(Self::new(
                nibbles[0] * 16 + nibbles[1],
                nibbles[2] * 16 + nibbles[3],
                nibbles[4] * 16 + nibbles[5],
            )),
            other => Err(ParseHexError::InvalidLength(other)),
        }
    }

    /// Format as lowercase `#rrggbb`.
    #[must_use]
    pub fn to_hex(self) -> String {
        self.to_string()
    }

    /// Return channels as normalized `[0, 1]` sRGB values.
    #[must_use]
    pub fn to_unit(self) -> [f64; 3] {
        [self.r, self.g, self.b].map(|channel| f64::from(channel) / 255.0)
    }

    /// Build a color from normalized sRGB channels, clamping to `[0, 1]`.
    #[must_use]
    pub fn from_unit(channels: [f64; 3]) -> Self {
        let [r, g, b] = channels.map(unit_to_u8);
        Self::new(r, g, b)
    }

    /// Return linear-light sRGB channels.
    #[must_use]
    pub fn to_linear(self) -> [f64; 3] {
        self.to_unit().map(srgb_to_linear)
    }

    /// Build a color from linear-light sRGB channels, clamping out-of-gamut values.
    #[must_use]
    pub fn from_linear(channels: [f64; 3]) -> Self {
        Self::from_unit(channels.map(linear_to_srgb))
    }

    /// WCAG relative luminance in `[0, 1]`.
    #[must_use]
    pub fn relative_luminance(self) -> f64 {
        let [r, g, b] = self.to_linear();
        0.0722_f64.mul_add(b, 0.2126_f64.mul_add(r, 0.7152 * g))
    }

    /// WCAG contrast ratio between two colors, in `[1, 21]`. Order does not matter.
    #[must_use]
    pub fn contrast_ratio(self, other: Self) -> f64 {
        let a = self.relative_luminance();
        let b = other.relative_luminance();
        let (light, dark) = if a >= b { (a, b) } else { (b, a) };
        (light + 0.05) / (dark + 0.05)
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl FromStr for Rgb {
    type Err = ParseHexError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_hex(value)
    }
}

impl From<(u8, u8, u8)> for Rgb {
    fn from((r, g, b): (u8, u8, u8)) -> Self {
        Self::new(r, g, b)
    }
}

impl From<[u8; 3]> for Rgb {
    fn from([r, g, b]: [u8; 3]) -> Self {
        Self::new(r, g, b)
    }
}

impl From<Rgb> for [u8; 3] {
    fn from(color: Rgb) -> Self {
        [color.r, color.g, color.b]
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Rgb;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a #rgb or #rrggbb hex color")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Rgb, E> {
                Rgb::from_hex(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

fn srgb_to_linear(channel: f64) -> f64 {
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(channel: f64) -> f64 {
    if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055_f64.mul_add(channel.powf(1.0 / 2.4), -0.055)
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to [0, 255] first.
fn unit_to_u8(channel: f64) -> u8 {
    if channel.is_nan() {
        return 0;
    }
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_long_and_unprefixed_hex() {
        assert_eq!(Rgb::from_hex("#7dd3fc"), Ok(Rgb::new(0x7d, 0xd3, 0xfc)));
        assert_eq!(Rgb::from_hex("7DD3FC"), Ok(Rgb::new(0x7d, 0xd3, 0xfc)));
        assert_eq!(Rgb::from_hex("#fa0"), Ok(Rgb::new(0xff, 0xaa, 0x00)));
    }

    #[test]
    fn rejects_bad_lengths_and_digits() {
        assert_eq!(
            Rgb::from_hex("#12345"),
            Err(ParseHexError::InvalidLength(5))
        );
        assert_eq!(Rgb::from_hex(""), Err(ParseHexError::InvalidLength(0)));
        assert_eq!(
            Rgb::from_hex("#1234567"),
            Err(ParseHexError::InvalidLength(7))
        );
        assert_eq!(
            Rgb::from_hex("#12g456"),
            Err(ParseHexError::InvalidDigit('g'))
        );
        assert_eq!(
            Rgb::from_hex("#１２３"),
            Err(ParseHexError::InvalidDigit('１'))
        );
    }

    #[test]
    fn hex_round_trips() {
        let color = Rgb::new(1, 128, 254);
        assert_eq!(color.to_hex(), "#0180fe");
        assert_eq!(color.to_hex().parse::<Rgb>(), Ok(color));
    }

    #[test]
    fn linear_round_trip_is_lossless_for_every_channel_value() {
        for value in 0..=255_u8 {
            let color = Rgb::new(value, value, value);
            assert_eq!(Rgb::from_linear(color.to_linear()), color);
        }
    }

    #[test]
    fn contrast_matches_wcag_reference_values() {
        assert!((Rgb::WHITE.contrast_ratio(Rgb::BLACK) - 21.0).abs() < 1e-9);
        assert!((Rgb::WHITE.contrast_ratio(Rgb::WHITE) - 1.0).abs() < 1e-9);
        // #767676 on white is the canonical minimum AA gray (~4.54:1).
        let gray = Rgb::new(0x76, 0x76, 0x76);
        let ratio = gray.contrast_ratio(Rgb::WHITE);
        assert!((4.5..4.6).contains(&ratio), "{ratio}");
        assert!((ratio - Rgb::WHITE.contrast_ratio(gray)).abs() < 1e-12);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_hex_strings() {
        let color = Rgb::new(0x0b, 0x10, 0x20);
        let json = serde_json::to_string(&color).unwrap();
        assert_eq!(json, "\"#0b1020\"");
        assert_eq!(serde_json::from_str::<Rgb>(&json).unwrap(), color);
        assert!(serde_json::from_str::<Rgb>("\"#xyz\"").is_err());
    }
}
