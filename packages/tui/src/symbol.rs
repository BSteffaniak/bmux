//! The text of one cell, stored inside the cell when it is short.
//!
//! Frames compare, copy and move every cell, so a cell's text must not cost a pointer chase or
//! an allocation: graphemes of up to [`Symbol::INLINE`] bytes (all but the longest emoji
//! sequences) are stored inline, longer ones on the heap.

use std::borrow::Borrow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;

/// One grapheme cluster: what a cell shows.
#[derive(Clone)]
pub struct Symbol(Repr);

#[derive(Clone)]
enum Repr {
    /// `bytes[..len]` is the text; the rest is zero, so equal texts have equal arrays.
    Inline {
        len: u8,
        bytes: [u8; Symbol::INLINE],
    },
    Heap(Box<str>),
}

impl Symbol {
    /// Longest text stored inline, in bytes.
    pub const INLINE: usize = 22;

    /// A single space, a blank cell's text.
    pub const SPACE: Self = Self::ascii(b' ');

    /// The empty text of a wide grapheme's continuation.
    pub const EMPTY: Self = Self(Repr::Inline {
        len: 0,
        bytes: [0; Self::INLINE],
    });

    /// One ASCII character.
    ///
    /// # Panics
    /// Panics if `byte` is not ASCII.
    #[must_use]
    pub const fn ascii(byte: u8) -> Self {
        assert!(byte.is_ascii(), "Symbol::ascii takes an ASCII byte");
        let mut bytes = [0; Self::INLINE];
        bytes[0] = byte;
        Self(Repr::Inline { len: 1, bytes })
    }

    /// `text` as a symbol.
    #[must_use]
    pub fn new(text: &str) -> Self {
        if text.len() <= Self::INLINE {
            let mut bytes = [0; Self::INLINE];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self(Repr::Inline {
                len: u8::try_from(text.len()).unwrap_or(0),
                bytes,
            })
        } else {
            Self(Repr::Heap(text.into()))
        }
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Inline { len, bytes } => {
                let bytes = &bytes[..usize::from(*len)];
                debug_assert!(std::str::from_utf8(bytes).is_ok());
                // SAFETY: inline bytes are only written by `new` (a whole `&str`) and `ascii` (one
                // ASCII byte), so the first `len` bytes are always valid UTF-8.
                unsafe { std::str::from_utf8_unchecked(bytes) }
            }
            Repr::Heap(text) => text,
        }
    }

    /// Replace the text with `text`, keeping the heap allocation when both are long.
    pub fn set(&mut self, text: &str) {
        if let Repr::Heap(current) = &self.0
            && text.len() > Self::INLINE
            && **current == *text
        {
            return;
        }
        *self = Self::new(text);
    }
}

impl Default for Symbol {
    fn default() -> Self {
        Self::SPACE
    }
}

impl Deref for Symbol {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Symbol {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for Symbol {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (
                Repr::Inline { len, bytes },
                Repr::Inline {
                    len: other_len,
                    bytes: other_bytes,
                },
            ) => len == other_len && bytes == other_bytes,
            _ => self.as_str() == other.as_str(),
        }
    }
}

impl Eq for Symbol {}

impl PartialEq<str> for Symbol {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Symbol {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Symbol {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<Symbol> for str {
    fn eq(&self, other: &Symbol) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<Symbol> for &str {
    fn eq(&self, other: &Symbol) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<Symbol> for String {
    fn eq(&self, other: &Symbol) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for Symbol {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Symbol {
    fn from(text: String) -> Self {
        if text.len() <= Self::INLINE {
            Self::new(&text)
        } else {
            Self(Repr::Heap(text.into_boxed_str()))
        }
    }
}

impl From<char> for Symbol {
    fn from(ch: char) -> Self {
        Self::new(ch.encode_utf8(&mut [0; 4]))
    }
}

impl From<&Symbol> for String {
    fn from(symbol: &Symbol) -> Self {
        symbol.as_str().to_owned()
    }
}

impl From<Symbol> for String {
    fn from(symbol: Symbol) -> Self {
        match symbol.0 {
            Repr::Heap(text) => text.into_string(),
            Repr::Inline { .. } => symbol.as_str().to_owned(),
        }
    }
}

impl FromIterator<Symbol> for String {
    fn from_iter<I: IntoIterator<Item = Symbol>>(symbols: I) -> Self {
        let mut text = Self::new();
        text.extend(symbols);
        text
    }
}

impl<'symbol> FromIterator<&'symbol Symbol> for String {
    fn from_iter<I: IntoIterator<Item = &'symbol Symbol>>(symbols: I) -> Self {
        let mut text = Self::new();
        text.extend(symbols);
        text
    }
}

impl Extend<Symbol> for String {
    fn extend<I: IntoIterator<Item = Symbol>>(&mut self, symbols: I) {
        for symbol in symbols {
            self.push_str(&symbol);
        }
    }
}

impl<'symbol> Extend<&'symbol Symbol> for String {
    fn extend<I: IntoIterator<Item = &'symbol Symbol>>(&mut self, symbols: I) {
        for symbol in symbols {
            self.push_str(symbol);
        }
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Symbol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::Symbol;

    #[test]
    fn short_and_long_symbols_keep_their_text() {
        for text in [
            "",
            " ",
            "a",
            "\u{754c}",
            "e\u{301}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
        ] {
            let symbol = Symbol::new(text);
            assert_eq!(symbol.as_str(), text);
            assert_eq!(symbol, text);
            assert_eq!(symbol, Symbol::from(text.to_owned()));
        }
    }

    #[test]
    fn equal_texts_are_equal_symbols_however_they_were_made() {
        let mut symbol = Symbol::new("a long grapheme that is not inline at all");
        symbol.set("x");
        assert_eq!(symbol, Symbol::ascii(b'x'));
        assert_eq!(Symbol::from('x'), Symbol::new("x"));
        assert_ne!(Symbol::new("x"), Symbol::new("xy"));
        assert_eq!(Symbol::SPACE, " ");
        assert_eq!(Symbol::EMPTY, "");
    }
}
