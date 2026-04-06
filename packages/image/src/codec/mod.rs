//! Protocol-specific image codecs.

pub mod base64;
#[cfg(feature = "iterm2")]
pub mod iterm2;
#[cfg(feature = "kitty")]
pub mod kitty;
#[cfg(feature = "sixel")]
pub mod sixel;
