//! Platform implementations of terminal modes and queries.

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::{
    disable_raw_mode, enable_raw_mode, is_raw_mode_enabled, size, supports_keyboard_enhancement,
    window_size,
};
#[cfg(windows)]
pub use windows::{
    disable_raw_mode, enable_raw_mode, is_raw_mode_enabled, size, supports_keyboard_enhancement,
    window_size,
};
