//! Text style commands.

use std::fmt::Display;
use std::io::{self, Write};

use crate::command::Command;

/// A terminal color.
///
/// The sixteen named colors are written as their 256-color palette indices
/// (`5;n`), so their appearance follows the terminal's palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    /// The terminal's default color (`39` foreground, `49` background).
    Reset,
    /// Palette index 0.
    Black,
    /// Palette index 8.
    DarkGrey,
    /// Palette index 9.
    Red,
    /// Palette index 1.
    DarkRed,
    /// Palette index 10.
    Green,
    /// Palette index 2.
    DarkGreen,
    /// Palette index 11.
    Yellow,
    /// Palette index 3.
    DarkYellow,
    /// Palette index 12.
    Blue,
    /// Palette index 4.
    DarkBlue,
    /// Palette index 13.
    Magenta,
    /// Palette index 5.
    DarkMagenta,
    /// Palette index 14.
    Cyan,
    /// Palette index 6.
    DarkCyan,
    /// Palette index 15.
    White,
    /// Palette index 7.
    Grey,
    /// A 24-bit color.
    Rgb {
        /// Red component.
        r: u8,
        /// Green component.
        g: u8,
        /// Blue component.
        b: u8,
    },
    /// A 256-color palette index.
    AnsiValue(u8),
}

impl Color {
    /// Write the SGR parameters selecting this color, after the layer prefix.
    #[inline]
    fn write_sgr(self, out: &mut (impl Write + ?Sized), layer: Layer) -> io::Result<()> {
        let (reset, prefix): (&[u8], &[u8]) = match layer {
            Layer::Foreground => (b"\x1b[39m", b"\x1b[38;"),
            Layer::Background => (b"\x1b[49m", b"\x1b[48;"),
        };
        let index = match self {
            Self::Reset => return out.write_all(reset),
            Self::Rgb { r, g, b } => {
                out.write_all(prefix)?;
                out.write_all(b"2;")?;
                write_u8(out, r)?;
                out.write_all(b";")?;
                write_u8(out, g)?;
                out.write_all(b";")?;
                write_u8(out, b)?;
                return out.write_all(b"m");
            }
            Self::AnsiValue(index) => index,
            Self::Black => 0,
            Self::DarkRed => 1,
            Self::DarkGreen => 2,
            Self::DarkYellow => 3,
            Self::DarkBlue => 4,
            Self::DarkMagenta => 5,
            Self::DarkCyan => 6,
            Self::Grey => 7,
            Self::DarkGrey => 8,
            Self::Red => 9,
            Self::Green => 10,
            Self::Yellow => 11,
            Self::Blue => 12,
            Self::Magenta => 13,
            Self::Cyan => 14,
            Self::White => 15,
        };
        out.write_all(prefix)?;
        out.write_all(b"5;")?;
        write_u8(out, index)?;
        out.write_all(b"m")
    }
}

#[derive(Clone, Copy)]
enum Layer {
    Foreground,
    Background,
}

#[inline]
fn write_u8(out: &mut (impl Write + ?Sized), value: u8) -> io::Result<()> {
    let hundreds = value / 100;
    let tens = (value / 10) % 10;
    let ones = value % 10;
    if hundreds > 0 {
        out.write_all(&[b'0' + hundreds, b'0' + tens, b'0' + ones])
    } else if tens > 0 {
        out.write_all(&[b'0' + tens, b'0' + ones])
    } else {
        out.write_all(&[b'0' + ones])
    }
}

/// Set the foreground color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetForegroundColor(pub Color);

impl Command for SetForegroundColor {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        self.0.write_sgr(out, Layer::Foreground)
    }
}

/// Set the background color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetBackgroundColor(pub Color);

impl Command for SetBackgroundColor {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        self.0.write_sgr(out, Layer::Background)
    }
}

/// Reset all colors and attributes (`CSI 0m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetColor;

impl Command for ResetColor {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[0m")
    }
}

/// A text attribute selected through SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Attribute {
    /// Reset every attribute (`0`).
    Reset,
    /// Bold (`1`).
    Bold,
    /// Faint (`2`).
    Dim,
    /// Italic (`3`).
    Italic,
    /// Single underline (`4`).
    Underlined,
    /// Double underline (`4:2`).
    DoubleUnderlined,
    /// Curly underline (`4:3`).
    Undercurled,
    /// Dotted underline (`4:4`).
    Underdotted,
    /// Dashed underline (`4:5`).
    Underdashed,
    /// Slow blink (`5`).
    SlowBlink,
    /// Rapid blink (`6`).
    RapidBlink,
    /// Swap foreground and background (`7`).
    Reverse,
    /// Hidden text (`8`).
    Hidden,
    /// Strikethrough (`9`).
    CrossedOut,
    /// Fraktur (`20`).
    Fraktur,
    /// Bold off, or double underline on some terminals (`21`).
    NoBold,
    /// Neither bold nor faint (`22`).
    NormalIntensity,
    /// Italic off (`23`).
    NoItalic,
    /// Underline off (`24`).
    NoUnderline,
    /// Blink off (`25`).
    NoBlink,
    /// Reverse off (`27`).
    NoReverse,
    /// Hidden off (`28`).
    NoHidden,
    /// Strikethrough off (`29`).
    NotCrossedOut,
    /// Framed (`51`).
    Framed,
    /// Encircled (`52`).
    Encircled,
    /// Overlined (`53`).
    OverLined,
    /// Neither framed nor encircled (`54`).
    NotFramedOrEncircled,
    /// Overline off (`55`).
    NotOverLined,
}

impl Attribute {
    /// The SGR parameter selecting this attribute.
    #[must_use]
    pub const fn sgr(self) -> &'static str {
        match self {
            Self::Reset => "0",
            Self::Bold => "1",
            Self::Dim => "2",
            Self::Italic => "3",
            Self::Underlined => "4",
            Self::DoubleUnderlined => "4:2",
            Self::Undercurled => "4:3",
            Self::Underdotted => "4:4",
            Self::Underdashed => "4:5",
            Self::SlowBlink => "5",
            Self::RapidBlink => "6",
            Self::Reverse => "7",
            Self::Hidden => "8",
            Self::CrossedOut => "9",
            Self::Fraktur => "20",
            Self::NoBold => "21",
            Self::NormalIntensity => "22",
            Self::NoItalic => "23",
            Self::NoUnderline => "24",
            Self::NoBlink => "25",
            Self::NoReverse => "27",
            Self::NoHidden => "28",
            Self::NotCrossedOut => "29",
            Self::Framed => "51",
            Self::Encircled => "52",
            Self::OverLined => "53",
            Self::NotFramedOrEncircled => "54",
            Self::NotOverLined => "55",
        }
    }
}

/// Set one text attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetAttribute(pub Attribute);

impl Command for SetAttribute {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[")?;
        out.write_all(self.0.sgr().as_bytes())?;
        out.write_all(b"m")
    }
}

/// Print displayable content as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Print<T: Display>(pub T);

impl<T: Display> Command for Print<T> {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        write!(out, "{}", self.0)
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // Plain content never needs console API translation.
        true
    }
}

impl<T: Display> Display for Print<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue;

    fn written(command: impl Command) -> String {
        let mut out = Vec::new();
        queue!(out, command).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn named_colors_use_palette_indices() {
        let expected = [
            (Color::Black, 0),
            (Color::DarkRed, 1),
            (Color::DarkGreen, 2),
            (Color::DarkYellow, 3),
            (Color::DarkBlue, 4),
            (Color::DarkMagenta, 5),
            (Color::DarkCyan, 6),
            (Color::Grey, 7),
            (Color::DarkGrey, 8),
            (Color::Red, 9),
            (Color::Green, 10),
            (Color::Yellow, 11),
            (Color::Blue, 12),
            (Color::Magenta, 13),
            (Color::Cyan, 14),
            (Color::White, 15),
        ];
        for (color, index) in expected {
            assert_eq!(
                written(SetForegroundColor(color)),
                format!("\x1b[38;5;{index}m")
            );
            assert_eq!(
                written(SetBackgroundColor(color)),
                format!("\x1b[48;5;{index}m")
            );
        }
    }

    #[test]
    fn reset_rgb_and_indexed_colors() {
        assert_eq!(written(SetForegroundColor(Color::Reset)), "\x1b[39m");
        assert_eq!(written(SetBackgroundColor(Color::Reset)), "\x1b[49m");
        assert_eq!(
            written(SetForegroundColor(Color::Rgb { r: 0, g: 128, b: 255 })),
            "\x1b[38;2;0;128;255m"
        );
        for index in [0u8, 7, 42, 199, 255] {
            assert_eq!(
                written(SetBackgroundColor(Color::AnsiValue(index))),
                format!("\x1b[48;5;{index}m")
            );
        }
        assert_eq!(written(ResetColor), "\x1b[0m");
    }

    #[test]
    fn attributes_write_sgr_parameters() {
        assert_eq!(written(SetAttribute(Attribute::Reset)), "\x1b[0m");
        assert_eq!(written(SetAttribute(Attribute::Bold)), "\x1b[1m");
        assert_eq!(written(SetAttribute(Attribute::Undercurled)), "\x1b[4:3m");
        assert_eq!(written(SetAttribute(Attribute::NormalIntensity)), "\x1b[22m");
        assert_eq!(written(SetAttribute(Attribute::NotCrossedOut)), "\x1b[29m");
    }

    #[test]
    fn print_writes_display_output() {
        assert_eq!(written(Print("héllo")), "héllo");
        assert_eq!(written(Print('x')), "x");
        assert_eq!(written(Print(String::from("s"))), "s");
    }
}
